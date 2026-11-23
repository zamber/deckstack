# Breakdown: Windows port (TODO — documentation only, DS-072)

**Date-stamped 2026-11-23.** This document turns `docs/40-platforms.md` §2 into an
itemized, schedulable breakdown. It implements nothing and changes no code; every item
below is TODO until a future Windows phase is ticketed. The standing rule stays:
**do not start until Linux alpha is stable** (deps DS-020 e2e, DS-026 hardware smoke;
`docs/00-scope.md` Linux-only alpha, ADR-011).

**Assumed contract versions this breakdown is written against:** `docs/contracts/ipc.md`
(protocol `1`, `crates/ds-ipc/src/lib.rs:75` `PROTOCOL_VERSION = 1`; schemas
`schemas/ipc-message.json`, `schemas/stream-frame.json` as of 2026-11-23),
`docs/contracts/state.md` + `schemas/state.json`, the `Transport` trait in
`crates/ds-device/src/transport.rs`, and the `hid` module of `crates/ds-device/src/hid/`.
If those move, re-date this document (risk R-rot, §9).

**Evidence convention:** `upstream/…` and `hubdeck/…` citations are path:line evidence
recorded in `docs/discovery/`; items not verifiable today are marked **OPEN**.

## 1. IPC: named pipes with ACLs (`ds-ipc`)

### 1.1 Endpoint mapping (Unix socket contract → Windows named pipe)

| Unix (alpha, normative) | Windows (to build) |
|---|---|
| `$XDG_RUNTIME_DIR/deckstack/daemon.sock`, dir 0700, socket 0600 (`docs/contracts/ipc.md` §1.1, `crates/ds-ipc/src/server.rs` `Server::bind`) | `\\.\pipe\deckstack-<user>` control pipe; `\\.\pipe\deckstack-<user>-stream` frame pipe |
| `<dir>/stream.sock` | second pipe above (no filesystem socket exists) |
| NDJSON control framing, DSF1 binary frames, `DSctl\n` stream control (`docs/contracts/ipc.md` §2) | **identical wire formats** — the pipe is a byte stream, so `framing.rs`/`frames/` are reused unchanged; only the listen/accept and read/write plumbing changes |
| per-install `token` + `tokens.json`, 0600, in the data dir (`crates/ds-daemon/src/lifecycle.rs` `RunPaths`) | same two files under `%LOCALAPPDATA%\deckstack\` (or `%APPDATA%\deckstack\`), with Windows ACLs instead of mode bits (T12) |
| hello → auth → ops state machine, scope table, rotation generation (`crates/ds-ipc/src/server.rs` `dispatch`) | unchanged logic; the `Server`/`Connection` types need a `cfg(windows)` transport layer, not a rewrite |

- **What exists:** the full server logic above is transport-agnostic down to
  `handle_connection`/`dispatch`; only `Server::bind`, `accept_loop`, `Connection`'s
  `UnixStream` writer, and `paths.rs` are Unix-bound. Client side (`ds` CLI) needs the
  pipe path substituted for the socket path.
- **What must be built (gated `#[cfg(windows)]` implementations):**
  - `crates/ds-ipc/src/paths.rs`: Windows data dir resolution (`%LOCALAPPDATA%\deckstack`
    fallback `%APPDATA%\deckstack`; no `XDG_RUNTIME_DIR`), pipe name from the user SID or
    logon name (`deckstack-<user>`).
  - `crates/ds-ipc/src/server.rs` (or a new `server_win.rs` behind `cfg`): a
    `CreateNamedPipeW` listener with `FILE_FLAG_FIRST_PIPE_INSTANCE`, explicit
    `SECURITY_ATTRIBUTES` carrying SDDL `D:P(A;;GA;;;SY)(A;;GA;;;<user-sid>)`
    (SYSTEM + owning user only — S5/T12, `docs/contracts/ipc.md` §1.1), `ConnectNamedPipe`
    accept loop mirroring `accept_loop`'s connection cap (≤ 64, `limits.rs`), one thread
    per connection mirroring `ds-ipc-conn-<id>`. `CreateFileW` on the client side must
    expect `ERROR_PIPE_BUSY` → `WaitNamedPipeW` retry.
  - Token files: `SetNamedSecurityInfo`/`SetSecurityDescriptorDacl` equivalent on the data
    dir, `token`, `tokens.json` — user + SYSTEM only, no inherited broad ACEs (T12). The
    dir-ACL and file-ACL code replaces the `0700`/`0600` `set_permissions` calls in
    `crates/ds-daemon/src/lifecycle.rs` (`prepare_data_dir`, `write_pid`, `ensure_token`).
  - Frame plane: same NDJSON/DSF1 code over the `-stream` pipe; `DSctl\n` prefix
    unchanged.
