# 40 — Platforms

R9. Design: D12. Linux (Bazzite/Fedora, KDE/Wayland) is primary. The core assumes
nothing POSIX: all OS specifics live behind `ds-platform` (and `ds-ipc` for sockets).

## 1. Linux (primary — Bazzite, KDE Plasma on Wayland)

| Need | Mechanism | Status / evidence |
|---|---|---|
| HID access | hidapi `linux-static-libusb` (vendor HID gets no hidraw node; udev rule shipped: `99-deckstack.rules` granting the `deckstack` group) | proven: `upstream/crates/mh-device/Cargo.toml:17-20`; OPEN O4 (hidraw alternative) |
| udev hot-plug | libusb poll ≤ 1 s busy / 15 s idle (no notifications on Linux) | `upstream/crates/mh-device/src/hub.rs:19-21,149-155` |
| Service | systemd user unit `ds-daemon.service` (`systemctl --user`), fallback detached start | D11 |
| Window focus | **external** ds-focus: KWin D-Bus script (proven: `hubdeck/.../platforms/linux/focus.py`); wlr-foreign-toplevel for other Wayland DEs (OPEN O1 — measure on Bazzite) | discovery 30/50 O1 |
| Key injection | `/dev/uinput` (needs `uinput` group; udev rule shipped) | `hubdeck/.../platforms/linux/keyboard.py:35-177`; `upstream/.../engine/uinput.rs` |
| Media keys | MPRIS over D-Bus | `hubdeck/.../platforms/linux/` (Python more complete) |
| Audio | `Unsupported` on Linux in the Python app — stays external | `hubdeck/.../platforms/linux/__init__.py:8-9` |
| Idle/lock | `loginctl`/`busctl` subprocesses | `upstream/.../engine/idle.rs:29-97` |
| Permissions | udev rules for HID+uinput; systemd unit hardening (NoNewPrivileges, ProtectSystem=strict, PrivateTmp) | docs/50-repo-topology.md packaging |

## 2. Windows

| Need | Mechanism | Evidence |
|---|---|---|
| IPC | named pipe `\\.\pipe\deckstack-<user>` (+ `-stream`), same NDJSON/binary framing via ds-ipc | R7; upstream has NO named pipes (grep-verified, discovery/50 O3) — new code, OPEN O5 perf |
| HID | hidapi winhid; multiple paths per module — driver remembers the handle that accepts each report type; overlapped writes, second handle, DEPTH=8, 1 s timeout | `hubdeck/.../device/hid.py:199-231`; `upstream/crates/mh-device/src/overlapped.rs:21-23` |
| Hot-plug | `CM_Register_Notification` | `upstream/crates/mh-device/src/notify.rs` |
| Wedged module | `pnputil /restart-device` (admin) | `upstream/PROTOCOL.md:192-197` |
| Focus | `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + 250 ms poll, in external ds-focus | `upstream/.../engine/foreground.rs:14-155` |
| Key injection | SendInput | planned |
| Plugins | job object `KILL_ON_JOB_CLOSE`, `CREATE_NO_WINDOW` | `upstream/.../engine/plugins.rs:180-205` |
| UIPI | detection + refusal of elevated-target writes | `upstream/.../engine/elevated.rs` |
| Packaging | MSI (wixl precedent `hubdeck/packaging/windows/hubdeck.wxs`) or single exe; service via `ds daemon start` (sc.exe) or Task Scheduler | planned |
| Shell glue | PowerShell examples in docs/contracts/cli.md §6 | R9 |

## 3. macOS

| Need | Mechanism | Evidence |
|---|---|---|
| HID | hidapi darwin; **exclusive opens** — one holder only; second daemon refuses with a clear error (OPEN O8) | `hubdeck/docs/masterhub-protocol.md:36-37` |
| Rollers | SET_IDLE at connect | `upstream/crates/mh-device/src/recover.rs:15-18`; `upstream/PROTOCOL.md:40` |
| Pipelined writes | second device object, 8 in flight | `upstream/.../pictures_macos.m`; `upstream/PROTOCOL.md:248-258` |
| Backlight-0 brick | never send 0 to dial; screens-off = black JPEG | `upstream/PROTOCOL.md:61` |
| Focus | NSWorkspace + AX title (Accessibility permission) in ds-focus | `hubdeck/.../platforms/macos/focus.py:27-141` |
| Packaging | .app bundle | `hubdeck/packaging/macos/` |

## 4. What is deliberately external (never in core)

Window watching (ds-focus), media control (MPRIS/playerctl), audio, cron/webhooks/git hooks
(ordinary programs calling `ds emit`), video decode (ffmpeg), UI (ds-web). Core = daemon +
device + state + router + frames + plugins.

## 5. Untested (explicit)

Pure Wayland non-KDE focus (O1) · two same-kind screen modules (O3) · fader LED soak
(O2) · hidraw vs libusb (O4) · Windows pipe perf (O5) · Landlock on target kernel (O6) ·
all of macOS (O8) · Linux audio.
