# Micro-app authoring-route conformance (DS-099)

`run.py` machine-checks the DS-099 deliverables against the real binaries, black-box
(ds-vdev + ds-daemon on a fresh temp dir):

- **help CONTENT for every verb** — `ds <topic> --help` for all 17 topics (top + the 10
  verbs + `daemon start|stop|status` + `stream push|stats|stop`) exits 0 and contains
  `Usage:`, an `Exit codes:` line, and a `ds schema` / `ds describe` pointer; every
  non-group verb help also documents flag defaults, carries exactly ONE runnable
  `Example:` command, mentions the contract's per-verb flags, and stays <= 25 lines;
  group helps keep one usage line per subcommand. (DS-062 asserted presence; DS-099
  asserts content.)
- **a micro-app end to end** — `examples/microapps/gitdeck.py --once` against a temp git
  clone with an upstream: clean → ahead=1 → dirty → behind=1 transitions, each asserted
  on keys 0-3 via `ds get <path> --json` (label + colour image).
- `focus_panel.py` — `--once` renders `/focus` (after a raw-IPC `emit app/focus/changed`)
  onto the dial, and `--loop` re-renders on the watched `app/focus/changed` topic
  (`ds watch --ndjson` path).
- `status_strip.py --once` — keys 0-3 get load/mem/time/uptime labels, asserted via
  `ds get`.

Registered in `tests/conformance/run.py` (`-k microapps` selects only these cases).

```sh
python tests/conformance/microapps/run.py     # RESULT: OK, exit 0
python tests/conformance/run.py               # runs this suite too
```
