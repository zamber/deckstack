# Micro-apps — write small programs that drive the deck

A **micro-app** is a small program (any language, stdlib is enough) that runs
*outside* the daemon and drives the deck through the `ds` CLI: read state with
`ds get`, write it with `ds set`, react to events with `ds watch --ndjson`,
declare reactions with `ds bind`. This page is the authoring route: learn the
API from the CLI itself, then copy `examples/microapps/` and adapt. MCP
(`ds-mcp`, `docs/agents.md` §4.1) stays available as the optional native-control
path; `clients/ds-py` is optional sugar over the same verbs. Everything here is
reachable with `ds` alone (R1).

Contracts are normative: `docs/contracts/cli.md` (grammar, output, exit codes),
`docs/contracts/state.md` (paths, rev, telemetry), `docs/contracts/actions.md`
(events, bindings), `docs/contracts/streaming.md` (frames). Where this page and
a contract disagree, the contract wins — report the gap (see `docs/agents.md` §6).

## 1. Discovery loop — learn the API from the CLI

Never read the source to find a capability; the binary describes itself
(DS-062/DS-099):

```sh
ds --help                     # the index: every verb, global flags, exit-code table
ds <verb> --help              # per verb: Usage, arguments WITH DEFAULTS, ONE runnable
                              # Example, the exit codes it returns, `ds schema`/`ds
                              # describe` pointer — all within ~25 lines
ds daemon --help ; ds daemon start --help   # same for group subcommands
ds schema state               # the normative JSON Schema (state|ipc|stream-frame|
                              # plugin-manifest|binding), no daemon needed
ds describe /controls/keys/0 # one path: schema + current value + allowed verbs
```

Loop: `--help` says *what exists* → `ds schema state` says *what shape a value
must have* → `ds describe <path>` says *what a path currently holds and which
verbs accept it*. Then write the app and run it against a virtual deck:

```sh
ds-vdev --socket /tmp/vdev.sock &
ds daemon start --device virtual:/tmp/vdev.sock --data-dir /tmp/deck
python3 examples/microapps/gitdeck.py --once --repo "$PWD" \
    --socket /tmp/deck/daemon.sock --data-dir /tmp/deck
