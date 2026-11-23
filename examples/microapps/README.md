# examples/microapps — write a deck micro-app in minutes

Three runnable, stdlib-only Python programs that drive the deck through the
`ds` CLI alone (no MCP, no ds-py, no third-party packages). Each has `--once`
and `--loop`, `--socket` / `--data-dir` pass-through flags, restore-on-exit
cleanup in `--loop` mode, and clear `[exit N]` errors. Read the header comment
of each file: it names the exact API calls the program teaches. The guide is
`docs/microapps.md`; the conformance proof is
`python3 tests/conformance/microapps/run.py`.

| Program | Keys / dial | Teaches | Run it |
|---|---|---|---|
| `status_strip.py` | keys 0-3: load / mem / time / uptime | `ds get --json`, `ds set`, `--socket/--data-dir`, ≤ 1 Hz loop | `python3 examples/microapps/status_strip.py --once` |
| `focus_panel.py` | dial + keys 0/1: focused app + title | `ds watch /app/focus/changed --ndjson`, `ds get /focus --json`, `ds set` | `python3 examples/microapps/focus_panel.py --loop` |
| `gitdeck.py` | keys 0-3: clean/dirty, ahead, behind, branch | `ds set` on key image/label, argv-only `git` from a micro-app | `python3 examples/microapps/gitdeck.py --once --repo .` |

Against a running daemon (or vdev stack from `docs/agents.md` §3):

```sh
ds-vdev --socket /tmp/vdev.sock &
ds daemon start --device virtual:/tmp/vdev.sock --data-dir /tmp/deck
python3 examples/microapps/gitdeck.py --once --repo "$PWD" \
    --socket /tmp/deck/daemon.sock --data-dir /tmp/deck
ds get /controls/keys/0 --json        # {"path":…,"rev":…,"value":{…}}
```

Boot-your-own-stack mode is not built into these examples on purpose: they are
micro-apps, not demos — point them at a socket with `--socket` (CI does exactly
that in `tests/conformance/microapps/run.py`).
