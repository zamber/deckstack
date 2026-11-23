# Runbook — deckstack operator guide

The single operator-facing guide for installing, running, and recovering the `ds-daemon`
process. Everything here is reachable from the `ds` CLI — there is no web UI and no
plugin UI in the alpha (see §1). Design references: `docs/components/daemon.md`,
`docs/contracts/ipc.md`, `docs/contracts/cli.md`, `docs/20-security.md`,
`docs/40-platforms.md`.

Every command in this runbook was executed once against a live `ds-vdev` + `ds-daemon`
(vdev session, `virtual:` device) while this document was written; the observed output
shapes are shown inline. Commands that need real hardware are marked **[hardware run]**
and were not executed here.

## 1. Scope and out-of-scope

- Linux-only alpha, starting from an empty state (no presets/sample decks), no
  `ds migrate` verb (scope: `docs/00-scope.md`). `ds migrate <anything>` exits 1:
  `ds: there is no 'migrate' verb — porting of old apps is manual and ticket-driven (docs/70-porting-notes.md)  [exit 1]`
- **Not implemented yet in this tree (do not expect them to work):**
  - `ds token list|issue|revoke` — the `token` verb group is recognized but refused:
    `ds: verb 'token' is not implemented in phase 1 (later ticket); see `ds --help`  [exit 1]`.
    Token *management* today = protecting the install token file (§10); scoped tokens land
    with their ticket (T9 rotation is designed in `docs/20-security.md`).
  - `ds emit`, `ds events`, `ds plugin …` — same later-phase refusal, exit 1.
  - `ds stream push` — the frame-stream socket `stream.sock` is defined by the contract
    (`docs/contracts/ipc.md` §1) but the daemon does not bind it yet in this tree, so
    `ds stream push …` exits 6 `E_UNREACHABLE` with a "stream.sock missing" hint.
    `ds stream stats <target>` / `ds stream stop <target>` work over the control socket
    (§9). State→device image pushes via `ds set … --file/--json` do work (§5).
  - `ds daemon restart` — there is no `restart` subcommand; restart = stop + start or
    `systemctl --user restart` (§11).
- Out of scope for the alpha entirely: web/debug UI over the WS/HTTP adapter (the adapter
  exists for agents only), Windows/macOS, plugin UI, any importer/migrator
  (`docs/00-scope.md`).

## 2. Prerequisites

- Linux with a systemd **user** session (Bazzite/Fedora default): `echo $XDG_RUNTIME_DIR`
  must print `/run/user/<uid>` and `systemctl --user is-system-running` must answer
  `running`/`degraded`. In containers or SSH sessions without a user manager, use the
  manual path in §5 — the daemon itself does not need systemd.
- Rust toolchain; build the workspace once: `cargo build --release --workspace`
  → binaries `target/release/ds`, `ds-daemon`, `ds-vdev`, `ds-vdevctl`, `ds-focus`.
- `ds` finds `ds-daemon` next to its own binary, else via `$DS_DAEMON_BIN`, else on `PATH`
  (see the `ds daemon start` error text: `cannot spawn … (set DS_DAEMON_BIN or put
  ds-daemon next to the ds binary)`). Keep them together (same directory) or export
  `DS_DAEMON_BIN=/abs/path/to/ds-daemon`.

## 3. Build and install

```sh
cargo build --release --workspace
# make ds-daemon available at the path the systemd unit expects:
sudo cargo install --locked --path crates/ds-daemon --root /usr/local   # -> /usr/local/bin/ds-daemon
# or keep the build tree and point the unit at it with a drop-in (see §7)
```

The unit names `/usr/local/bin/ds-daemon` explicitly — the alpha has no installer
(AppImage/distro packages are DS-083). To use a different binary without editing the unit
file:

```sh
systemctl --user edit deckstack.service
#   [Service]
#   ExecStart=
#   ExecStart=/home/<you>/repos/deckstack/target/release/ds-daemon run --device ${DECKSTACK_DEVICE} --data-dir %t/deckstack
systemctl --user daemon-reload
```

