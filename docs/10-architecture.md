# 10 — Architecture

Process model, planes, data flow, ownership, hot-plug/reconnect, crash recovery.
Contracts referenced here are normative in `docs/contracts/*` and `schemas/*`.

## 1. Process model

```
┌─ user session ─────────────────────────────────────────────────────────────┐
│                                                                            │
│  ds (CLI, one-shot or `ds watch`)      ds-focus   ds-mcp   ds-web  agents   │
│     │  NDJSON over Unix socket          │          │        │               │
│     └──────────────┐  ┌─────────────────┘          │        │               │
│                    ▼  ▼ WS/HTTP 127.0.0.1 (opt-in)  │        │               │
│              ┌─────────────┐   ┌─────────────────────┘        │               │
│              │ ds-daemon   │◄──┘  (all clients are peers;     │               │
│              │  ┌────────┐ │     none is privileged)          │               │
│              │  │ state  │ │◄──────┐                            │               │
│              │  │ router │ │───────┼── bindings resolve to     │               │
│              │  │ frame  │─┼──┐    │    set/emit/plugin/exec   │               │
│              │  │ plane  │ │  │    │                          │               │
│              │  └────┬───┘ │  │    │   ┌──────────────┐     │               │
│              │  ds-device  │  │    └──►│ plugin proc  │◄────┘               │
│              │  (mh-device)│  │       │ (any language)│                     │
│              └────┬───────┘  │       └──────────────┘                     │
│                   │ HID (hidapi, libusb on Linux)                         │
│              ┌────┴──────────┐    ┌─────────────┐                         │
│              │ MasterHub base│◄───│ key/dial/   │                         │
│              │ + modules     │    │ fader/knob/ │                         │
│              └───────────────┘    │ roller      │                         │
│                                   └─────────────┘                         │
│  ds-vdev (virtual device, tests/CI only, same HID report protocol)        │
└────────────────────────────────────────────────────────────────────────────┘
```

One daemon. The CLI is stateless: each invocation connects, authenticates, issues one
request (or opens a subscription with `watch`), prints, exits (R1). Every client — CLI,
web UI, MCP bridge, focus sender — is an unprivileged peer speaking the same protocol;
"UI state is not privileged" (R7).

## 2. Control plane vs frame plane

Two channels, one socket family, deliberately different encodings (ADR-004/005):

| | Control plane | Frame plane |
|---|---|---|
| Encoding | NDJSON (one JSON object per line) | Binary: 16-byte header + JSON meta + payload |
| Content | state reads/writes, subscriptions, events, plugin messages, admin | encoded frames (JPEG) + frame metadata |
| Path | `/control` endpoint on the daemon socket | separate stream endpoint `…/stream.sock` |
| Latency class | p50 ≤ 2 ms, p99 ≤ 10 ms even under 32 fps frame load | throughput class: device caps at ~32 fps accepted / ~24 fps displayed / ~2.1 MB/s (`upstream/PROTOCOL.md:244,265`) |
| Locking | state lock (RWLock), never held across I/O | per-module queue mutex, never held across HID writes |
| Backpressure | slow subscriber: events dropped, counted (`dropped: n`) | latest-frame-wins: 1 frame in flight + 1 waiting per module (`upstream/app/src-tauri/src/engine/mod.rs:1303-1335`); the in-flight frame is never dropped mid-send (S16/M8) |

**Per-frame state churn: none (S1).** Pushing a frame updates no durable state and no `rev`;
stats are sampled ≤ 1 Hz into the non-durable `/telemetry/*` tree (state.md §0). At 32 fps
there are zero state transactions and zero `/controls` subscription events per frame.

**In-flight frames (S16/M8, verified against upstream):** the writer thread sends a picture
whole via `module.send_all` (`upstream/app/src-tauri/src/engine/mod.rs:1488-1530`) and only
ever replaces the *waiting* slot (`mod.rs:1303-1335`); abandoning an in-flight chunked
transfer would tear the image on the module, so no timeout/drop is added there. Control
independence is structural: writer threads share no lock with the state task and no lock is
held across HID I/O — a stuck frame transfer cannot starve queries or bindings.

Why: putting base64 frames into JSON state would inflate every query and serialize on the
state lock — the exact failure observed in the Python engine, where a slow HID write
blocks the whole engine loop (`hubdeck/src/hubdeck/engine.py:465-514, 1325-1341`).
Local socket throughput is orders above the 2.1 MB/s device cap, so bytes-over-socket is
not a video bottleneck (ADR-005).

## 3. Data flow

```
key press ──HID input report──► ds-device ──► router ──► binding lookup ──► steps:
                                                                      ├ set path value   → state write → rev++ → subscribers + repaint check
                                                                      ├ emit topic       → event bus → subscribers (incl. external senders)
                                                                      ├ plugin id.act    → plugin host → effects → state writes
                                                                      └ exec argv (cap)  → spawn, rlimits, timeout
focus change ──► ds-focus (external) ──ds emit app/focus {…}──► router ──► binding ──► ds set … (recolor)
state query ──► CLI ──► NDJSON get ──► daemon state lock ──► JSON reply (rev included)
frame push ──► CLI/producer ──► binary stream ──► per-module queue (latest-wins) ──► writer thread ──► chunked HID 02 36/35/45
```

