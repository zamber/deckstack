# Runbook — deckstack daemon lifecycle (phase 1)

Operator-facing lifecycle for the `ds-daemon` process in phase 1 (virtual device or no
hardware; udev/hardware rules land in phase 2, DS-025). Everything here is reachable from
the `ds` CLI — there is no web UI. Design references: `docs/components/daemon.md`,
`docs/contracts/ipc.md`, `docs/contracts/cli.md`.

## 1. Prerequisites

- Linux with a systemd **user** session (Bazzite/Fedora default): `echo $XDG_RUNTIME_DIR`
  must print `/run/user/<uid>` and `systemctl --user is-system-running` must answer
  `running`/`degraded`. In containers or SSH sessions without a user manager, use the
  manual path in §4 — the daemon itself does not need systemd.
- Rust toolchain; build the workspace once: `cargo build --release --workspace`
  → binaries `target/release/ds`, `ds-daemon`, `ds-vdev`, `ds-vdevctl`.
- `ds` finds `ds-daemon` next to its own binary, else via `$DS_DAEMON_BIN`, else on `PATH`
  (see `ds daemon start` error text). Keep them together (same directory) or export
  `DS_DAEMON_BIN=/abs/path/to/ds-daemon`.

## 2. Paths and files

| Path | Mode | Meaning |
|---|---|---|
| `$XDG_RUNTIME_DIR/deckstack/` | 0700 | data dir (default); fallback `~/.local/state/deckstack/` when `XDG_RUNTIME_DIR` is unset |
| `<data-dir>/daemon.sock` | 0600 | NDJSON control socket (all `ds` verbs) |
| `<data-dir>/stream.sock` | 0600 | DSF1 binary frame stream |
| `<data-dir>/token` | 0600 | install token, `admin` scope — **never print or log it** |
| `<data-dir>/tokens.json` | 0600 | scoped tokens minted by `ds token issue` |
| `<data-dir>/daemon.pid` | 0600 | pid of the running daemon; removed on clean exit |
| `<data-dir>/state.json` | — | durable state document (every mutation bumps `rev`) |
| `<data-dir>/daemon.log` | — | stderr log when started via `ds daemon start` (detached) |
| `~/.config/deckstack/env` | — | optional env file for the systemd unit (`DECKSTACK_DEVICE=…`) |
| `~/.config/systemd/user/deckstack.service` | — | the user unit (installed from `packaging/systemd/deckstack.service`) |

Override the locations per invocation with the global flags `ds --socket PATH` and
`ds --data-dir PATH` (e.g. a second test instance:
`ds --data-dir /tmp/ds-test daemon start --device none`).

## 3. Build and install

```sh
cargo build --release --workspace
# make ds-daemon available at the path the unit expects:
sudo cargo install --locked --path crates/ds-daemon --root /usr/local   # -> /usr/local/bin/ds-daemon
# or keep the build tree and point the unit at it with a drop-in (see §5)
```

The unit names `/usr/local/bin/ds-daemon` explicitly — phase 1 has no installer
(AppImage is DS-083). To use a different binary without editing the unit file:

```sh
systemctl --user edit deckstack.service
#   [Service]
#   ExecStart=
#   ExecStart=/home/<you>/repos/deckstack/target/release/ds-daemon run --device ${DECKSTACK_DEVICE} --data-dir %t/deckstack
systemctl --user daemon-reload
```

## 4. Manual run (no systemd) — also the no-hardware test path

```sh
ds-vdev --socket /tmp/vdev.sock &                          # virtual MasterHub (Ctrl-C or kill to stop)
ds daemon start --device virtual:/tmp/vdev.sock            # detached; writes <data-dir>/daemon.log
ds daemon status                                           # ok running pid=… rev=… device=… socket=…
ds set /controls/keys/0/image --json '{"kind":"color","value":"#3cc86e"}'
ds get /controls/keys/0 --json
ds-vdevctl --socket /tmp/vdev.sock frames keys             # sha256+bytes: frame reached the device
ds daemon stop                                             # admin-scope control op; waits for socket removal
```

- `ds daemon start` waits up to 5 s for the socket + a successful `daemon status` op;
  on timeout it prints the hint to read `<data-dir>/daemon.log`.
- `ds daemon start --foreground` runs the daemon in the terminal (what systemd does).
- `ds daemon status` with the daemon down exits **6** (`E_UNREACHABLE`) and prints the
  exact `ds daemon start` command to use.
- Without any device: `ds daemon start --device none` (or `--device auto`, currently
  mapped to `none` until DS-021).

## 5. systemd user unit

```sh
install -Dm644 packaging/systemd/deckstack.service ~/.config/systemd/user/deckstack.service
systemctl --user daemon-reload
systemctl --user enable --now deckstack.service     # start now + on login
systemctl --user status deckstack.service
systemctl --user stop deckstack.service             # clean SIGTERM stop
systemctl --user disable deckstack.service          # do not start on login
```

- The unit runs `ds-daemon run --device ${DECKSTACK_DEVICE} --data-dir %t/deckstack` in
  the foreground (`Type=simple`), `Restart=on-failure` with `RestartSec=2s`,
  `WantedBy=default.target`. `%t/deckstack` is exactly the daemon's default data dir
  (`$XDG_RUNTIME_DIR/deckstack`), so CLI and unit share one socket and token.