## 4. Paths and files

| Path | Mode | Meaning |
|---|---|---|
| `$XDG_RUNTIME_DIR/deckstack/` | 0700 | data dir (default); fallback `~/.local/state/deckstack/` when `XDG_RUNTIME_DIR` is unset |
| `<data-dir>/daemon.sock` | 0600 | NDJSON control socket (all `ds` verbs) — the only socket the daemon binds in this tree |
| `<data-dir>/stream.sock` | 0600 | DSF1 binary frame stream — **contract-defined, not yet bound by the daemon in this tree** (§1) |
| `<data-dir>/token` | 0600 | install token, `admin` scope — **never print or log it** |
| `<data-dir>/tokens.json` | 0600 | scoped tokens minted by `ds token issue` (verb pending, §10) |
| `<data-dir>/daemon.pid` | 0600 | pid of the running daemon; removed on clean exit, left behind on a crash (crash evidence, §12) |
| `<data-dir>/state.json` | — | durable state document (every mutation bumps `rev`); corrupt copies are quarantined to `state.json.corrupt-<ts>` (§12) |
| `<data-dir>/daemon.log` | — | stderr log when started via `ds daemon start` (detached) |
| `~/.config/deckstack/env` | — | optional env file for the systemd unit (`DECKSTACK_DEVICE=…`) |
| `~/.config/systemd/user/deckstack.service` | — | the user unit (installed from `packaging/systemd/deckstack.service`) |

Override the locations per invocation with the global flags `ds --socket PATH` and
`ds --data-dir PATH`. Note they are independent: `--data-dir` alone does **not** move the
socket (the socket default stays `$XDG_RUNTIME_DIR/deckstack/daemon.sock`), so a second,
isolated instance needs both:
`ds --data-dir /tmp/ds-test --socket /tmp/ds-test/daemon.sock daemon start --device none`.

## 5. First start — virtual device (no hardware)

```sh
ds-vdev --socket /tmp/vdev.sock &                          # virtual MasterHub (Ctrl-C or kill to stop)
ds daemon start --device virtual:/tmp/vdev.sock            # detached; writes <data-dir>/daemon.log
# -> ok running pid=2708732 rev=0 device=virtual:/tmp/vdev.sock socket=/run/user/1000/deckstack/daemon.sock   [exit 0]
ds daemon status                                           # same line, exit 0
ds daemon status --json
# -> {"device":"virtual:…","device_status":"connected virtual:…","pid":…,"pid_file":…,
#     "protocol":1,"rev":0,"running":true,"server":"ds-daemon/0.1.0","socket":…,"uptime_ms":…}
ds set /controls/keys/0/image --json '{"kind":"color","value":"#3cc86e"}'
# -> {"ok": true, "rev": 1}  (or human: ok rev=1)
ds get /controls/keys/0 --json
# -> {"path":"/controls/keys/0","rev":1,"value":{"image":{"kind":"color","value":"#3cc86e"}}}
ds-vdevctl --socket /tmp/vdev.sock frames key:0            # frame reached the device:
# -> key:0 c6ff9cf8…7fee 771B seq=10   (sha256 + bytes of the reassembled frame)
ds watch /controls --ndjson --max-events 1 &               # then in another shell: ds set …
# -> {"op":"event","path":"/controls/keys/2/label","rev":3,"value":"x"}
ds daemon stop                                             # admin-scope control op
# -> ok stopping   [exit 0]; afterwards daemon.sock + daemon.pid are gone,
#    ds daemon status exits 6 (see §14)
```

- `ds daemon start` waits up to 5 s for the socket + a successful `daemon status` op;
  on timeout it prints the hint to read `<data-dir>/daemon.log`.
