# 40 — Platforms

R9. Design: D12. Linux (Bazzite/Fedora, KDE/Wayland) is primary. The core assumes nothing
POSIX: all OS specifics live behind `ds-platform` / `ds-ipc`. **Honesty rule (S15/M6/M7):
we do not claim a platform is supported until its gate has actually run.**

## 0. Support status (S15)

| Platform | Status claim |
|---|---|
| Linux (Bazzite/KDE/Wayland) | primary; fully tested virtually in CI, hardware smoke on self-hosted deck |
| Windows | **supported shape, untested on hardware**: named-pipe transport is exercised by the conformance suite on the Windows CI VM (incl. the p99 budget, M6), pipe ACL per S5; real-deck behaviour gated in phase 2 (OPEN O5). Not claimed met before that |
| macOS | **untested, not phase 1–2**: CI builds + unit tests only; exclusive hidapi opens (OPEN O8) and SET_IDLE roller fix are known requirements, not verified here |

## 1. Linux (primary — Bazzite, KDE Plasma on Wayland)

| Need | Mechanism | Status / evidence |
|---|---|---|
| HID access | hidapi `linux-static-libusb` (vendor HID gets no hidraw; udev rule `99-deckstack.rules`, `deckstack` group) | proven `upstream/crates/mh-device/Cargo.toml:17-20`; OPEN O4 |
| udev hot-plug | libusb poll ≤ 1 s busy / 15 s idle | `upstream/crates/mh-device/src/hub.rs:19-21,149-155` |
| Service | systemd user unit, fallback detached | D11 |
| Focus | external ds-focus: KWin D-Bus script (proven `hubdeck/.../platforms/linux/focus.py`); wlr-foreign-toplevel elsewhere (OPEN O1) | discovery 30/50 |
| Key injection | `/dev/uinput` + udev rule | `hubdeck/.../platforms/linux/keyboard.py:35-177`; `upstream/.../engine/uinput.rs` |
| Media / idle | MPRIS; `loginctl`/`busctl` | `upstream/.../engine/idle.rs:29-97` |
| Audio | stays external (`Unsupported` in Python app) | `hubdeck/.../platforms/linux/__init__.py:8-9` |
| Permissions | udev rules; systemd unit hardening (NoNewPrivileges, ProtectSystem=strict, PrivateTmp) | packaging/ |

## 2. Windows (supported shape; hardware gate in phase 2 — S15/M6)

| Need | Mechanism | Evidence |
|---|---|---|
| IPC | named pipe `\\.\pipe\deckstack-<user>` + `-stream`, same framings via ds-ipc; **ACL `D:P(A;;GA;;;SY)(A;;GA;;;<user-sid>)`** (S5/T12); token dir/files ACL user+SYSTEM | R7; upstream has no pipes (discovery/50 O3); pipe perf OPEN O5, CI conformance asserts p99 on VM |
| HID | hidapi winhid; per-report-type handle memory; overlapped writes, second handle, DEPTH=8 | `hubdeck/.../device/hid.py:199-231`; `upstream/crates/mh-device/src/overlapped.rs:21-23` |
| Hot-plug | `CM_Register_Notification`; wedged module → `pnputil /restart-device` | `upstream/crates/mh-device/src/notify.rs`; `upstream/PROTOCOL.md:192-197` |
| Focus | WinEvent hook + 250 ms poll, in external ds-focus | `upstream/.../engine/foreground.rs:14-155` |
| Key injection | SendInput | planned |
| Plugins | job object `KILL_ON_JOB_CLOSE`, `CREATE_NO_WINDOW` | `upstream/.../engine/plugins.rs:180-205` |
| UIPI | detection + refusal | `upstream/.../engine/elevated.rs` |
| Packaging | single exe / MSI (wixl precedent `hubdeck/packaging/windows/hubdeck.wxs`); start via `ds daemon start` (Task Scheduler or sc) | planned |
| Shell glue | PowerShell examples, cli.md §6 | R9 |

## 3. macOS (untested — not claimed, M7/O8)

| Need | Mechanism | Evidence |
|---|---|---|
| HID | hidapi darwin, **exclusive opens** — second daemon refuses with a clear error | `hubdeck/docs/masterhub-protocol.md:36-37` |
| Rollers | SET_IDLE at connect | `upstream/crates/mh-device/src/recover.rs:15-18` |
| Pipelined writes | second device object, 8 in flight | `upstream/.../pictures_macos.m`; `upstream/PROTOCOL.md:248-258` |
| Backlight-0 brick | never 0 on dial | `upstream/PROTOCOL.md:61` |
| Focus | NSWorkspace + AX in ds-focus | `hubdeck/.../platforms/macos/focus.py:27-141` |

## 4. Deliberately external (never in core)

Window watching (ds-focus), media (MPRIS), audio, cron/webhooks/git hooks, video decode
(ffmpeg), UI (ds-web).

## 5. Untested (explicit)

Pure Wayland non-KDE focus (O1) · two same-kind screen modules (O3) · fader LED soak (O2) ·
hidraw vs libusb (O4) · Windows pipe on real hardware (O5) · Landlock fs+net on target
kernel (O6/O11) · **all of macOS** (O8) · Linux audio.
