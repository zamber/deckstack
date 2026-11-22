# CLI self-description + error-contract conformance (DS-062)

`run.py` machine-checks `docs/contracts/cli.md` §1–§5 against the real `ds` binary,
black-box (ds-vdev + ds-daemon on a fresh temp dir; a scripted fake control socket
drives the exit codes not reachable against a healthy stack):

- recursive `--help` walk (parses the help text; every advertised verb/group/sub must
  have help exit 0 **and** a covering test — the coverage report is printed),
- per-verb help mentions the contract's flags; later-phase verbs (emit/events/plugin/
  token) are refused exit 1 and fail the suite the moment they land,
- `ds schema <name>` == `schemas/<file>.json` (parse, `$id`, semantic equality) for all
  five schemas; `ds describe <path>` = schema + current value + allowed verbs,
- exit codes 0–8 with the exact `ds: …  [exit N]` stderr shape and the one-object
  `--json` error envelope (0–6 on the real stack, 7/8 + the full E_* table through the
  fake socket),
- `--json` = exactly one object on stdout (empty stderr); `--ndjson` one object per
  line for `watch`; `--json` on a streaming verb exits 1; unknown flags exit 1 with an
  actionable message; `ds migrate` exits 1.

Registered in `tests/conformance/run.py` (`-k cli` selects only these cases) and
runnable directly:

```sh
python3 tests/conformance/run.py -k cli
python3 tests/conformance/cli/run.py
```