- Device is configuration, not code: the default is `none` (set by `Environment=` in the
  unit). To change it, create `~/.config/deckstack/env` with one line, e.g.
  `DECKSTACK_DEVICE=virtual:/tmp/vdev.sock`, then `systemctl --user daemon-reload &&
  systemctl --user restart deckstack.service`. (systemd has no shell-style
  `${VAR:-default}` in `ExecStart=` — unset variables expand to empty — so the default
  comes from `Environment=` and the env file overrides it.)
- Validate the unit file on any host: `systemd-analyze --user verify <path>/deckstack.service`.
  On a host where `/usr/local/bin/ds-daemon` is not installed yet, verify reports only
  `Command /usr/local/bin/ds-daemon is not executable: No such file or directory` and
  exits 1 — install/build the binary (§3) or add the drop-in above; every other
  directive verifies clean.
- Restart policy: a crash (kill -9, OOM) is restarted automatically; a clean
  `ds daemon stop` / SIGTERM exit is not. `systemctl --user reset-failed deckstack.service`
  clears a failed state before a manual start.

## 6. Logs

- systemd unit: `journalctl --user -u deckstack.service` (`-f` to follow, `-n 200`,
  `--since "10 min ago"`). One JSON object per line
  (`{"ts_ms":…,"level":…,"target":…,"msg":…}`).
- manual start: `tail -f <data-dir>/daemon.log` (same JSON lines; the detached
  `ds daemon start` redirects the daemon's stderr there).
- Look for `listening on …` at startup, `shutdown requested; draining` /
  `stopped cleanly (exit 0)` at stop, and `state file was corrupt … quarantined` if
  `state.json` was damaged.

## 7. Token and auth (read once, then never print it)

- The install token lives at `<data-dir>/token`, mode 0600, generated randomly at first
  start and **stable across restarts** (per-run tokens would break the stateless CLI).
  `ds` reads it automatically (`--token-file PATH` overrides). **Never `cat` it into a
  log, chat, or terminal transcript** — treat it like a password. `ds token list` shows
  ids/scopes only.
- Bad/missing token → `E_AUTH`, exit 4. Scoped tokens: `ds token issue --scope read|mutate`,
  `ds token revoke <id>` (revocation is instant for new connections).

## 8. Clean restart and state

```sh
systemctl --user restart deckstack.service          # or: ds daemon stop && ds daemon start
ds daemon status --json                             # rev continues from state.json
```

`state.json` persists across restarts; `rev` keeps counting. On clean exit the daemon
removes `daemon.sock` and `daemon.pid` and exits 0.

## 9. Recovery

| Situation | What to do |
|---|---|
| Stale `daemon.sock`/`daemon.pid` after `kill -9` (unit: `Restart=on-failure` usually already handled it) | `kill -0 $(cat <data-dir>/daemon.pid) 2>/dev/null` — if dead: `rm <data-dir>/daemon.sock <data-dir>/daemon.pid`, then `systemctl --user start deckstack.service` or `ds daemon start` |
| `ds daemon status` → exit 6 `E_UNREACHABLE` | daemon down; socket file may be stale — remove as above, then start |
| Unit in failed state | `systemctl --user reset-failed deckstack.service && systemctl --user start deckstack.service` |
| `state.json` corrupt | daemon quarantines it to `state.json.corrupt-<ts>`, starts empty, logs loudly — check `journalctl --user -u deckstack.service` / `daemon.log`, restore from the quarantine file if needed |
| Port/socket dir missing (`$XDG_RUNTIME_DIR` unset, e.g. plain SSH) | the daemon falls back to `~/.local/state/deckstack/`; point the CLI the same way: `ds --data-dir ~/.local/state/deckstack …` |
| Device busy / hardware error | `ds daemon status` shows the device state; exit 7 `E_DEVICE` — stop the vendor app |

## 10. Troubleshooting

| Symptom | Diagnose | Fix |
|---|---|---|
| `systemctl --user start deckstack` fails instantly | `systemctl --user status deckstack.service`; `journalctl --user -u deckstack.service -n 50` | binary missing → §3/drop-in; bad `DECKSTACK_DEVICE` → fix `~/.config/deckstack/env` |
| verify complains only `Command … is not executable` | `ls -l /usr/local/bin/ds-daemon` | install/build the binary or use the `systemctl --user edit` drop-in (§3); unit is otherwise valid |
| `ds …` exits 6 `E_UNREACHABLE` | `ds daemon status`; `ls $XDG_RUNTIME_DIR/deckstack/` | start the daemon (§4/§5); remove stale socket if pid is dead (§9) |
| `ds …` exits 4 `E_AUTH` | `ls -l <data-dir>/token` (mode must be 0600, ≥16 chars) | do not print the token; if the file was deleted, restart the daemon to regenerate it |
| daemon running but no device response | `ds daemon status --json` → `device`/`device_status` | with `virtual:` URIs: is `ds-vdev --socket …` alive? `ds-vdevctl --socket /tmp/vdev.sock writes` |
| high CPU / restart loop | `systemctl --user status deckstack.service` (check `NRestarts`), logs §6 | fix the config in `~/.config/deckstack/env`; `reset-failed` then start |
| want a second, isolated instance | — | `ds --data-dir /tmp/ds-test --socket /tmp/ds-test/daemon.sock daemon start --device none` |

## 11. Exit codes (quick reference)

`0` ok · `1` usage/schema · `2` `E_PATH` · `3` `E_CONFLICT` · `4` `E_AUTH`/`E_SCOPE`/`E_CAP` ·
`5` `E_LIMIT` · `6` `E_UNREACHABLE` (daemon down) · `7` `E_DEVICE` · `8` `E_INTERNAL`/`E_VERSION`.
Full table: `docs/contracts/ipc.md` §4.
