# Agents — from zero to a working deck session

The single onboarding route for a coding agent (or an agent operator) that wants to drive
a MasterHub deck through deckstack. Everything on this page is reachable from the `ds`
CLI with no UI (R1): install → permissions → daemon → MCP bridge → tools → errors.

- **Alpha status, Linux only.** The alpha runs on Linux. Deliberately absent: any web UI,
  a hosted service, plugins published on PyPI (`clients/ds-py` is copied, not installed),
  Windows/macOS. The daemon starts from an **empty state** (no presets/sample decks) and
  there is no `ds migrate` verb.
- **Normative references:** the tool/protocol spec is `docs/components/mcp-bridge.md`, the
  operator guide is `docs/runbook.md`, the contracts are `docs/contracts/{cli,ipc,state}.md`
  (schemas in `schemas/`). This page is a route, not a second contract — where they
  disagree, the contracts win and the gap gets reported (see §6).
- **Executable proof of this route:** `python tests/conformance/agent-onboarding/run.py`
  (boots ds-vdev + ds-daemon + ds-mcp, runs initialize / tools/list / deck_status /
  deck_set / deck_get, then the same three through `clients/ds-py`; exit 0) and
  `python examples/agents/mcp/demo.py` (the ~40-line demo in §4).

## 1. Prerequisites (real deck)

1. **Linux**, deck plugged in (Cooler Master MasterHub modules, VID `0x2516`; PIDs `01f6`
   base, `01fe` keys, `0202` knobs, `0204` rollers, `0206` dial).
2. **udev rule installed** (once, as root) — `packaging/udev/99-deckstack.rules`:
   ```sh
   sudo install -Dm644 packaging/udev/99-deckstack.rules /etc/udev/rules.d/99-deckstack.rules
   sudo udevadm control --reload-rules && sudo udevadm trigger    # or re-login
   ```
   The rule sets `TAG+="uaccess"` **and** `RUN{builtin}+="uaccess"`. The `RUN{builtin}`
   part is not optional: the stock `73-seat-late.rules` only schedules the ACL builtin for
   devices that already carry the tag when it runs, and a later-numbered file (this one is
   `99-`) must schedule the builtin itself — otherwise the tag is set but no ACL is applied
   and the node stays `root:root 0600` (observed on systemd 258). Never rely on
   `TAG+="uaccess"` alone from a late file, and never use `MODE="0666"`. Without
   logind/uaccess, use the commented group fallback (`GROUP="deckstack", MODE="0660"` +
   `sudo groupadd -r deckstack && sudo usermod -aG deckstack $USER`, re-login).
   Verify: `ls -l /dev/hidraw*` — uaccess nodes carry an ACL for your user;
   `udevadm info /dev/hidraw<N>` shows the `uaccess` tag.
3. **The vendor app must not be running.** The HubDeck/MasterHub desktop app claims the
   deck's USB interfaces through libusb; while it holds them the hidraw nodes are absent
   and the daemon cannot open the device. Stop it before starting the daemon on `hid:`
   (`docs/40-platforms.md` §1.1).

No hardware yet? Skip to §3 and use the virtual device (`virtual:`) — everything except
the physical pixels is identical.

## 2. Install

Built by `sh packaging/release/release.sh --version 0.1.0 --out <dir>` (DS-083): a
deterministic `deckstack-0.1.0-linux-<arch>.tar.gz` + `SHA256SUMS`, and an AppImage when
`appimagetool` is on PATH.

