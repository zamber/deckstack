# tests/conformance/events/ — the DS-040 event-bus conformance suite

Black-box tests for the in-daemon event bus + router (`crates/ds-daemon/src/router/`),
driven only through the real binaries (raw NDJSON control socket + `ds-vdevctl inject`):

```
python3 tests/conformance/events/run.py [--verbose]
```

`RESULT: OK` (exit 0) = all 7 cases green. `sh tools/check.sh` runs
`tests/conformance/run.py` only, so this suite is invoked directly; the main suite's
cases must stay green alongside it (`python3 tests/conformance/run.py`).

Cases:

| case | proves |
|---|---|
| 010 | `subscribe /controls/keys/*` + `set` → exactly one `{"op":"event","path":…,"rev":N,"value":…}` with the new rev |
| 020 | `unsubscribe` stops delivery (`removed:true`, then `false`); no event afterwards |
| 030 | wildcard topic subscription `/app/*` receives a matching `emit` topic event |
| 040 | two subscribers on the same pattern both receive the event |
| 050 | `ds-vdevctl inject` key/dial input arrives as `deck/control/keys.0/down` · `deck/control/dial/down` events |
| 060 | `emit` op publishes `app/<sender>/<topic>` + payload; bad topic grammar → `E_SCHEMA` |
| 070 | a subscriber that never reads does not stall a `set` (6000-set flood, final set < 2 s), events dropped oldest with counted `{"op":"dropped","count":n}` + a `warn` line in daemon.log |

The contracts proven here are documented in `docs/contracts/actions.md` §1–2 and
`docs/components/event-router.md` (topic grammar, wildcard rules, dispatch order,
backpressure).
