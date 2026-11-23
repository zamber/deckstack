# Breakdown: macOS port (TODO — documentation only, DS-073)

**Date-stamped 2026-11-23.** This document turns `docs/40-platforms.md` §3 into an
itemized, schedulable breakdown. It implements nothing and changes no code; every item
below is TODO until a future macOS phase is ticketed. The standing rule stays:
**do not start until Linux alpha is stable** (deps DS-020 e2e, DS-026 hardware smoke;
`docs/00-scope.md` Linux-only alpha, scope delta 1: "Windows, no macOS implementation in
alpha", `docs/00-scope.md:10,44-46`; `docs/30-testing.md:114-117` — no macOS CI gate in
alpha). macOS is **post-alpha, untested and not claimed** (OPEN O8,
`docs/open-questions.md:26`).

**Assumed contract versions this breakdown is written against:** `docs/contracts/ipc.md`
(protocol `1`, `crates/ds-ipc/src/lib.rs` `PROTOCOL_VERSION = 1`; schemas
`schemas/ipc-message.json`, `schemas/stream-frame.json` as of 2026-11-23),
`docs/contracts/state.md` + `schemas/state.json`, the `Transport` trait in
`crates/ds-device/src/transport.rs:36-56` (`send`, `query`, `recv`, `close`, all `&self`,
`Send + Sync`), and the `hid` module of `crates/ds-device/src/hid/` (`mod.rs`,
`enumerate.rs`, `reader.rs`). If those move, re-date this document (risk R-rot, §9).

**Evidence convention:** `upstream/…` and `hubdeck/…` citations are path:line evidence
recorded in `docs/discovery/`; items not verifiable today are marked **OPEN** (O8 = all of
macOS, `docs/40-platforms.md:107`).

## 1. IPC: Unix domain sockets carry over unchanged (`ds-ipc`)

macOS has native Unix domain sockets, so — unlike the Windows named-pipe port
(`docs/breakdowns/windows.md` §1) — the transport layer of `ds-ipc` is **reused, not
replaced**. The wire formats (NDJSON control, DSF1 binary frames, `DSctl\n` stream
control, `docs/contracts/ipc.md` §1.2/§2) are byte-identical; only path resolution and
permission bits change.

- **What exists:** `crates/ds-ipc/src/server.rs` `Server::bind` (line 202, binds
  `UnixListener` at line 224, dir 0700 / socket 0600), `accept_loop` (line 294, ≤ 64
  connections, `limits.rs`), the full `dispatch` state machine, and
  `crates/ds-ipc/src/paths.rs` (`default_dir` = `$XDG_RUNTIME_DIR/deckstack` fallback
  `~/.local/state/deckstack`, lines 12-22). `crates/ds-daemon/src/lifecycle.rs`
  `RunPaths` (line 30) lays out `daemon.sock`/`stream.sock`/`state.json`/`daemon.pid`/
  `token`/`tokens.json` under one data dir.
- **What must be built (small `cfg(target_os = "macos")` arms, not a rewrite):**
  - `crates/ds-ipc/src/paths.rs`: on macOS prefer
    `~/Library/Application Support/deckstack/` (or keep honoring `XDG_RUNTIME_DIR` when
    set, since it exists as a variable but has no system-assigned value on macOS) —
    decision to pin in the porting ticket; the socket dir stays user-only
    (`0700`/`0600` work unchanged on APFS).
  - `crates/ds-daemon/src/lifecycle.rs::RunPaths` / `resolve_data_dir` (lines 30, 62):
    the same macOS data-dir arm; `prepare_data_dir` (line 70) and the `0600` token
    handling (`ensure_token`, line 79) are POSIX and carry over as-is.
  - Signals: `signal_hook` SIGTERM/SIGINT and `Shutdown::install` (lifecycle.rs:184-228)
    work on macOS unchanged; no `SetConsoleCtrlHandler` analogue needed.