- **Risk:** named pipes default to broad ACLs (T12) — a pipe created without the SDDL
  is connectable by any local user; also `PIPE_UNLIMITED_INSTANCES` misuse can fork
  listeners. Pipe throughput on real hardware is unmeasured (OPEN O5).
- **Acceptance test:** (a) second local account connect attempt → `ERROR_ACCESS_DENIED`
  and `E_AUTH`-equivalent refusal (T12); (b) full conformance suite (`tests/conformance`)
  run over the pipe with the virtual device, byte-identical to the Unix run; (c) pipe
  throughput incl. p99 ≤ 10 ms control latency under 32 fps frame load on a Windows VM
  (OPEN O5).

## 2. Device transport: hidapi `winhid` behind the existing `Transport` trait (`ds-device`)

- **What exists:** the trait (`crates/ds-device/src/transport.rs`: `send`, `query`, `recv`,
  `close`, all `&self`, `Send + Sync`) and the Linux implementation
  `crates/ds-device/src/hid/{mod,enumerate,reader}.rs` (per-module `HidDevice` behind a
  `Mutex`, one reader thread per module looping `hid_read_timeout` in 20 ms slices,
  shared `Inbox`/`Condvar` event queue, `track_mode` software-mode tracking, forbidden-
  report guard + padding in `prepare_write`). Enumeration keys modules by VID `0x2516`
  + per-kind PIDs (`crates/ds-device/src/module.rs`).
- **What must be built:** a `cfg(windows)` sibling of `hid/mod.rs` —
  `WinHidTransport` implementing the same `Transport` trait — reusing `enumerate`'s
  VID/PID logic, `report::{ensure_allowed, pad_report}`, `handshake`, and the shared
  `Inbox` event model unchanged. hidapi's `winhid` backend replaces `linux-static-libusb`
  (same crate, different backend feature; the pinned `mh-device` selects its backend per
  platform already, `upstream/crates/mh-device/Cargo.toml:17-20`). Specifics:
  - **Per-report-type handle memory:** on Windows one `HIDP` preparsed handle per report
    type; keep the per-module handle map keyed by `(ModuleId, report id)`
    (`hubdeck/.../device/hid.py:199-231` precedent).
  - **Overlapped writes, DEPTH=8, on a second handle:** `send` must not block the control
    plane on a slow HID write. Open a second `HidDevice` handle per module for writes and
    issue `WriteFile` with an `OVERLAPPED` event, up to 8 in flight
    (`upstream/crates/mh-device/src/overlapped.rs:21-23`); a dedicated writer thread
    drains completions — this is the Windows analogue of the per-module writer threads +
    latest-frame-wins queue in `docs/components/device-driver.md`. The reader thread keeps
    using the read handle with `hid_read_timeout` slices exactly like
    `crates/ds-device/src/hid/reader.rs`.
  - **Hot-plug:** `CM_Register_Notification` (device-interface class
    `GUID_DEVINTERFACE_HID`) for event-driven rescan instead of polling
    (`upstream/crates/mh-device/src/notify.rs`; `docs/10-architecture.md` §5 already
    names it); keep the ≤ 1 s busy / 15 s idle poll as fallback.
  - **Wedged modules:** on repeated read/write errors, shell out to
    `pnputil /restart-device` (admin) before dropping the module
    (`upstream/PROTOCOL.md:192-197`; `docs/10-architecture.md` §6).
  - Software-mode handshake, forbidden-command refusal, and `query` gating on keys/dial
    are **unchanged** — they live above the backend in `handshake.rs`/`report.rs`.