- `ds daemon start --foreground` runs the daemon in the terminal (what systemd does).
- Without any device: `ds daemon start --device none`. `--device auto` currently maps to
  `none` in the CLI (`ds daemon start --help`: "auto maps to none in phase 1"); real
  hardware uses `hid:` URIs (§6).
- Empty state is the invariant: on a fresh data dir `/controls/keys` does not exist yet —
  `ds get /controls/keys` exits 2 `E_PATH` until the first `set` creates it.

## 6. Real hardware (Linux) — **[hardware run]**

Device addressing is a URI (`crates/ds-device/src/uri.rs`): `hid:` opens every attached
MasterHub module, `hid:<os-path>` (e.g. `hid:/dev/hidraw0`, `hid:1-2`) exactly one.

```sh
# 1. permissions (once, as root) — packaging/udev/99-deckstack.rules, ticket DS-025:
sudo install -Dm644 packaging/udev/99-deckstack.rules /etc/udev/rules.d/99-deckstack.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
ls -l /dev/hidraw*                 # uaccess nodes carry an ACL for the logged-in user
# group fallback on systems without logind/uaccess: the commented GROUP="deckstack",
# MODE="0660" variant in the rules file, then: sudo groupadd -r deckstack &&
# sudo usermod -aG deckstack "$USER"  (re-login)
# 2. start the daemon against the hardware:
ds daemon start --device hid:
# or via systemd: DECKSTACK_DEVICE=hid: in ~/.config/deckstack/env, then
#   systemctl --user daemon-reload && systemctl --user restart deckstack.service
ds daemon status --json            # "device_status":"connected hid:…" when open
```

Without the udev rule the `hid:` open fails with `E_DEVICE` (CLI exit 7) — check
`ls -l /dev/hidraw*` first when diagnosing (`docs/40-platforms.md` §1.1). The software-mode
handshake and the forbidden-command safety rules are enforced inside `ds-device`/`ds-protocol`
(DS-022/DS-029, `docs/20-security.md`); the operator never sends raw HID reports.
`ds-vdev` needs none of this — the virtual device talks over a user-owned Unix socket,
which is why CI and this runbook use it.

## 7. systemd user unit lifecycle

```sh
install -Dm644 packaging/systemd/deckstack.service ~/.config/systemd/user/deckstack.service
systemctl --user daemon-reload
systemctl --user enable --now deckstack.service     # start now + on login
systemctl --user status deckstack.service
systemctl --user restart deckstack.service          # clean restart (SIGTERM, handback, start)
systemctl --user stop deckstack.service             # clean SIGTERM stop
systemctl --user disable deckstack.service          # do not start on login
```

- The unit runs `ds-daemon run --device ${DECKSTACK_DEVICE} --data-dir %t/deckstack` in
  the foreground (`Type=simple`), `Restart=on-failure` with `RestartSec=2s`,
  `WantedBy=default.target`. `%t/deckstack` is exactly the daemon's default data dir
  (`$XDG_RUNTIME_DIR/deckstack`), so CLI and unit share one socket and token.
- Device is configuration, not code: the default is `none` (set by `Environment=` in the
  unit). To change it, create `~/.config/deckstack/env` with one line, e.g.
  `DECKSTACK_DEVICE=virtual:/tmp/vdev.sock` or `DECKSTACK_DEVICE=hid:`, then
  `systemctl --user daemon-reload && systemctl --user restart deckstack.service`.
  (systemd has no shell-style `${VAR:-default}` in `ExecStart=` — unset variables expand
  to empty — so the default comes from `Environment=` and the env file overrides it.)
- Validate the unit file on any host: `systemd-analyze --user verify <path>/deckstack.service`.
  On a host where `/usr/local/bin/ds-daemon` is not installed yet, verify reports only
  `Command /usr/local/bin/ds-daemon is not executable: …` and exits 1 — install/build the
  binary (§3) or add the drop-in above; every other directive verifies clean.