```

## 2. The state path map an app needs

One JSON document, `/`-separated paths, `*` matches one segment in queries
(`docs/contracts/state.md` §1). The paths a micro-app actually touches:

| Path | Shape | Notes for apps |
|---|---|---|
| `/meta` | `{version, rev, daemon_pid, started_at, protocol}` | `rev` bumps by exactly 1 per committed transaction |
| `/controls/keys/<0-14>` | `{image, label, brightness}` | the 15 keys — status strips, indicators |
| `/controls/dial` | `{image, label, brightness}` | single control, not a map; brightness 0 refused |
| `/controls/knobs/<0-2>`, `/controls/faders/<0-4>`, `/controls/rollers/<0-1>` | `{led: {mode,color,brightness}, value}` | LED controls |
| `/bindings`, `/bindings/<id>` | array of binding objects | managed via `ds bind`/`ds unbind` (§5) |
| `/focus` | `{app, title, pid, at_rev}` | ephemeral mirror of `app/focus/changed` (S7); may not exist yet → `E_PATH` exit 2 |
| `/telemetry/*` | samples | **no rev, never persisted, sampled ≤ 1 Hz by the daemon** (S1) |
| `/settings/…` | senders, log level | e.g. `/settings/senders/focus` |
| `/policy/…` | `net.allow_hosts`, `fs.roots`, `exec.allow`/`deny`, `led_rate_per_s` | admin scope; `--file` images must live under `fs.roots` |
| `/devices/<serial>` | hot-plug identity | usually irrelevant to apps |
| `/streams/<target>` | durable per-target config `{queue_depth}` | frame-plane config, not stats |

Index ranges are closed by the schema itself: keys `0-14`, knobs `0-2`, faders
`0-4`, rollers `0-1`. `ds set /controls/keys/15 '{}'` exits 2 with `E_PATH`.

### Clearing and restoring a path

A write replaces the whole value at a path. Set an object path to `{}` to clear it. The
value must match the schema for that path. `null` is not a valid `screenControl`.
`ds set /controls/keys/0 null` is rejected with `E_SCHEMA` and exit 1.

Snapshot the keys before the first write. Restore the snapshot on exit. With `DS` and
`ds_set` from §9:

```python
def ds_get(path):
    p = subprocess.run(DS + ["get", path, "--json"], capture_output=True, text=True)
    if p.returncode not in (0, 2):
        sys.exit("ds get failed [exit %d]: %s" % (p.returncode, p.stderr.strip()))
    return json.loads(p.stdout)["value"] if p.returncode == 0 else None

snap = {k: ds_get("/controls/keys/%d" % k) for k in keys}

def restore(keys, snap):
    for k in keys:
        ds_set("/controls/keys/%d" % k, snap[k] if snap[k] is not None else {})
```

`ds_get` returns `None` when the path does not exist. `ds get` exits 2 in that case.
`restore` sets a key to `{}` when the key was absent before the app started.
## 3. Events and `ds watch --ndjson`

Topics (`docs/contracts/actions.md` §1): `deck/control/<control>/<gesture>`,
`deck/device/<serial>/<event>`, `deck/system/<event>`, `daemon/<event>`,
`app/<sender>/<topic>` (any external program — the focus sender emits
`app/focus/changed`), `plugin/<id>/<event>`.

```sh
ds watch /controls --ndjson                 # state events: {"op":"event","path":…,"rev":…,"value":…,"prev":…}
ds watch /app/focus/changed --ndjson        # topic subscription: topic spelled with a leading /
ds watch /deck/control/keys/* --ndjson      # wildcard: * eats exactly one segment
```

- Streaming verb: **only `--ndjson` is accepted** — `ds watch … --json` is a
  usage error, exit 1. One JSON object per line, no wrapper. `--from-rev N`
  replays, `--max-events N` exits after N events, else run until Ctrl-C.
- Topic events arrive as `{"op":"event","topic":"app/focus/changed","path":"/app/focus/changed","ts_ms":…,"payload":{…}}`
  with `rev: null` (only state events carry a rev). `/telemetry/*` events carry
  `"telemetry": true` and no rev.
- Backpressure: a slow subscriber gets `{"op":"dropped","count":n}` after 1000
  queued events — read fast, or use `--max-events` batches.
- `ds watch` needs no polling loop in your app: the daemon pushes. Prefer
  watch over polling (see §7).
- D-Bus is **not** a daemon topic — there is no `app/mpris/*` stream. To show
  an external bus source (e.g. the first MPRIS player) on the deck, poll it
  argv-only from a script micro-app at ≤ 1 Hz and `ds set` the result:
  `examples/microapps/now_playing.py` is the worked example (gdbus/dbus-send
  probe, documented "no player" fallback when the bus or a player is absent).

## 4. Images and the target table

`image` on a screen control is a *visual* (`schemas/state.json` `$defs/visual`):

| kind | value | use |
|---|---|---|
| `color` | `"#rrggbb"` | solid key/dial colour |
| `text` | `{value ≤ 32 chars, fg?, bg?}` | short text on the key/dial |
| `icon` | `tabler:<name>` | icon from the tabler set |
| `file` | absolute path, no `..` | `ds set <path> --file f` — canonicalized and confined to `/policy/fs/roots` (+ `/tmp`), else exit 4 |
| `frame` | `{target, seq, sha256}` | a *reference* to a streamed frame, set by `stream push`; never bytes |

```sh
ds set /controls/keys/0/image --json '{"kind":"color","value":"#ff0000"}'
ds set /controls/keys/0/label '"Salon"'
ds set /controls/keys/0/image --file ~/pictures/key.png
ds set /controls/dial/image --json '{"kind":"text","value":"12:40","fg":"#ffffff","bg":"#000000"}'
```

Frame animation (`ds stream …`, phase 3): the **target table** is the unified
vocabulary `keys | key:<0-14> | dial` — there is no `panel`/`screen` target
(S6). `ds stream push <target> --file f | --dir d --fps N [--loop]` rides DSF1
on `<data-dir>/stream.sock`; `ds stream stats <target>` reads
`/telemetry/streams/<target>` (no rev); `ds stream stop <target>` stops it.
Frames never bump `rev` and never enter the state document (S1) — use frames for
animation, `set` for everything else.

## 5. Bindings — declarative behavior, no code

A binding says "on topic T (optionally when C), run steps S". Stored in
`/bindings`, validated against `schemas/binding.json` on client and daemon:

```sh
ds bind deck/control/keys.0/down --do 'set /controls/keys/5/image {"kind":"color","value":"#ff8800"}'   # → b0001
ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' \
     --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}'
ds get /bindings --json            # list; ds watch /bindings --ndjson streams changes
ds unbind b0001                    # or: ds unbind --topic deck/control/keys.0/down
```

- Topics: `*` is a wildcard only as a **whole segment**; `/` and `.` both
  separate segments (`deck/control/keys/*` matches `keys.0/down`).
- Steps (closed vocabulary, `actions.md` §3): `set <path> <value>`,
  `emit <topic> [payload]`, `plugin <id.action> [args]`, `exec <argv…>`,
  `sleep <ms>`. Code-ish steps (`{"do":"eval",…}`) are rejected (exit 1) — config
  never contains code.
- `when` reads **state** (`path`) or the **event payload** (`payload`); for
  `app/focus/changed` the router mirrors the payload into `/focus` in the same
  transaction (S7), so `/focus`-based conditions are never stale.
- `exec` steps are fire-and-forget argv-only: absolute `argv[0]` must be in
  `/policy/exec/allow` and not in `/policy/exec/deny` (shells/interpreters are
  denied by default), admin scope + `allow_exec:true` required (S3/S11).

A micro-app that is pure "event → set state" is often **just a binding** — no
program needed. Write a program when you need external data (git, /proc, a
socket) or a loop.

## 6. Script vs binding vs plugin — the decision table

| If the behavior is… | Use | Why |
|---|---|---|
| "on event X, set path Y" (no outside data) | `ds bind` (§5) | stored in state, schema-validated, survives restarts, zero code |
| "poll something external and show it on keys" (git status, load, clock, weather) | a **script** micro-app (`examples/microapps/`) calling `ds set`/`ds get` | runs outside the daemon, any language, easy to kill/replace |
| "react to an event stream and update the deck" (focus panel) | a script running `ds watch --ndjson` + `ds set` | the daemon pushes; no polling |
| "needs network or filesystem access inside the daemon's sandbox/capabilities" | a **plugin** (`docs/contracts/plugins.md`; phase 5 — `deck_plugin_list` answers `E_INTERNAL` exit 8 in phase 1) | host-mediated `net:` caps, fs roots, rlimits; `plugin` steps in bindings |
| "drive the deck from an agent harness (Claude Code etc.)" | **MCP** `ds-mcp` (`docs/agents.md` §4.1) or `ds-py` sugar | native-control path; same daemon ops, not a micro-app |

Rule of thumb: bindings for declarative reactions, scripts for anything that
reads the outside world, plugins only when you need the daemon's capability
sandbox. A script and a binding compose: the script sets `/settings/…` or a
state path, the binding reacts.

## 7. Rate limits and the 1 Hz attention rule

- **The 1 Hz attention rule:** anything that changes *what the human looks at*
  (key/dial labels, colours, LED state) updates at most **once per second**.
  Poll `ds get` no faster than 1 Hz (the examples enforce `--interval >= 1.0`
  and default to 1 s); prefer `ds watch` over polling entirely. The only
  faster lane is frames (`ds stream push --fps 1..=30`), which never touch
  state or `rev` (S1).
- `/telemetry/*` is sampled by the daemon at **≤ 1 Hz**, never persisted,
  `ds get /telemetry/…` returns no `rev` — read it freely, it is cheap.
- Wire limits: request line ≤ 1 MiB, response ≤ 4 MiB, subscriber queue 1000
  events (overflow → `dropped` events, oldest first), stream frame payload
  ≤ 16 MB, `--dir` pushes capped at 300 frames and 30 fps, `--credits 1..=1024`.
- `/policy/led_rate_per_s` (default 30, range 1-30) caps LED writes **per module** —
  admin scope. LED writes are coalesced latest-wins (the newest colour always reaches the
  deck, no backlog), so a 20 Hz colour feed is safe; `/controls/rollers/*/led` is accepted
  but is a no-op (the M032LE has no LED command in the protocol). When in doubt: write
  once per second, watch for the rest.

## 8. Errors and exit codes

`ds` exit codes (`docs/contracts/cli.md` §3): `0` ok · `1` usage/schema ·
`2` E_PATH · `3` E_CONFLICT (`current_rev` in the payload — re-read and retry
with `--if-rev`) · `4` E_AUTH/E_SCOPE/E_CAP (token/scope — see
`docs/agents.md` §6) · `5` E_LIMIT (back off) · `6` E_UNREACHABLE (message
carries the exact `ds daemon start` command — surface it) · `7` E_DEVICE ·
`8` E_INTERNAL/E_VERSION.

Rules for a micro-app: stdout is **data only**; errors go to stderr as
`ds: <message>  [exit N]` (with `--json`, stderr is one
`{"ok":false,"error":{…}}` object). Check the return code on every call, print
the stderr verbatim on failure, exit nonzero yourself. A path that does not
exist yet is exit 2 — create it with `ds set` (e.g. `/controls/keys/0` appears
on first write); `/focus` may be absent until the first focus event. Known CLI
gap (reported, not papered over): the `ds emit`, `ds plugin …` and `ds token …`
verbs are refused with exit 1 in phase 1 — emit events through the raw IPC
`emit` op or MCP `deck_emit` instead (`docs/agents.md` §6).

## 9. Worked example (~30 lines): a clock key

```python
#!/usr/bin/env python3
"""clock_key.py — key 14 shows HH:MM, updated once a minute.
Teaches: ds set (label + colour), the exit-code check, a <=1 Hz loop."""
import json, subprocess, sys, time