**Release tar (recommended for agents):**
```sh
tar -xzf deckstack-0.1.0-linux-x86_64.tar.gz -C ~/Applications   # -> ~/Applications/deckstack-0.1.0/
export PATH="$HOME/Applications/deckstack-0.1.0/bin:$PATH"          # add to ~/.bashrc
~/Applications/deckstack-0.1.0/bin/ds --version                    # ds 0.1.0
```
Layout: `bin/` (ds, ds-daemon, ds-mcp, ds-focus, ds-vdev, ds-vdevctl), `share/deckstack/schemas/`
(the normative JSON Schemas), `share/deckstack/systemd/deckstack.service`,
`share/deckstack/udev/99-deckstack.rules`, `share/doc/deckstack/` (runbook, LICENSE, NOTICE),
`VERSION`. (A ready-made unpack of exactly this layout exists on this machine at
`/home/catwars/Applications/deckstack-0.1.0`.) To install system-wide into a prefix
instead: `sh packaging/linux/install.sh --src ~/Applications/deckstack-0.1.0 --prefix /usr/local`
(also installs the systemd user unit and the udev rule via sudo).

**AppImage:** `deckstack-0.1.0-linux-x86_64.AppImage`. AppImages need FUSE to run
directly; **without FUSE** use:
```sh
./deckstack-0.1.0-linux-x86_64.AppImage --appimage-extract-and-run --version
# or extract once: ./…AppImage --appimage-extract && ./squashfs-root/usr/bin/ds --version
```
The AppImage's `AppRun` execs `usr/bin/ds`; the other binaries sit next to it, so for the
daemon/bridge use the extracted `squashfs-root/usr/bin/ds-mcp` etc.

Keep `ds` and `ds-daemon` in the same directory (or set `DS_DAEMON_BIN=/abs/path/to/ds-daemon`):
`ds daemon start` spawns the daemon from beside its own binary, else `$DS_DAEMON_BIN`, else
`PATH`.

## 3. Start the daemon

Where things live (defaults; `ds --socket PATH --data-dir DIR` and `ds-mcp --socket/--data-dir`
override per invocation):

| Path | Meaning |
|---|---|
| data dir | `$XDG_RUNTIME_DIR/deckstack` (fallback `~/.local/state/deckstack`), dir 0700 |
| `<data-dir>/daemon.sock` | NDJSON control socket, 0600 — what `ds`/`ds-mcp`/`ds-py` connect to |
| `<data-dir>/token` | per-install auth token, 0600, ≥16 chars, generated at first start — never print/log it |
| `<data-dir>/daemon.pid`, `<data-dir>/daemon.log` | pid file + daemon stderr log |
| `<data-dir>/state.json` | the durable state document (back this up; `state.json.corrupt-<ts>` files are quarantined forensic copies) |
| `<data-dir>/stream.sock` | DSF1 frame-stream socket (not bound by the phase-1 daemon — see §6) |

**Virtual device (no hardware, what CI and the examples use):**
```sh
ds-vdev --socket /tmp/vdev.sock &                        # virtual MasterHub
ds daemon start --device virtual:/tmp/vdev.sock          # detached; logs to <data-dir>/daemon.log
```
**Real deck:**
```sh
ds daemon start --device hid:                            # every attached module
# or one node: ds daemon start --device hid:/dev/hidraw0
```
(`hid:` opens every attached MasterHub module; `hid:<os-path>` exactly one. `--device auto`
maps to `none` in phase 1; `--device none` starts with no device.)

Then check, and optionally run it under systemd (`packaging/systemd/deckstack.service`,
`DECKSTACK_DEVICE=hid:` in `~/.config/deckstack/env`, `systemctl --user enable --now
deckstack.service` — see `docs/runbook.md` §7):
```sh
ds daemon status --json
# {"device":"virtual:…","device_status":"connected virtual:…","pid":…,"running":true,
#  "rev":0,"server":"ds-daemon/0.1.0","socket":…,…}
ds daemon stop                                            # clean stop; socket+pid file removed
```
`ds daemon start` waits ≤5 s for the socket + a successful status op; on timeout read
`<data-dir>/daemon.log`.

## 4. Connect an agent

### 4.1 MCP bridge: `ds-mcp` over stdio

`ds-mcp` is the MCP server for coding agents: one JSON-RPC 2.0 message per line on
stdin/stdout (NDJSON), tools = one daemon control-socket op each, auth via the same token
file. Flags: `ds-mcp [--socket PATH] [--data-dir PATH] [--token-file PATH]` — `--socket`
wins, else `--data-dir/daemon.sock`, else the default data dir; token defaults to
`<socket dir>/token` (0600). stdout carries protocol messages only (no logging, ever).