- **Risk:** hidapi winhid opens are exclusive per interface and fail busy if the vendor
  `MasterHUB`/`MHDaemon` holds the device (startup check must say so explicitly,
  `upstream/PROTOCOL.md:217-221`); overlapped writes change error semantics (async
  completion errors surface late — map them to `DeviceEvent::Disconnected` like the
  Linux path). Vendor-defined interfaces on Windows do not get a raw HID path the way
  libusb does, so `hid:<os-path>` URIs become `hid:<device-instance-path>`.
- **Acceptance test:** `cargo test -p ds-device` on a Windows runner with the vdev
  assertions plus a winhid unit suite against a filter-driver fake (or a real deck in
  the hardware smoke); hardware smoke pushes 32 fps panel frames over overlapped writes
  and asserts no control-plane stall (p99 control ≤ 10 ms under frame load) on a
  Windows VM with the deck.

## 3. Platform shim: focus, input injection, child containment (`ds-platform` role)

There is no `ds-platform` crate today; the platform seam is the `ds-platform`/`ds-ipc`
shim named in `docs/00-scope.md` and `docs/components/daemon.md:39`. On Windows this
becomes a new `ds-platform` crate (or `cfg(windows)` modules in `ds-daemon` +
`bins/ds-focus`):

- **Focus detection (ds-focus):** `SetWinEventHook(EVENT_OBJECT_FOCUS / NAME_CHANGE)`
  on the foreground window instead of KWin D-Bus (`upstream/.../foreground.rs:14-155`);
  emit the same `app/focus/changed` topic with the same payload schema
  (`docs/components/focus-sender.md`). **OPEN:** wm-class equivalent via
  `GetClassNameW`/UIAutomation — pick one and pin it in the focus-sender contract.
- **Key injection:** `SendInput` instead of `/dev/uinput` (bindings that inject keys).
  **UIPI:** a medium-integrity daemon cannot `SendInput` into elevated windows — either
  run the daemon elevated (packaging decision, §5) or document the limitation; do not
  silently fail.
- **Job objects for child/plugin containment:** every plugin process is spawned into a
  Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so no plugin outlives the daemon
  (`upstream/.../engine/plugins.rs:180-205`; `docs/20-security.md` §job-object row);
  daemon shutdown terminates the job, which kills all plugins — the Windows equivalent
  of the guard/handback path (`docs/10-architecture.md` §6). Also set
  `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` and breakaway rules per plugin policy.
- **What exists:** the plugin host's capability model and lifecycle in `ds-daemon`
  (`crates/ds-daemon/src/plugins/`), the exec policy `/policy/exec` allow−deny
  (argv-only, no shells — unchanged on Windows).
- **Risk:** UIPI elevation mismatch is a UX cliff; job objects require the daemon to
  hold the job handle for its lifetime (leak = orphan plugins).
- **Acceptance test:** unit test that a killed daemon leaves zero plugin processes
  (`Get-Process` where parent chain broken); focus hook test on a VM switching windows;
  SendInput test into a non-elevated foreground window.

## 4. Path and locale differences

