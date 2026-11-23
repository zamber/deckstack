# 40 — Platforms

R9 + scope delta 1: **alpha is Linux only** (Bazzite/Fedora, KDE Plasma on Wayland first).
Windows and macOS are **not implemented in alpha** — they are TODO sections below with
exactly what to implement, where the shim goes, what must be tested, and the rule: **do
not start until Linux alpha is stable**. The `ds-platform`/`ds-ipc` shims stay in the core
so the ports are additive, not refactors.

## 0. Support status (authoritative for alpha)

| Platform | Status |
|---|---|
| Linux (Bazzite/KDE/Wayland) | **alpha target**; vdev-tested in CI, hardware smoke + camera (`docs/90-verification.md`) on the self-hosted deck |
| Windows | **TODO, not in alpha**; build-only or no CI gate; see §2 |
| macOS | **TODO, not in alpha**; no CI gate; see §3 |

No alpha phase exit criterion or CI gate mentions Windows or macOS (`docs/60-roadmap.md`,
`docs/30-testing.md` §8).

## 1. Linux alpha (Bazzite, KDE Plasma on Wayland)

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

### 1.1 Install: udev rules and device permissions (operator)

Phase 2 runs as a normal user, so the deck's device nodes need a udev rule — no
root/sudo at runtime. Ship `packaging/udev/99-deckstack.rules` (VID `0x2516`, the five
module PIDs `01f6|01fe|0202|0204|0206`, `TAG+="uaccess"` on both the `hidraw` and `usb`
nodes; no `MODE=0666`):

```sh
sudo install -Dm644 packaging/udev/99-deckstack.rules /etc/udev/rules.d/99-deckstack.rules
# either location works; /etc/udev/rules.d overrides /usr/lib/udev/rules.d
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Verify the rule took effect on the plugged-in deck:

```sh
ls -l /dev/hidraw*                 # uaccess nodes carry an ACL for the logged-in user
udevadm info /dev/hidraw<N>        # check the tag; replug if the device was already attached
```

**Group fallback** (systems without logind/`uaccess`): use the commented rule variant
`GROUP="deckstack", MODE="0660"` in the rules file, then
`sudo groupadd -r deckstack && sudo usermod -aG deckstack "$USER"` and re-login.

**Without the rule** the daemon cannot open the device: `hid:` open fails with
`E_DEVICE` (CLI exit 7, `docs/contracts/ipc.md` §4) — the same code as any hardware
error, so check `ls -l /dev/hidraw*` first when diagnosing.

**`ds-vdev` avoids all of this**: the virtual device (`ds daemon start --device
virtual:/tmp/vdev.sock`, `docs/runbook.md`) talks over a Unix socket owned by the
user, so no hidraw/usb node and no udev rule are involved — that is why CI and the
runbook need no permissions setup.

## 2. Windows — TODO (do not start until Linux alpha is stable)

The itemized port breakdown (named-pipe IPC + ACLs, hidapi winhid transport, overlapped
writes, job objects, packaging, test plan) lives in **`docs/breakdowns/windows.md`** —
schedulable ticket-by-ticket from there, still gated by the rule above.

**Where the shim goes:** `ds-ipc` (named pipe `\\.\pipe\deckstack-<user>` + `-stream`,
same NDJSON/DSF1 framings; ACL SDDL `D:P(A;;GA;;;SY)(A;;GA;;;<user-sid>)`, S5/T12);
`ds-platform` (WinEvent focus hook, SendInput, job objects, UIPI).
**What to implement:** hidapi winhid with per-report-type handle memory
(`hubdeck/.../device/hid.py:199-231`), overlapped writes DEPTH=8 on a second handle
(`upstream/crates/mh-device/src/overlapped.rs:21-23`), `CM_Register_Notification`
(`upstream/crates/mh-device/src/notify.rs`), `pnputil /restart-device` for wedged modules
(`upstream/PROTOCOL.md:192-197`), plugin job object `KILL_ON_JOB_CLOSE`
(`upstream/.../engine/plugins.rs:180-205`), PowerShell glue examples (cli.md §6), MSI/single-exe
packaging.
**What must be tested:** pipe throughput incl. p99 budget under 32 fps frame load on a VM
(OPEN O5), pipe/file ACL with a second local account (T12), full conformance suite over the
pipe, hardware smoke on a Windows VM with the deck. **Evidence gaps:** upstream has no
named pipes at all (grep-verified, discovery/50 O3).

## 3. macOS — TODO (do not start until Linux alpha is stable)

**Where the shim goes:** `ds-platform` (NSWorkspace+AX focus, app bundle), ds-device macOS
paths. **What to implement:** exclusive hidapi opens with a clear "busy" error (OPEN O8,
`hubdeck/docs/masterhub-protocol.md:36-37`), SET_IDLE at connect for rollers
(`upstream/crates/mh-device/src/recover.rs:15-18`; `upstream/PROTOCOL.md:40`), pipelined
pictures on a second device object DEPTH=8 (`upstream/.../pictures_macos.m`;
`upstream/PROTOCOL.md:248-258`), never backlight-0 on the dial (`upstream/PROTOCOL.md:61`).
**What must be tested:** full conformance on a macOS runner, hardware smoke with a deck,
roller input after SET_IDLE.

## 4. Deliberately external (never in core)

Window watching (ds-focus), media (MPRIS), audio, cron/webhooks/git hooks, video decode
(ffmpeg), any UI.

## 5. Untested (explicit)

Pure Wayland non-KDE focus (O1) · two same-kind screen modules (O3) · fader LED soak (O2) ·
hidraw vs libusb (O4) · **Windows pipe on real hardware (O5)** · Landlock fs+net on target
kernel (O6/O11) · **all of macOS (O8)** · Linux audio.