**Claude Code** — project config `.mcp.json` (or `claude mcp add deckstack -- <bin> --data-dir <dir>`):
```json
{
  "mcpServers": {
    "deckstack": {
      "command": "/home/YOU/Applications/deckstack-0.1.0/bin/ds-mcp",
      "args": ["--data-dir", "/home/YOU/.local/state/deckstack"]
    }
  }
}
```
**Claude Desktop** — same `{"mcpServers": …}` object in
`~/.config/Claude/claude_desktop_config.json` on Linux. A ready-to-edit copy of the snippet
lives at `examples/agents/mcp/mcp-servers.json`. Other MCP clients: any stdio client works —
register `command` + `args` as above; the spec of what the server speaks is
`docs/components/mcp-bridge.md`.

**Generic stdio client (any language), the whole handshake in one pipe:**
```sh
{ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"my-agent","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"deck_set","arguments":{"path":"/controls/keys/0/image","value":{"kind":"color","value":"#3cc86e"}}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"deck_get","arguments":{"path":"/controls/keys/0/image"}}}' \
  ; sleep 1; } | ds-mcp --data-dir ~/.local/state/deckstack
```
`initialize` → result with `protocolVersion "2024-11-05"` (pinned), `capabilities.tools`,
`serverInfo {name: "ds-mcp", version}`; then send the `notifications/initialized`
notification (never answered). `ping` → `{}`. A ~40-line runnable version of exactly this
is `examples/agents/mcp/demo.py` (boots its own vdev+daemon when no `--socket` is given).

### 4.2 Python alternative: `clients/ds-py`

Stdlib-only helper, optional sugar over the `ds` CLI (never reimplements daemon logic, never
opens the socket itself — one argv-only `ds` subprocess per call):
```python
import sys; sys.path.insert(0, "/path/to/deckstack/clients/ds-py")
from deckstack import DeckClient
with DeckClient(socket="…/daemon.sock", data_dir="…") as ds:   # or ds="/abs/path/to/ds"
    ds.status()                                   # ds daemon status --json
    rev = ds.set("/controls/keys/0/image", {"kind": "color", "value": "#3cc86e"})
    print(ds.get("/controls/keys/0/image"))       # {"path":…,"value":…,"rev":…}
```
See `clients/ds-py/README.md`; examples in `examples/agents/`.

## 5. The tool table (10 tools)

`docs/components/mcp-bridge.md` is **normative**; the authoritative argument schemas are the
`inputSchema`s returned by `tools/list` (each tool has `type: "object"`). Summary:

