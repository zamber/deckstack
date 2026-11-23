# tests/conformance/actions/ — DS-045 action-chain conformance

Black-box conformance for the two worked chains of `docs/contracts/actions.md` §6
plus the refused-step behaviour of §2/§4:

- **Chain A** — device input → binding → visual: `ds-vdevctl inject` a key-down on
  the virtual device, a binding's `set` steps change the key's label/image, and the
  visual change is observed via `ds get` within the 2 s budget (actions.md §5).
  Asserts one event path (exactly one `deck/control/keys.0/down` topic event, no
  duplicates, nothing on state-path subscriptions for the input itself), step
  ordering (label then image, one rev bump per `set`), the `emit` step, and that no
  `deck/system/binding-error` fires.
- **Chain B** — focus → binding → refused step (S7): `emit app/focus/changed` fires
  both a payload-`when` and a `/focus/app`-path-`when` binding in one atomic
  dispatch (one rev bump for mirror + both sets) and recolours `/controls/keys/3`;
  a binding whose leading `plugin`/`exec` steps are refused publishes
  `deck/system/binding-error` per refused step (E_CAP stubs until DS-050 / the
  ingest path's fail-closed exec) while the remaining `set` + `emit` + `sleep`
  steps still run.

Run (standalone, like `events/`, `bindings/`, `focus_ingest/` — `check.sh` runs only
`tests/conformance/run.py`, which this ticket's `touches` do not allow editing):

```sh
python3 tests/conformance/actions/run.py [--verbose]
```

Exit 0 = all green (`RESULT: OK`), each case prints one `PASS`/`FAIL` line. The suite
boots real `ds-vdev` + `ds-daemon` binaries on a fresh temp dir and always kills
them (SIGTERM then SIGKILL) and removes the dir, so nothing leaks.

The latency numbers behind actions.md §5 live in `tests/bench/actions.rs` (DS-045
bench, env-gated by `DS_ACTIONS_BENCH=1`).