Device writes are the single choke point: every screen/LED write funnels through
`ds-device`, which enforces the unrepresentable-forbidden-commands rule
(`upstream/crates/mh-protocol/src/safety.rs:8-32`) and the `/policy` rules (D18) before
any byte leaves the host.

## 4. Ownership boundaries

| Owner | Owns | Must never do |
|---|---|---|
| `ds-daemon` | the state document, all HID I/O, plugin processes, subscriptions, revisions | render UI, hardcode actions, eval config, spawn shells |
| `ds-device` (crate) | HID enumeration, software-mode handshake, chunking, forbidden-command refusal, hot-plug rescan, handback | know about state paths or bindings |
| `ds` (CLI) | argument parsing, one request, output formatting, exit codes | keep state between invocations; talk HID |
| plugin host (in daemon) | plugin lifecycle, capability enforcement, effect application | trust plugin output beyond schema validation |
| plugin process | its own actions | touch daemon memory, HID, or state directly — only via effects/requests |
| senders (ds-focus…) | detecting OS events | decide deck policy — they only `emit` |
| ds-vdev | impersonating HID reports in memory | run outside tests/CI |

## 5. Hot-plug / reconnect

- Rescan: event-driven where the OS gives notifications (Windows `CM_Register_Notification`),
  else poll ≤ 1 s busy / 15 s idle (`upstream/crates/mh-device/src/hub.rs:20-21,149-155`).
  Target: keys back ≤ 5 s after replug (measured upstream, `upstream/PROTOCOL.md:172-177`).
- Module identity = USB serial read at connect (`upstream/crates/mh-device/src/hub.rs:241-245`),
  fallback USB port path (`upstream/PROTOCOL.md:107`). State is keyed by serial, so a
  replugged module gets its `/devices/<serial>/…` subtree — and its last visuals — back.
- On disconnect: held keys get synthetic `up` (`hubdeck/src/hubdeck/engine.py:1427-1433`),
  `/devices/<serial>/connection` → `disconnected`, subscribers notified, frame queue drained.
- On reconnect: software-mode handshake with ≥ 600 ms settle, double command before first
  read (`upstream/crates/mh-device/src/module.rs:191-222`, `upstream/PROTOCOL.md:197`),
  then full state re-push (all frames + LEDs + bindings-affecting state). Reconnect path
  must be idempotent — virtual-hub hot-unplug/replug tests assert the exact report sequence
  (discovery/50 P6).
- Reads of screen-module features only in software mode — a hardware-mode feature read
  wedges the module until reseat (`upstream/PROTOCOL.md:182-197`; guard:
  `upstream/crates/mh-device/src/module.rs:158-166`).

## 6. Crash recovery

- Guard child process (`ds-daemon --guard` pattern, `upstream/app/src-tauri/src/guard.rs:15-149`):
  liveness via anonymous stdin pipe; on unclean exit it sends software-mode **off**
  (`03 3A 00` / `03 49 00`, handback to firmware, `upstream/PROTOCOL.md:213-216`) and
  restarts the daemon, max 3 restarts / 10 min, writes `last-crash.json`.
- Daemon persists state atomically (temp+rename, like `upstream/app/src-tauri/src/engine/profile.rs:882-909`)
  after every committed transaction batch (debounced 250 ms); on start it loads
  `state.json`, replays nothing else, re-pushes everything to reconnected modules.
- A wedged module on Windows is restarted via `pnputil /restart-device` (admin)
  (`upstream/PROTOCOL.md:192-197`); on Linux the rescan loop handles it.
- Vendor MasterHUB/MHDaemon must not hold the modules concurrently — startup check:
  if HID open fails with busy, `ds daemon status` says so explicitly
  (`hubdeck/CLAUDE.md:66`, `upstream/PROTOCOL.md:217-221`).

## 7. What lives where

| Thing | Lives | Durable? |
|---|---|---|
| State document (controls, bindings, plugins, settings, policy) | daemon memory + `~/.local/state/deckstack/state.json` | yes, atomic |
| Frame bytes | frame-plane queues only; never persisted | no (only telemetry stats + last frame sha per target, sampled ≤ 1 Hz, non-durable) |
| Device identity/serials | `/devices/<serial>` in state; re-derived at connect | re-derived |
| Tokens | `~/.local/state/deckstack/token` 0600 | yes, per-install |
| Plugin code + manifests | `~/.local/share/deckstack/plugins/<id>/` | yes, user-owned |
| Logs | `~/.local/state/deckstack/daemon.log` (rotating, 5×1 MB) | yes |
| Crash record | `last-crash.json` next to state | yes |
| Virtual device | test processes only | no |
