# clients/ds-py — deckstack Python helper for agents (ticket DS-063)

A stdlib-only, thin wrapper over the `ds` CLI for agents that prefer Python.
**The helper is optional sugar: everything it does is reachable with the `ds`
CLI alone (R1), and the CLI contract in `docs/contracts/cli.md` is the single
source of truth.** The helper never reimplements daemon logic and never opens
the control socket itself — every call is one `ds` subprocess (argv-only, never
`shell=True`, bounded timeout), so there is no second protocol to drift.

## Layout

```
clients/ds-py/
  deckstack/          the package (stdlib only: subprocess + json)
    __init__.py       exports DeckClient, WatchStream, DeckError
    client.py         DeckClient — typed helpers, one subprocess per call
    errors.py         DeckError + the exit-code table (cli.md §3)
  smoke.py            end-to-end smoke: boots ds-vdev + ds-daemon, exercises the API
  README.md           this file
```

Copy `deckstack/` next to your agent script or put `clients/ds-py` on
`sys.path`; no install step, no third-party packages (not published in alpha —
PyPI name is decided at DS-083).

## Discovery contract — how an agent finds capabilities

The helper adds no hidden surface; discover through the CLI itself:

| Command | Gives you |
|---|---|
| `ds --help`, `ds <group> --help` | the full grammar (groups: get set bind unbind emit watch events stream plugin daemon token schema describe) |
| `ds schema <state\|ipc\|stream-frame\|plugin-manifest\|binding>` | the normative JSON Schemas from `schemas/` |
| `ds describe <path>` | schema + current value + allowed verbs for one path |
| `ds-mcp` (stdio MCP) `tools/list` | the same API as MCP tools: deck_status, deck_get, deck_set, deck_bind, deck_unbind, deck_emit, deck_watch, deck_stream_push, deck_plugin_list, deck_schema (docs/components/mcp-bridge.md) |

In Python: `client.help()`, `client.help("stream")`, `client.schema("state")`,
`client.describe("/controls/keys")`.

## API surface

```python
from deckstack import DeckClient, DeckError

with DeckClient(socket="…/daemon.sock", data_dir="…") as ds:   # or ds="target/release/ds"
    ds.status()                    # ds daemon status --json        -> dict
    ds.get("/controls/keys/0")     # ds get <path> --json           -> {"path","value","rev"}
    rev = ds.set("/controls/keys/0/image",
                 {"kind": "color", "value": "#3cc86e"})   # -> new rev (int)
    rev = ds.set("/controls/keys/0/image", file="pic.png")     # --file variant
    bid = ds.bind("deck/control/keys/0/down",
                  'set /controls/keys/5/image {"kind":"color","value":"#ff8800"}',
                  when={"payload": "delta", "op": "gt", "value": 0})  # -> "bNNNN"
    ds.unbind(binding_id=bid)      # or topic="…"                   -> ["bNNNN"]
    ds.emit("app/demo", {"x": 1})  # NOTE: exit 1 in phase 1 (later ticket)
    with ds.watch("/controls", max_events=1) as w:             # --ndjson stream
        for event in w: ...        # one JSON object per event
    ds.stream_push("key:0", "frame.png", loop=True)            # DSF1 on stream.sock
    ds.stream_stats("key:0")       # ds stream stats --json (no rev, S1)
    ds.stream_stop("key:0")
    ds.schema("state")             # ds schema <name>               -> dict
    ds.describe("/controls/keys")  # ds describe <path>             -> text
```

Connection defaults follow the CLI: `--socket` wins, else `--data-dir/daemon.sock`,
else `$XDG_RUNTIME_DIR/deckstack/daemon.sock` (fallback `~/.local/state/deckstack/`);
the token is read by `ds` from the 0600 token file and is never handled (or
logged) by this helper.

## Errors

Any nonzero `ds` exit raises `DeckError` carrying `exit_code` (0–8), the mapped
IPC `code` (`E_PATH`, `E_CONFLICT`, …), raw `stderr`/`stdout`, and a message
ending in the same `[exit N]` suffix the human CLI prints:

| exit | code | meaning |
|---|---|---|
| 1 | `E_SCHEMA` | usage/schema error |
| 2 | `E_PATH` | path not found / bad syntax |
| 3 | `E_CONFLICT` | `if_rev` mismatch |
| 4 | `E_AUTH`/`E_SCOPE`/`E_CAP` | token/scope/capability |
| 5 | `E_LIMIT` | size/rate/queue limit |
| 6 | `E_UNREACHABLE` | daemon not running (message carries the `ds daemon start` hint) |
| 7 | `E_DEVICE` | hardware error |
| 8 | `E_INTERNAL`/`E_VERSION` | bug / protocol mismatch |

Machine-readable output is `--json`-only: helpers that return objects parse
stdout as exactly one JSON object and raise `DeckError` otherwise. Streaming
verbs (`watch`, `events`) accept only `--ndjson` — one object per line — and the
helper never passes `--json` to them (exit 1, S9).

## Smoke / verification

```sh
python clients/ds-py/smoke.py     # boots ds-vdev + ds-daemon, exit 0 = RESULT: OK
python examples/agents/focus_recolor.py
python examples/agents/status_poller.py
```