| MCP tool | Arguments | One-line example | `ds` equivalent | ds-py equivalent |
|---|---|---|---|---|
| `deck_status` | `{}` | `deck_status` → running/rev/pid/device/socket | `ds status` / `ds daemon status --json` | `client.status()` |
| `deck_get` | `path` req, `fields?`, `rev?` | `deck_get /controls/keys/0/image` | `ds get <path> [--fields a,b] [--rev N] --json` | `client.get(path, fields, rev)` |
| `deck_set` | `path` req, `value` req (any JSON), `if_rev?` | `deck_set /controls/keys/0/image {"kind":"color","value":"#3cc86e"}` | `ds set <path> --json '<v>' [--if-rev N]` (`--file f` for images) | `client.set(path, value, if_rev=…, file=…)` |
| `deck_bind` | `topic` req `^(deck\|app)/`, `do` ≥1 step objects req, `when?` | `deck_bind deck/control/keys/0/down do=[{"do":"set","path":"/controls/keys/5/image","value":{"kind":"color","value":"#ff8800"}}]` | `ds bind <topic> --do '<step>' [--when '<cond>']` → `bNNNN` | `client.bind(topic, step, when=…)` |
| `deck_unbind` | exactly one of `binding` `bNNNN` / `topic` | `deck_unbind {"binding":"b0001"}` | `ds unbind <id> \| --topic <topic>` | `client.unbind(binding_id=…)` / `topic=…` |
| `deck_emit` | `topic` req, `payload?` | `deck_emit app/demo/red {}` | `ds emit <topic> [payload]` (CLI verb refused exit 1 in phase 1 — but the MCP tool works: the bridge speaks the daemon `emit` op directly; reported gap, §6) | `client.emit(topic, payload)` |
| `deck_watch` | `path` req, `from_rev?`, `max_events?` 1..100000 | `deck_watch /controls/keys/*` → subscription `wN`, then `notifications/deck/event` per event, `notifications/deck/watch_closed` at the end (cancel via `notifications/cancelled`) | `ds watch <path> --ndjson` | `client.watch(path, max_events=…)` |
| `deck_stream_push` | `target` req (`keys\|dial\|key:0-14`), `file` req (png/jpeg/gif ≤16 MiB), `credits?` 1..1024 | `deck_stream_push {"target":"key:0","file":"pic.png"}` (phase 1: `E_UNREACHABLE` exit 6 — `stream.sock` is not bound yet; use `deck_set … value {"kind":"image"…}`/`--file` for static images) | `ds stream push <target> --file f` | `client.stream_push(target, file)` |
| `deck_plugin_list` | `{}` | `deck_plugin_list` (phase 1: honest `E_INTERNAL` exit 8 — plugins land in a later ticket) | `ds plugin list` (refused exit 1 in phase 1) | — |
| `deck_schema` | `name` req ∈ `state, ipc, stream-frame, plugin-manifest, binding` | `deck_schema {"name":"state"}` | `ds schema <name>` | `client.schema(name)` |

Tool results: `{"content":[{"type":"text","text":<pretty JSON>}], "structuredContent": <result object>, "isError": false}`.
Tool errors are results with `isError: true` and the stable `E_*` code + CLI exit code in the
text (e.g. `E_UNREACHABLE: daemon unreachable at <sock> … — start it with: ds daemon start [exit 6]`).
Protocol errors are JSON-RPC errors, not tool errors: malformed line `-32700`, not an object
`-32600`, unknown method `-32601`, unknown tool / bad arguments `-32602`. Per-call timeout 10 s
(`E_INTERNAL … exceeded the 10-second per-call timeout [exit 8]`).

## 6. Troubleshooting

`E_*` codes and CLI exits (`docs/contracts/ipc.md` §4, `docs/contracts/cli.md` §3):

| Code (exit) | Meaning | How to check / fix |
|---|---|---|
| `E_UNREACHABLE` (6) | daemon not running / socket missing | `ds daemon status`; `ls $XDG_RUNTIME_DIR/deckstack/`; start it (§3). MCP tool errors carry the exact `ds daemon start` command. After a crash a stale socket may remain — remove it if the pid is dead (`docs/runbook.md` §12). `deck_stream_push` also answers this while `stream.sock` is unbound (phase 1) |
| `E_AUTH`/`E_SCOPE`/`E_CAP` (4) | bad/missing token, scope, capability | `ls -l <data-dir>/token` (0600, ≥16 chars; never print it). If the file was deleted, restart the daemon to regenerate it. Exec steps need `/policy/exec/allow` − `deny` + admin scope |
| `E_PATH` (2) | path not found | On a fresh start `/controls/keys` does not exist until the first `set` — `ds describe <path>` shows schema + allowed verbs |
| `E_SCHEMA` (1) | value violates the schema | `ds schema state`, `ds describe <path>`; e.g. colour must match `^#[0-9a-fA-F]{6}$` |
| `E_CONFLICT` (3) | `if_rev` mismatch | payload carries `current_rev` — re-read, retry with the new rev |
| `E_LIMIT` (5) | size/rate/queue limit | request line ≤1 MiB, response ≤4 MiB, >1000 queued subscription events → close; stream credits |
| `E_DEVICE` (7) | hardware error, incl. permission failure and device busy | `ls -l /dev/hidraw*` (uaccess ACL present?); `udevadm info /dev/hidraw<N>`; rule installed + reloaded? **vendor app stopped?** `ds daemon status --json` → `device_status` |
| `E_VERSION`/`E_INTERNAL` (8) | protocol mismatch / bug | `deck_plugin_list` answers `E_INTERNAL [exit 8]` on the phase-1 daemon by design (plugins land later) — the bridge maps, it does not paper over |