- Restart policy: a crash (kill -9, OOM) is restarted automatically; a clean
  `ds daemon stop` / SIGTERM exit is not. `systemctl --user reset-failed deckstack.service`
  clears a failed state before a manual start.

## 8. Logs

- systemd unit: `journalctl --user -u deckstack.service` (`-f` to follow, `-n 200`,
  `--since "10 min ago"`). One JSON object per line
  (`{"ts_ms":…,"level":…,"target":…,"msg":…}`).
- manual start: `tail -f <data-dir>/daemon.log` (same JSON lines; the detached
  `ds daemon start` redirects the daemon's stderr there).
- Look for, at startup: `starting: data_dir=… socket=… device=…`, `connected to device …`,
  `listening on <socket> (rev=…, server=ds-daemon/0.1.0)`. At stop: the guard's handback
  lines (`handback: sent …`, `device handed back, transport closed`) and exit 0. On crash
  recovery: `previous run (pid …) did not exit cleanly (stale daemon.pid) — recovering: …`
  (target `ds-daemon::guard`, level `warn`). On state damage:
  `state file was corrupt (…); quarantined to Some("…/state.json.corrupt-<ts>"), starting empty`.

## 9. Status, health checks, telemetry

```sh
ds daemon status            # ok running pid=… rev=… device=… socket=…   (exit 0; exit 6 when down)
ds status                   # same + client=ds/0.1.0
ds daemon status --json     # {"running":true,"pid":…,"rev":…,"device":…,"device_status":"connected …",…}
ds get /meta --json         # {"daemon_pid":…,"protocol":1,"rev":…,"started_at":"…Z","version":1}
ds get /telemetry --json    # non-durable, sampled ≤ 1 Hz, never bumps rev: {"path":"/telemetry","value":{…}}
ds stream stats <target>    # /telemetry/streams/<target> (keys|key:0-14|dial); E_PATH exit 2 until a stream ran
ds stream stop <target>     # ok target=<t> stopped=true
```

- `/telemetry/*` reads never bump `rev` and carry no `rev` (S1); frames never enter the
  JSON state document.
- `ds daemon status` with the daemon down exits **6** (`E_UNREACHABLE`) and prints the
  exact command to use:
  `ds: daemon unreachable at <socket> — start it with: ds daemon start  [exit 6]`.

## 10. Token and auth — read once, then never print it

- The install token lives at `<data-dir>/token`, mode 0600, 64 chars, generated randomly
  at first start and **stable across restarts** (per-run tokens would break the stateless
  CLI). `ds` reads it automatically from `<socket dir>/token` (`--token-file PATH`
  overrides). **Never `cat` it into a log, chat, or terminal transcript** — treat it like
  a password. Inspection without revealing it: `ls -l <data-dir>/token` (mode must be
  0600) and `wc -c < <data-dir>/token` (must be ≥ 16).
- Bad/missing token → `ds: bad token  [exit 4]` (`E_AUTH`; `E_SCOPE`/`E_CAP` also exit 4).
  If the file was deleted, restart the daemon to regenerate it — do not hand-write one
  shorter than 16 chars (the daemon refuses to start: `install token shorter than 16 chars`).
- Scoped tokens (`ds token issue --scope read|mutate|admin`, `ds token list`,
  `ds token revoke <id>`) are **pending their ticket**: today the verb exits 1
  ("not implemented in phase 1"). The design — instant revocation for new connections,
  ≤ 5 s grace only for in-flight requests on an established connection (T9/S12) — is in
  `docs/contracts/ipc.md` §3 and `docs/20-security.md`. Until then, share access by
  copying the install token file between trusted hosts (0600), never by printing it.

## 11. Restart and upgrade

```sh
# restart (no `ds daemon restart` verb exists):
systemctl --user restart deckstack.service     # unit path
# or manual: ds daemon stop && ds daemon start --device <same-uri>
ds daemon status --json                        # rev continues from state.json (e.g. rev=3 after a crash+restart)
```

- `state.json` persists across restarts; `rev` keeps counting; the token is stable.
  On clean exit the daemon removes `daemon.sock` and `daemon.pid` and exits 0.
- **Upgrade = install the new release, then restart.** There is no `ds migrate` and no
  state migration in the alpha (scope: `docs/00-scope.md`) — a new release starts from the
  same `state.json` if the schema is compatible, else the corrupt-state quarantine path
  applies (§12). Procedure: `cargo build --release` (or install the new package) →
  `sudo cargo install --locked --path crates/ds-daemon --root /usr/local` →
  `systemctl --user daemon-reload && systemctl --user restart deckstack.service` →
  verify with `ds daemon status --json` (`server` field shows the new version) and
  `journalctl --user -u deckstack.service -n 50`.

## 12. Recovery after a crash

| Situation | What to do |
|---|---|
| Stale `daemon.sock`/`daemon.pid` after `kill -9` (observed: pid file says e.g. 2708967, socket file still present, `ds daemon status` → exit 6) | The next start recovers by itself: the guard detects the dead pid and logs `previous run (pid …) did not exit cleanly (stale daemon.pid) — recovering: re-handshake on connect, state reloaded from the atomically committed document`. Just start: `ds daemon start …` (or `systemctl --user start deckstack.service`; `Restart=on-failure` usually already did). Manual cleanup if you want to be sure the pid is dead: `kill -0 $(cat <data-dir>/daemon.pid) 2>/dev/null || rm <data-dir>/daemon.sock <data-dir>/daemon.pid` |
| `ds daemon status` → exit 6 `E_UNREACHABLE` | daemon down; socket file may be stale — remove as above if the pid is dead, then start (§5/§7) |
| Daemon crashed hard (critical-task panic) | the guard runs the device handback, exits with code **8** (`E_INTERNAL`), and leaves `daemon.pid` behind as crash evidence; systemd `Restart=on-failure` restarts it, or start manually; the `deck/system/crash-recovered` topic exists in the event grammar (`docs/contracts/actions.md`) |
| Unit in failed state / restart loop | `systemctl --user reset-failed deckstack.service && systemctl --user start deckstack.service`; check `journalctl --user -u deckstack.service -n 50` and `NRestarts` |
| `state.json` corrupt (observed: daemon starts empty, `rev=0`, logs `state file was corrupt (…); quarantined to Some("<data-dir>/state.json.corrupt-<ts>"), starting empty`) | the corrupt file is kept as `state.json.corrupt-<ts>` — inspect it, restore from your backup (§13) if needed: stop the daemon, `cp <backup> <data-dir>/state.json`, start again |
| Data dir missing (`$XDG_RUNTIME_DIR` unset, e.g. plain SSH) | the daemon falls back to `~/.local/state/deckstack/`; point the CLI the same way: `ds --data-dir ~/.local/state/deckstack --socket ~/.local/state/deckstack/daemon.sock …` |
| Device busy / hardware error **[hardware run]** | `ds daemon status --json` → `device_status` says so; CLI exit 7 `E_DEVICE` — stop the vendor app; check `ls -l /dev/hidraw*` (§6) |

## 13. Backup and restore of the state document

`state.json` is the only durable file that matters (token/tokens.json are regenerable
credentials; `daemon.log` is diagnostics):

```sh
# backup (daemon may be running — the file is replaced atomically on every commit):
cp -a <data-dir>/state.json ~/backups/deckstack-state-$(date -u +%Y%m%dT%H%M%SZ).json
# restore: stop the daemon, replace, start:
ds daemon stop                                    # or: systemctl --user stop deckstack.service
cp ~/backups/deckstack-state-<ts>.json <data-dir>/state.json
systemctl --user start deckstack.service          # or: ds daemon start --device <same-uri>
ds daemon status --json                           # rev continues from the restored document
```

Quarantine files (`state.json.corrupt-<ts>`) are safe to keep as forensic copies; delete
them once the state is healthy.

## 14. Troubleshooting

| Symptom | Diagnose | Fix |
|---|---|---|
| `systemctl --user start deckstack` fails instantly | `systemctl --user status deckstack.service`; `journalctl --user -u deckstack.service -n 50` | binary missing → §3/drop-in; bad `DECKSTACK_DEVICE` → fix `~/.config/deckstack/env` |
| verify complains only `Command … is not executable` | `ls -l /usr/local/bin/ds-daemon` | install/build the binary or use the `systemctl --user edit` drop-in (§3); unit is otherwise valid |
| `ds …` exits 6 `E_UNREACHABLE` | `ds daemon status`; `ls $XDG_RUNTIME_DIR/deckstack/` | start the daemon (§5/§7); remove stale socket if the pid is dead (§12) |
| `ds stream push …` exits 6 with "stream.sock missing" | `ls <data-dir>/stream.sock` | expected in this tree — the daemon does not bind the frame socket yet (§1); use `ds set … --file/--json` for images |
| `ds …` exits 4 `E_AUTH` (`ds: bad token  [exit 4]`) | `ls -l <data-dir>/token` (mode 0600, ≥16 chars) | do not print the token; if the file was deleted, restart the daemon to regenerate it |
| `ds token …`, `ds emit …`, `ds plugin …` exit 1 "not implemented in phase 1" | `ds --help` | later-phase verbs, pending their tickets (§1) — not a misconfiguration |
| `ds get /controls/keys/99` | — | `ds: path not found: /controls/keys/99  [exit 2]` (`E_PATH`); on a fresh start `/controls/keys` itself is absent until the first `set` (empty state) — use `ds describe <path>` |
| `ds watch … --json` exits 1 | — | streaming verbs accept only `--ndjson`: `--json is not valid for streaming verbs; use --ndjson  [exit 1]` (`docs/contracts/cli.md` §5) |
| daemon running but no device response | `ds daemon status --json` → `device`/`device_status` | with `virtual:` URIs: is `ds-vdev --socket …` alive? `ds-vdevctl --socket /tmp/vdev.sock ping` / `writes`. With `hid:`: §6 udev check |
| high CPU / restart loop | `systemctl --user status deckstack.service` (check `NRestarts`), logs §8 | fix the config in `~/.config/deckstack/env`; `reset-failed` then start |
| want a second, isolated instance | — | `ds --data-dir /tmp/ds-test --socket /tmp/ds-test/daemon.sock daemon start --device none` (both flags — §4) |

## 15. Exit codes (quick reference)

`0` ok · `1` usage/schema (incl. `--json` on a streaming verb; later-phase verbs; no `migrate` verb) ·
`2` `E_PATH` · `3` `E_CONFLICT` · `4` `E_AUTH`/`E_SCOPE`/`E_CAP` · `5` `E_LIMIT` ·
`6` `E_UNREACHABLE` (daemon down or stream socket missing) · `7` `E_DEVICE` ·
`8` `E_INTERNAL`/`E_VERSION` (also the daemon's crash exit code). Full table with exact
error text: `docs/contracts/ipc.md` §4 and `docs/contracts/cli.md` §5.

## 16. Uninstall

```sh
systemctl --user disable --now deckstack.service
rm ~/.config/systemd/user/deckstack.service && systemctl --user daemon-reload
sudo rm /etc/udev/rules.d/99-deckstack.rules && sudo udevadm control --reload-rules   # if installed
sudo rm /usr/local/bin/ds-daemon                                                       # if installed
rm -rf $XDG_RUNTIME_DIR/deckstack        # sockets/pid/token; keeps nothing durable there
# durable data lives only in state.json — back it up first if you want to keep it (§13),
# then remove ~/.local/state/deckstack (the no-XDG fallback dir) if you used it:
rm -rf ~/.local/state/deckstack
```