- **Paths:** no `XDG_RUNTIME_DIR`; data dir moves to `%LOCALAPPDATA%\deckstack\`
  (fallback `%APPDATA%\deckstack\`), plugins to `%APPDATA%\deckstack\plugins\<id>\`,
  logs to `%LOCALAPPDATA%\deckstack\daemon.log`. `crates/ds-ipc/src/paths.rs` and
  `crates/ds-daemon/src/lifecycle.rs::RunPaths` get `cfg(windows)` arms. Pipe names use
  the user name/SID, so two local users on one machine each get their own pipes and data
  dirs (this is what makes the T12 second-account test meaningful).
- **Signals → events:** `signal_hook` SIGTERM/SIGINT has no Windows equivalent;
  `lifecycle.rs::Shutdown::install` becomes a `SetConsoleCtrlHandler` +
  `CreateEvent`-based shutdown, and service mode uses SCM stop requests.
- **Locale/encoding:** all NDJSON stays UTF-8; Windows console output needs
  `SetConsoleOutputCP(CP_UTF8)` or wide-char printing in the CLI; paths with non-ASCII
  user names must go through wide APIs end-to-end (no `to_string_lossy` on the pipe
  name). State JSON schema is encoding-agnostic — no schema change.
- **Risk:** `\` vs `/` in URI parsing (`hid:<path>` rest, `virtual:` is Linux-only);
  drive-letter colons collide with URI scheme parsing in `crates/ds-device/src/uri.rs`.
- **Acceptance test:** paths unit tests on Windows with a non-ASCII user name; CLI
  output round-trip test; `ds daemon start/stop` via console ctrl handler.

## 5. Packaging (MSI / portable)

- **What exists:** `packaging/` holds the systemd unit, udev rules, an MSI/wxs skeleton,
  AppImage spec, NOTICE.md (`docs/50-repo-topology.md` packaging row); release flow is
  `packaging/release.sh`. DS-083 stays Linux-first — nothing here is scheduled now.
- **What must be built:** WiX/wixl MSI producing a per-user install (no admin required
  if everything lives under `%LOCALAPPDATA%`/`%APPDATA%`, which matches the ACL model),
  Start-Menu shortcut for `ds`, optional service install (admin) for the daemon, and a
  portable single-exe zip (`ds.exe` + `ds-focus.exe` + `ds-vdev.exe`) for the no-install
  path. The MSI must set the token-dir/file ACLs at install time (T12) or the daemon
  sets them at first start — decide in the packaging ticket; first-start is simpler.
- **Risk:** per-user MSI + elevated daemon is contradictory — pick one integrity model
  and keep UIPI (§3) consistent with it. Antivirus false positives on `SendInput`/
  `SetWinEventHook` are likely; document.
- **Acceptance test:** install MSI on a clean Windows VM as a standard user, run
  `ds daemon start --device virtual:…`, `ds ping` over the pipe, uninstall leaves no
  pipes/files behind; portable zip runs from a USB path.

## 6. Test plan

| Layer | What | Where |
|---|---|---|
| Unit | `cfg(windows)` arms of `paths.rs`, `lifecycle.rs`, `server_win.rs` ACL construction (assert the SDDL string), winhid transport against a fake, job-object spawn/kill | `cargo test --workspace` on a Windows runner (new CI job — only when the port starts; **no Windows CI gate in alpha**, `docs/30-testing.md` §8 stays Linux-only) |
| Conformance equivalent | the full `tests/conformance` suite over the named pipe with ds-vdev, byte-identical results to the Linux Unix-socket run; rotation/revocation (S12/M4) cases included | Windows VM, conformance runner pointed at `\\.\pipe\deckstack-<user>` |
| Performance (OPEN O5) | pipe throughput + control p99 ≤ 10 ms under 32 fps DSF1 frame load; compare against Linux numbers | Windows VM, bench harness `ds-bench` |
| Security (T12) | second local account: pipe connect → access denied; token file read → denied; `ds auth` with stolen-token file → `E_AUTH` | Windows CI/hw job with two accounts |
| Hardware smoke | real deck on a Windows VM: handshake, 32 fps panel push camera-checked per `docs/90-verification.md`, hot-plug via `CM_Register_Notification`, wedge recovery via `pnputil /restart-device` | `DS_HARDWARE=1` smoke suite on the Windows VM + deck (mirrors DS-026) |

## 7. Evidence gaps (OPEN)

- Upstream Rust engine has **no named pipes at all** — all IPC is loopback TCP + files
  (grep-verified, discovery/50 O3) — so the pipe design here is new, not ported; pipe
  perf on real hardware is unmeasured (OPEN O5).
- WinEvent focus-hook fidelity vs the KWin D-Bus payload is unverified (no
  `docs/discovery/` row yet).
- hidapi `winhid` vs libusb behaviour differences (exclusive opens, report-id handling)
  are asserted from hidapi docs, not yet measured with the deck.

## 8. Suggested ticket breakdown when the port starts

1. `ds-ipc` Windows pipe transport + ACLs (+ T12 test) — depends on this doc.
2. `ds-device` winhid transport + overlapped writer + `CM_Register_Notification`.
3. `ds-platform` crate: WinEvent focus, SendInput/UIPI decision, job objects.
4. `lifecycle.rs`/`paths.rs` Windows arms (paths, console-ctrl shutdown, ACL'd token dir).
5. Packaging: MSI + portable zip.
6. Windows CI gate + VM conformance + hardware smoke (O5/T12 close-out).

## 9. Rot risk

This breakdown assumes the contract versions listed in the header (2026-11-23). As the
Linux core evolves, re-date and re-verify against `docs/contracts/ipc.md` and
`crates/ds-device/src/transport.rs` before ticketing any item.