Device permission failure, concretely: `ds daemon start --device hid:` fails with `E_DEVICE`
exit 7 when the udev rule is missing (nodes are `root:root 0600`) or the vendor app holds the
USB interfaces (hidraw nodes absent). Check `ls -l /dev/hidraw*`, `udevadm info`, reinstall
the rule (§1), stop the vendor app, replug, retry.

Daemon unreachable from an agent: every MCP tool answers `E_UNREACHABLE … ds daemon start
[exit 6]`; from `ds` the same exit 6 with the hint. Check `ds daemon status`, the socket file,
and `<data-dir>/daemon.log`.

MCP-level protocol errors (`-32700/-32600/-32601/-32602`) mean the client's framing is wrong,
not the deck: one JSON-RPC 2.0 object per line, `initialize` first, `notifications/initialized`
afterwards. stdout of `ds-mcp` is protocol-only — anything else is a bug.

**Known doc gaps (reported, not papered over):** `ds emit` / `ds plugin list` / `ds token …`
are refused exit 1 by the phase-1 CLI (`docs/runbook.md` §1), while the MCP tools speak the
daemon ops directly — `deck_emit` currently succeeds, `deck_plugin_list` answers
`E_INTERNAL [exit 8]`, `deck_stream_push` answers `E_UNREACHABLE [exit 6]` (no `stream.sock`
yet). Follow the bridge doc for the MCP surface.

## 7. Design constraints an agent must respect

- **No raw HID escape, ever.** There is no tool/verb/op that sends a raw HID report; the
  operator never does either. The software-mode handshake runs before any read on
  keys/dial, forbidden HID commands are unrepresentable and refused with zero bytes on the
  wire, and the dial is never backlight-0'd (mh-protocol safety, `docs/20-security.md`,
  machine-checked by `tests/conformance/safety/`). If you think you need raw HID, you are
  holding it wrong — use `set`/`stream`/bindings.
- **Exec is argv-only.** Binding `exec` steps are argv arrays with an absolute `argv[0]`
  that must be in `/policy/exec/allow` and not in `/policy/exec/deny` (default-deny list
  includes the shells and interpreters); cwd is confined to `/policy/fs/roots` + `/tmp`;
  no shell, no `eval` of config anywhere. `ds-mcp` never spawns processes; `deck_stream_push`
  reads its payload from a file path only.
- **Frames and telemetry never bump `rev` (S1).** Frame pushes at 32 fps cause zero rev
  increments; `get /telemetry/…` returns the current sample with **no rev**; only durable
  state transactions bump `/meta/rev` — by exactly 1 per committed transaction, persisted
  atomically. Use `if_rev` / `rev` for compare-and-set; a mismatch is `E_CONFLICT` with
  `current_rev` in the payload. Concurrent same-path transactions are serialized by rev.
- **State is data.** Empty at start; schemas have no code field; no `ds migrate`; no web UI.
  Discover capabilities at runtime: `ds --help`, `ds schema <name>`, `ds describe <path>`,
  or MCP `tools/list`.
- **Token hygiene.** The token file is 0600 and per-install; never print, log, or commit it.
  The bridge reads it from `<socket dir>/token` (override `--token-file`).

## 8. Verify the route end to end

```sh
python tests/conformance/agent-onboarding/run.py     # scripted proof, RESULT: OK, exit 0
python examples/agents/mcp/demo.py                  # ~40-line MCP demo against vdev
python tests/conformance/run.py -k onboarding        # same suite inside the big harness
```