DS = ["ds", "--socket", "/tmp/deck/daemon.sock"]   # or rely on ds defaults
def ds_set(path, value):                            # one argv-only call
    p = subprocess.run(DS + ["set", path, "--json", json.dumps(value)],
                       capture_output=True, text=True, timeout=10)
    if p.returncode != 0:
        sys.exit("clock_key: ds set failed [exit %d]: %s" % (p.returncode, p.stderr.strip()))

def tick():
    now = time.strftime("%H:%M")
    ds_set("/controls/keys/14/label", now)                       # label ≤ 32 chars
    ds_set("/controls/keys/14/image", {"kind": "color", "value": "#3cc86e"})
    print(now, flush=True)

if __name__ == "__main__":
    while True:                       # 1 Hz attention rule: sleep 60 s, not less
        tick()
        time.sleep(60)
```

Run it: `python3 clock_key.py &` then `ds get /controls/keys/14 --json`.
Copy-adapt `examples/microapps/` instead of starting from zero:
`status_strip.py` (poll /proc → keys), `focus_panel.py` (`ds watch
/app/focus/changed --ndjson` → dial), `gitdeck.py` (argv-only `git` → keys
0-3), `now_playing.py` (argv-only D-Bus/MPRIS probe → keys 0-2 + dial, with a
documented no-player fallback). The conformance proof that they run against a
virtual deck: `python3 tests/conformance/microapps/run.py`.
