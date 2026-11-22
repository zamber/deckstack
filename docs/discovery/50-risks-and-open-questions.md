# Discovery 50 — risks and open questions

Prefixes: `hubdeck/` = `/var/home/catwars/repos/hubdeck`, `upstream/` =
`/var/home/catwars/repos/hubdeck-upstream`, `deckstack/` = this repo. Every risk names its
evidence and the experiment that resolves it. OPEN = not verified in the sources read.

## 1. Protocol / device risks

| # | Risk | Evidence | Experiment to resolve |
|---|---|---|---|
| P1 | **No device readback of what is displayed.** Writes have no ack; feature reads return saved config, not live state (upstream/PROTOCOL.md:35, 152, 219-220; hubdeck docs/masterhub-protocol.md:219-220). R5 demands tests capture what the device reports. | mh-device `Error` enum has no readback path beyond `Query` (`crates/mh-device/src/lib.rs:35-52`); both apps track "sent" vs "shown" host-side (`upstream/app/src-tauri/src/engine/api.rs:135-143` `GET /api/leds {shown, sent}`; Python `shown` map `hubdeck/src/hubdeck/engine.py:1319-1323`). | Camera/photodiode rig (already used: upstream PROTOCOL.md:70-87, 243-246 film tests, `examples/align.rs`, `counter.rs`) as golden-frame oracle; otherwise adopt the strongest proxy: frames-sent log + per-write byte counts + 45-48 ms feature-read liveness (PROTOCOL.md:141-143) + HID read-error as disconnect signal. Decide in the design run whether the proxy suffices for R5. |
| P2 | **Hardware-mode feature reads wedge keys/dial until reseat** (upstream/PROTOCOL.md:182-197). A daemon that crashes mid-read or a buggy read path bricks a module. | `Error::HardwareMode` guard exists (`crates/mh-device/src/module.rs:158-166`); Python has no equivalent guard on reads. | Virtual-hub test: assert every read goes through a software-mode check; hardware test: scripted read in hardware mode on a sacrificial module (already done 2026-10-09, PROTOCOL.md:184-190). |
| P3 | **Backlight 0 bricks the dial on macOS** until reseat (upstream/PROTOCOL.md:61). | Both drivers avoid 0 (upstream `mod.rs:3009`; Python uses black images). | Hardware: send `03 44 00` to a dial, observe; already observed — keep the rule "screens off = black JPEG", enforce in the protocol builder (no `0` arg), test on virtual device. |
| P4 | High LED write rates may drop the faders module off USB (two drops at ~50/s; stable at 10/s; ~25/s suspected limit) — "likely, not proven" (upstream/PROTOCOL.md:118-121). | `examples/lights.rs`; PROTOCOL.md:118-121. | Soak test: 10/25/50/100 colour writes/s for 1 h each on the faders module while pinging feature `0x2E`; log disconnects. Set the daemon's LED rate cap from the result. |
| P5 | Roller silence without SET_IDLE on macOS; Python notes 2xRoller silent on macOS with unknown cause (upstream/PROTOCOL.md:40; hubdeck docs/masterhub-protocol.md:359). | `crates/mh-device/src/recover.rs:15-18` sends SET_IDLE on macOS; `prototype/tools/hid_idle.py`. | On macOS: enumerate rollers, read input without and with SET_IDLE (`examples/rollers.rs --send`); confirm the fix covers all M0 modules. |
| P6 | USB dropouts from loose contacts (~0.5 s on press) and self-drops of the keys module (upstream/PROTOCOL.md:176-177; hubdeck docs/masterhub-protocol.md:191-192). | — | Reconnect path must be idempotent and re-push full state; test with virtual hub hot-unplug/replug cycles asserting the repaint report sequence. |
| P7 | Two same-kind modules mirror on keys/dial (upstream PROTOCOL.md:130-139, issue #7). Whether the base USB carries two panels at full rate is unknown. | PROTOCOL.md:130-139. | Hardware: two keys modules, send distinct panels, measure fps and camera-check both. |

## 2. OS / platform risks

| # | Risk | Evidence | Experiment |
|---|---|---|---|
| O1 | **Foreground watching is weakest on Linux, the primary target (R9).** Python: KWin-only D-Bus script, non-KDE `Unsupported` (`hubdeck/src/hubdeck/platforms/linux/focus.py:99-103`). Rust: 250 ms poll of `active-win-pos-rs` = X11 xcb + hyprland + kdotool, no native Wayland (upstream `engine/foreground.rs:157-195`, Cargo.lock deps). OPEN: pure-Wayland behaviour unverified. | As cited; also `loginctl`/`busctl` subprocesses for lock/idle (upstream `engine/idle.rs:29-97`). | On Bazzite (KDE, Wayland): run both watchers, log event latency vs `busctl`/`kdotool`; decide between wlr-foreign-toplevel, KWin script, or external watcher program calling the CLI (R4). |
| O2 | hidapi backends differ per OS: Linux needs `linux-static-libusb` (vendor HID gets no hidraw; udev rule needed) (`upstream/crates/mh-device/Cargo.toml:17-20`); macOS opens exclusively (hubdeck docs/masterhub-protocol.md:36-37); Windows exposes multiple paths per module (hubdeck `device/hid.py:5-8, 199-231`). | As cited. | CI matrix: enumerate/open/read/write each module kind per OS; record which backend and handle count. |
| O3 | Windows port needs more than named pipes: the working Windows specifics are WinEvent foreground hook, overlapped writes + second handle, CM_Register_Notification, pnputil restart, job objects, UIPI (upstream `foreground.rs:14-155`, `crates/mh-device/src/overlapped.rs`, `notify.rs`, `engine/elevated.rs`, `engine/plugins.rs:180-205`). Rust impl has **no named pipes at all** — all IPC is loopback TCP + files (grep-verified). | upstream `app/src-tauri/src` grep for `\\.\pipe`/`CreateNamedPipe`: none. | For the new base on Windows: named pipe (R7) + port the enumerated list; smoke-test on a Windows VM with the deck. |
| O4 | Linux audio/keyboard: Python audio `Unsupported` on Linux, keys via `/dev/uinput` (`hubdeck/src/hubdeck/platforms/linux/__init__.py:8-9`, `keyboard.py:35-177`); Rust same uinput route (upstream `engine/uinput.rs`, `exec.rs:256-281`). Needs `/dev/uinput` access (uinput group) — a permissions story the daemon must document. | As cited. | On Bazzite: `evtest` capture while sending keys via uinput; document group/udev rule. |

## 3. Security risks (R8 threat model inputs)

| # | Risk | Evidence | Mitigation to design / experiment |
|---|---|---|---|
| S1 | Local API/control port is unauthenticated beyond a token in a world-readable-ish file; Python writes `control.json` 0600 (`hubdeck/src/hubdeck/control.py:86-90`) but the port is random TCP — any local process that reads the file can mutate everything, and `hubdeck send action` can run `{launch.run: …}` = shell. | control.py:101-123 (`action` accepts any config action incl. `launch.run`); Rust API likewise allows `core.run` (`upstream engine/exec.rs`) but its assistant guard refuses it for the assistant role (`engine/guard.rs:12-21`). | New base: token with scopes (read vs mutate), per-install socket with 0600, no shell-action reachable without an explicit capability; test: unauthenticated/mutating-with-read-token requests must fail. |
| S2 | Config-as-code: Python `{python: code}` is `exec` in the daemon process (`hubdeck/src/hubdeck/plugins/python.py:40-60`). R8 forbids eval of config. | As cited; analysis D5 kept it (`hubdeck/analysis/decision-log.md:45`). | New base: config is data only; arbitrary behaviour = external programs calling the CLI (R4). Test: config containing code must be rejected by the schema. |
| S3 | Plugin sandboxing absent in Rust (`permissions` displayed only, upstream `engine/plugins.rs:58-59`); Python remote plugins are socketpair children with no restrictions (`plugins/host/remote.py`). A misbehaving plugin can fork-bomb or hold the deck. | As cited; R6 demands capability grants + resource budgets. | Design: process per plugin, capability token per plugin, rlimits (nproc/nofile/mem via setrlimit or systemd scope), watchdog kill; test: plugin that spawns 1000 children / loops forever must be killed and the daemon stays healthy. |
| S4 | Image/frame limits: Python caps downloads 10 MB, `hubdeck send image` 20 MB (`render/images.py:21`, `control.py:36`); Rust caps API body 16 MB and animations 120 frames/15 MB (`engine/plugins.rs:266`, `api.rs`). Unbounded frames = memory DoS on the daemon. | As cited. | Bounded queues + max frame size/count in the daemon; fuzz: send max-size frames at max rate, watch memory (virtual device). |
| S5 | Path handling: Python resolves images relative to config dir and GUI only edits `.yaml` under the config folder (`hubdeck/src/hubdeck/gui/server.py:428-436`); `launch.run` is `shell=True` (`platforms/posix.py:58-64`). Command injection through shellouts is real. | As cited. | New base: no shell by default (exec argv), plugin/file paths confined to declared roots; test: path traversal (`../../etc`) and shell metachars in action args must be rejected or inert. |
| S6 | DNS rebinding on the HTTP GUI: mitigated by Host check (hubdeck `gui/server.py:556-566`); Rust relies on bearer token. | As cited. | Keep both checks on any HTTP surface; test with `Host: evil.example`. |

## 4. Performance risks (R2/R5/R11)

| # | Risk | Evidence | Experiment |
|---|---|---|---|
| F1 | Control path starving under frame load: Python paints synchronously on the engine thread — a slow write blocks everything (`hubdeck/src/hubdeck/engine.py:465-514`, `_paint` `:1325-1341`). Rust already separates `keys pictures`/`dial pictures` threads with latest-frame-wins (`upstream engine/mod.rs:1313-1335, 1475-1535`). | As cited. | Benchmark harness (R2/R11): stream panel frames at 32 fps while issuing state queries; assert p99 query latency budget; reproduce the Python stall on a throttled virtual HID to prove the harness detects it. |
| F2 | Device ceilings: ~24 fps displayed / ~32 fps accepted / ~2.1 MB/s USB; pipelined writes 8-in-flight are 3-5× faster than one-at-a-time (upstream/PROTOCOL.md:91-97, 243-266). | As cited + `examples/framerate.rs`, `pipeline.rs`, `pipeline_mac.rs`. | Re-run `framerate`/`pipeline` examples against the new device layer; publish per-module fps table. |
| F3 | Per-write costs on shared handles: 64-byte LED write 2.7 ms own handle vs 34.8 ms shared (reader's 20 ms slice); SET_REPORT hangs 5 s on the base (upstream/PROTOCOL.md:116-118). | `crates/mh-device/tests/output_timing.rs`. | New layer: dedicated writer handle per module where the OS allows (Windows/macOS patterns in `overlapped.rs`/`pictures.rs`); measure on Linux hidraw. |
| F4 | Query latency while streaming (R2) unmeasured in both. | No benchmarks exist in either repo (grep: none). | Define p50/p99 budget in the design run; harness = virtual device + synthetic frame load. |

## 5. Testability risks (R10)

| # | Risk | Evidence | Experiment |
|---|---|---|---|
| T1 | Python's virtual hub asserts on protocol-level writes (`hubdeck/tests/fakes/virtual_hub.py:17-130, 196-246`); Rust has no equivalent (unit tests + hardware examples only; `HUBDECK_AUTOMATION` is a second full copy, `upstream/src/automation.rs:4-45`). OPEN: confirmed by reading, but no write-asserting harness found in app tests. | As cited. | Port the virtual-hub idea: a fake HID transport injected at the mh-device boundary; CI runs the whole acceptance suite against it (no hardware). |
| T2 | Hardware-vs-virtual drift: CLAUDE.md mandates "if the real device disagrees with the virtual one, fix the model first" (hubdeck/CLAUDE.md:103). | As cited. | Keep golden frames + the camera oracle for pixel-level facts (key window table, PROTOCOL.md:81). |
| T3 | **Video-streaming test question (explicit):** can the device report what it displays? **No** — no readback path exists (see P1); input reports carry only control events (keys/knobs/faders/rollers/dial/orientation), never screen content. Strongest proxy: (a) host-side log of frames sent (bytes, chunk count, timestamps, per-write return counts), (b) feature-read liveness + read errors as disconnect signal, (c) orientation/fader readbacks as the only device-originated state, (d) optional camera oracle for golden tests. Experiment that would settle it: full USBPcap capture of the official app during video playback, grepping for any screen-content report — none is known today (methodology at hubdeck docs/masterhub-protocol.md:291-311). | upstream/PROTOCOL.md:19-23 (report inventory), :141-167 (read table); hubdeck docs/protocol/README.md:31-36. | USBPcap capture + descriptor diff during panel video; if any screen-feedback report exists, add it to the read model. |

## 6. Prior-analysis contradictions (what the new brief supersedes)

`hubdeck/analysis/decision-log.md` D1 (lines 11-16) and `analysis/rewrite-vs-refactor.md:36, 45-46`
chose **"refactor the product layer, keep the Python device/protocol core"**; D2 keeps Python for
the core; D5 keeps python-in-yaml; D6 moves config to JSON authored by a UI; Q2 assumes a plain-web
UI. `deckstack/BRIEF.md:124-126` explicitly supersedes this: a **ground-up CLI-first base** in a new
repo (R12), no `eval` of config (R8, kills D5), no UI requirement for any capability (R1, kills the
UI-authored-config assumption), composable external event sources (R4), language per component open
(decision 2 — kills D2). What survives from the analysis: the "protocol + device layer + tests are
the moat" thesis (analysis/audit.md:65-83) — carried into this dossier as 40-hardware-and-protocol.md
and the virtual-hub requirement (R10).

## What this means for the new base

Constraints: unrepresentable forbidden commands (40-§8); software-mode + settle discipline and
never-brightness-0 (P2/P3); LED rate cap ≤30/s pending P4; pipelined writes + separate I/O threads
(F1-F3); virtual-device conformance harness before any hardware claim (T1/T3); token-scoped local
socket with no shell-eval paths (S1/S2/S5); Linux-first platform seam with an external watcher
program (O1/R4).