- **Risk:** low — the main trap is assuming an XDG runtime dir exists; on macOS it does
  not unless the user exports it, so the fallback path becomes the common case. File
  flags (`O_EVTONLY`, kqueue) differ but nothing in `ds-ipc` uses them.
- **Acceptance test:** full conformance suite (`tests/conformance`) on a macOS runner
  over the Unix socket with ds-vdev, byte-identical to the Linux run; second local user
  on the same Mac cannot connect (dir mode 0700 denies); `ds daemon start/stop` via
  SIGTERM clean.

## 2. Device transport: hidapi `IOHIDManager` backend behind the same `Transport` trait (`ds-device`)

- **What exists:** the trait (`crates/ds-device/src/transport.rs:36-56`) and the Linux
  implementation `crates/ds-device/src/hid/{mod,enumerate,reader}.rs`: enumeration keys
  modules by VID `0x2516` + per-kind PIDs (`crates/ds-device/src/module.rs`), each open
  module gets one `hidapi::HidDevice` behind a `Mutex` (`reader.rs:33-42
  `ModuleHandle`), one reader thread per module looping `read_timeout` in 20 ms slices
  (`reader.rs:25` `READ_SLICE`, `reader.rs:79`), a shared `Inbox`/`Condvar` event queue
  (`mod.rs` `Shared`), software-mode tracking (`track_mode`), and the forbidden-report
  guard + padding in `prepare_write` (`crates/ds-device/src/report.rs`
  `ensure_allowed`/`pad_report`). The pinned `mh-device` selects hidapi's
  `linux-static-libusb` backend (`upstream/crates/mh-device/Cargo.toml:17-20`,
  `crates/ds-device/src/hid/mod.rs:20-26`).
- **What must be built:** a `cfg(target_os = "macos")` sibling of `hid/mod.rs` —
  `MacHidTransport` implementing the same `Transport` trait — reusing `enumerate`'s
  VID/PID logic, `report::{ensure_allowed, pad_report}`, `handshake`, and the shared
  `Inbox` event model unchanged. hidapi's **macOS backend is `IOHIDManager`**
  (`hidapi` crate `macos` feature; the pinned `mh-device` would select it per platform).
  Specifics:
  - **Run loop vs reader thread:** `IOHIDManager` is callback-driven and needs a
    `CFRunLoop` on the thread that registered it. Either (a) dedicate one thread per
    manager running `CFRunLoopRun()`, with input reports delivered through
    `IOHIDDeviceRegisterInputReportCallback` and marshalled into the existing `Inbox` —
    keeping the `recv()` shape of `Transport` unchanged, or (b) keep the polling
    `read_timeout` model, which hidapi-on-macOS also supports via its internal run-loop
    thread. Option (a) is the native shape; option (b) is the smaller diff. **Pick one
    in the porting ticket and pin it**; do not mix. Either way the per-module
    `Mutex<HidDevice>` + `send`/`query` interleaving model of `reader.rs` is preserved so
    the control plane never blocks on a read slice.
  - **Exclusive opens + busy error (OPEN O8):** on macOS only one program may hold a
    HID device (`kIOReturnExclusiveAccess`); if the vendor `MasterHUB` app (or another
    deckstack instance) holds the deck, `hid_open` fails. The transport must map this to
    a **clear busy error** surfaced in `ds daemon status` (not a generic `E_DEVICE`),
    per `hubdeck/docs/masterhub-protocol.md:36-37` and `docs/open-questions.md:26`.
  - **SET_IDLE at connect (rollers):** upstream sends SET_IDLE at connect on macOS so
    roller modules start speaking (`upstream/crates/mh-device/src/recover.rs:15-18`;
    `upstream/PROTOCOL.md:40`; `docs/components/device-driver.md` invariant 5). The macOS
    transport must issue SET_IDLE right after the software-mode handshake per module and
    record it, so `DeviceEvent::Input` for rollers actually arrives (OPEN O8 — untested
    on real hardware).
  - **Pipelined picture writes, DEPTH=8, on a second device object:** `send` must not
    block the control plane on a slow HID write. Upstream's macOS path pipelines image
    writes on a **second `IOHIDDevice` object** with up to 8 reports in flight
    (`upstream/.../pictures_macos.m`; `upstream/PROTOCOL.md:248-258`;
    `upstream/crates/mh-device/src/overlapped.rs:21-23` DEPTH=8;
    `docs/components/device-driver.md` invariant 7). Build the same on the macOS
    transport: a dedicated writer handle per module + a writer queue draining in-order
    completions, mirroring the per-module writer threads + latest-frame-wins queue of
    `docs/components/device-driver.md`. **Never use SET_REPORT on the base — it hangs 5 s**
    (`upstream/PROTOCOL.md:116-118`).
  - **Never backlight-0 the dial:** screens-off is a black JPEG, never a backlight 0
    report (`upstream/PROTOCOL.md:61`; `docs/components/daemon.md` invariant 5;
    `docs/20-security.md` T8). Unchanged — enforced above the backend in
    `report.rs`/`ds-protocol` safety.
  - **Hot-plug:** `IOHIDManagerRegisterDeviceMatchingCallback` /
    `IOHIDManagerRegisterDeviceRemovalCallback` for event-driven rescan (the macOS
    analogue of `docs/10-architecture.md` §5); keep the ≤ 1 s busy / 15 s idle poll as
    fallback (`crates/ds-device/src/hotplug.rs`).
  - Software-mode handshake before any read on keys/dial, forbidden-command refusal,
    and `query` gating are **unchanged** — they live above the backend
    (`crates/ds-device/src/handshake.rs`, `report.rs`).
- **Risk:** IOHIDManager callback threading is easy to get wrong (callbacks fire on the
  run-loop thread — never call `send` from inside a callback or it deadlocks the
  manager); exclusive-access conflicts with the vendor app are a UX cliff (the busy
  error must name the holder if `IORegistry` can say so); SET_IDLE timing vs the
  600 ms software-mode settle (`upstream/PROTOCOL.md:42,60,197`) is unmeasured (OPEN O8).
- **Acceptance test:** `cargo test -p ds-device` on a macOS runner with the vdev
  assertions plus a macOS hidapi unit suite against a fake device; hardware smoke pushes
  32 fps panel frames over the pipelined writer (DEPTH=8) and asserts p99 control
  latency ≤ 10 ms under frame load and roller `DeviceEvent::Input` after SET_IDLE on a
  real deck (OPEN O8 close-out).

## 3. Platform shim: focus via NSWorkspace + Accessibility (`ds-platform` role, `bins/ds-focus`)

There is no `ds-platform` crate today; the platform seam is the `ds-platform`/`ds-ipc`
shim named in `docs/00-scope.md` and `docs/components/daemon.md` ("platform (shim)"
module). On macOS this becomes a new `ds-platform` crate (or `cfg(macos)` modules in
`ds-daemon` + `bins/ds-focus`):

- **What exists:** `bins/ds-focus` with pluggable backends selected by
  `--backend auto|kwin|wlr|x11` (`bins/ds-focus/src/backend.rs`: trait at line 44,
  `detect` at line 104, `available_backends` at line 119); the KWin D-Bus and
  wlr-foreign-toplevel backends (`docs/components/focus-sender.md`), the settle/dedupe
  watch loop and the `app/focus/changed` payload schema
  `{"app":…,"title":…,"pid":…?}` (`docs/components/focus-sender.md`; `schemas/state.json`
  `/focus` mirrors it).
- **What must be built:** a `macos` backend for `bins/ds-focus` —
  - frontmost app via `NSWorkspace.frontmostApplication` (pid + `bundleIdentifier` as
    the `app`/wm-class, localized name as `title`);
  - focus change events via `NSWorkspace.didActivateApplicationNotification` (or
    AX `kAXFocusedWindowChangedNotification` / `kAXApplicationActivatedNotification`
    through the Accessibility API for window-level granularity — **pick one and pin it
    in the focus-sender contract**, OPEN O8);
  - same emit path: `{"op":"emit","topic":"app/focus/changed","payload":{…}}` with the
    same settle (300 ms) / dedupe / 500 ms latency budget
    (`docs/components/focus-sender.md`), so the daemon side and bindings are unchanged.
- **Key injection / input monitoring:** bindings that inject keys need the
  Accessibility **Input Monitoring** TCC permission (CGEventTap / `CGEventPost`); the
  daemon or `ds-focus` must detect a missing grant and fail loudly (exit with a clear
  message), never silently drop keys.
- **Risk:** AX events require the **Accessibility** TCC grant — a system Settings
  prompt on first run; without it focus detection is empty. Polling
  `frontmostApplication` is simpler but misses rapid switches; AX notifications are
  event-driven but can be throttled by the system. Both need a hardware-verified
  decision (OPEN O8).
- **Acceptance test:** on a macOS runner with Accessibility granted, switch
  frontmost apps and assert one deduped `app/focus/changed` event per settled change
  within the 500 ms budget; with the grant **denied**, `ds-focus --backend macos` exits
  non-zero with a message naming the TCC permission to grant.

## 4. Packaging: app bundle + launchd (`packaging/`)

- **What exists:** `packaging/` holds the systemd user unit, udev rules, MSI/wxs
  skeleton, AppImage spec, NOTICE.md (`docs/50-repo-topology.md` packaging row); release
  flow is `packaging/release.sh`. Nothing macOS is scheduled now.
- **What must be built:**
  - a macOS **app bundle** for the user-facing pieces (`ds.app` wrapping `ds`,
    `ds-focus`, optionally `ds-vdev`) so TCC prompts attach to a stable bundle identity
    (TCC grants are per bundle ID / path — ad-hoc binaries re-prompt on every path
    change);
  - a **launchd LaunchAgent** (`~/Library/LaunchAgents/ai.deckstack.ds-daemon.plist`,
    `RunAtLoad`/`KeepAlive`) replacing the systemd user unit for `ds daemon start`;
    `daemon stop` = `launchctl unload`;
  - code signing + notarization decision for distribution (Developer ID, or
    ad-hoc + documented "right-click → Open" first run); unsigned builds still work
    locally but TCC treats them per-path.
- **Risk:** TCC + signing interact — an unsigned binary re-prompts for Accessibility
  whenever its path changes; the LaunchAgent must reference the same signed bundle or
  TCC splits. Notarization is a release-flow change, out of scope for the port ticket.
- **Acceptance test:** on a clean macOS VM/user, install the bundle, `launchctl load`
  the agent, `ds daemon start --device virtual:…`, `ds ping` over the socket; uninstall
  (`launchctl unload` + delete bundle) leaves no agent, socket, or token behind.

## 5. TCC / permissions (the macOS permission model)

| Permission | Needed for | How granted | Failure mode if missing |
|---|---|---|---|
| **Accessibility** | `ds-focus` macos backend (AX notifications / NSWorkspace focus) | System Settings → Privacy & Security → Accessibility; one-time prompt | `ds-focus --backend macos` exits non-zero naming the grant; no focus events |
| **Input Monitoring** | key injection via CGEventPost in bindings | System Settings → Privacy & Security → Input Monitoring | injected keys silently dropped unless the check fails loudly — must log + surface |
| File access (sandbox, if enabled) | data dir `~/Library/Application Support/deckstack/`, token files | App Sandbox entitlement or no sandbox in alpha | `E_DEVICE`/`E_AUTH` style failures at first start — keep the daemon **unsandboxed in alpha** to match the Linux model |

- **What exists:** the Linux permission model is udev rules + systemd hardening
  (`docs/40-platforms.md` §1.1); T8 device-safety invariants
  (`docs/20-security.md:25`) must hold identically on macOS — forbidden HID bytes stay
  unrepresentable and backlight-0 stays refused regardless of OS.
- **What must be built:** a runtime TCC check helper in `ds-platform` (`AXIsProcessTrusted`,
  `IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)`-equivalent) called at `ds-focus` /
  daemon start, with the Settings deep-link (`x-apple.systempreferences:…`) in the error
  message; docs for operators in the runbook.
- **Risk:** TCC state is per-bundle-ID and survives reinstalls confusingly; CI runners
  cannot grant TCC interactively — the conformance path must use `--inject`
  (`docs/components/focus-sender.md` test hook) so tests never depend on a real grant.
- **Acceptance test:** with grants present, focus + injection work end-to-end on a
  hardware runner; with grants absent, both fail with the named-permission error; the
  conformance suite passes on a headless macOS CI runner using `--inject` only.

## 6. Test plan

| Layer | What | Where |
|---|---|---|
| Unit | `cfg(target_os = "macos")` arms of `paths.rs`, `lifecycle.rs`; IOHIDManager transport against a fake; TCC-check helper (mocked trust flags); packaging plist generation | `cargo test --workspace` on a macOS runner (new CI job — only when the port starts; **no macOS CI gate in alpha**, `docs/30-testing.md:114-117` §8 stays Linux-only) |
| Conformance equivalent | the full `tests/conformance` suite over the Unix socket with ds-vdev, byte-identical results to the Linux run; rotation/revocation (S12/M4) cases included; focus conformance via `ds-focus --inject` only (no TCC grant on CI) | macOS runner, conformance runner pointed at the macOS data-dir socket |
| Performance (OPEN O8) | pipelined picture writes DEPTH=8: control p99 ≤ 10 ms under 32 fps DSF1 frame load; compare against Linux numbers | macOS runner with a deck, `ds-bench` harness |
| Security (T8) | forbidden-report suite, backlight-0 refusal, hardware-mode read refusal — the same DS-029 safety suite must pass on macOS; second local user cannot reach the socket (dir 0700) | macOS CI/hw job |
| Hardware smoke (OPEN O8) | real deck on a macOS runner: exclusive-open busy error when the vendor app holds the deck, handshake + SET_IDLE → roller input events, 32 fps panel push camera-checked per `docs/90-verification.md`, hot-plug via IOHIDManager callbacks | `DS_HARDWARE=1` smoke suite on the macOS runner + deck (mirrors DS-026) |

## 7. Evidence gaps (OPEN O8 — all of macOS is untested)

- SET_IDLE-at-connect behavior on rollers is asserted from upstream
  (`upstream/crates/mh-device/src/recover.rs:15-18`; `upstream/PROTOCOL.md:40`) but never
  measured on a deck through this codebase.
- IOHIDManager exclusive-access semantics and the pipelined-writer DEPTH=8 path are
  asserted from hidapi docs and `upstream/.../pictures_macos.m`, not yet measured.
- NSWorkspace vs AX focus fidelity (event rate, throttling, `pid` availability) is
  unverified — no `docs/discovery/` row yet.
- TCC grant flow on a fresh user account (prompt text, deep-link, headless CI behavior)
  is unverified.

## 8. Suggested ticket breakdown when the port starts

1. `ds-ipc`/`lifecycle` macOS path arms (data dir under `~/Library/Application Support`).
2. `ds-device` IOHIDManager transport: exclusive-open busy error, run-loop vs reader
   thread decision, SET_IDLE at connect, hot-plug callbacks.
3. `ds-device` pipelined picture writer (second device object, DEPTH=8) + perf bench.
4. `ds-platform` crate + `ds-focus --backend macos` (NSWorkspace + AX) + TCC check helper.
5. Packaging: app bundle, launchd LaunchAgent, signing decision.
6. macOS CI gate (build+unit) then runner-with-deck conformance + hardware smoke
   (closes O8).

## 9. Rot risk

This breakdown assumes the contract versions listed in the header (2026-11-23). As the
Linux core evolves, re-date and re-verify against `docs/contracts/ipc.md`,
`crates/ds-device/src/transport.rs:36-56`, and `docs/40-platforms.md` §3 before ticketing
any item.
