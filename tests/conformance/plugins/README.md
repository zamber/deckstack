# tests/conformance/plugins/ — DS-053 plugin-abuse conformance suite

Machine-checked proof that a hostile/buggy plugin cannot take down the daemon,
escape its capabilities, or reach non-granted hosts (docs/20-security.md T3/T5/T6/T11;
contract `docs/contracts/plugins.md` §5/§9; component `docs/components/plugin-host.md`).

- `run.py` — the suite (8 cases, one fresh ds-vdev + ds-daemon stack per case) plus the
  standalone entry point: `python3 tests/conformance/plugins/run.py` (exit 0 = all
  contained). Registered in `tests/conformance/run.py`, so
  `python3 tests/conformance/run.py -k plugins` runs only these cases.
- `plugins_harness.py` — the black-box harness: `Stack` (boot/teardown discipline: every wait
  has a timeout, daemon+vdev always killed, plugin processes swept and asserted gone,
  temp dir removed), the raw NDJSON `Ipc` client, the fake lights HTTP server, and the
  raw-stdio fixture plugin template (`PLUGIN_PY`) that the tests generate into each
  plugin dir (stdlib only, no third-party deps).

Case map (threat row -> test):

| Threat | Case | Claim |
|---|---|---|
| T3 | `test_plugins_010_forkbomb_contained_by_nproc` | nproc rlimit contains the fork bomb (forks_ok <= grant), plugin killed, daemon healthy, no leaked pid |
| T3 | `test_plugins_020_memory_bomb_killed_by_as_limit` | AS rlimit kills the hog in the child, host healthy, daemon RSS bounded, kill visible in `/plugins` + daemon.log |
| T3 | `test_plugins_030_capability_violation_E_CAP_no_state_change` | ungranted set/get -> `E_CAP`, counted in `stats.violations`, no state change; granted emit works |
| T6 | `test_plugins_040_fs_traversal_refused_E_CAP_nothing_read` | fs request outside `fs_roots` / set outside the writable subset -> `E_CAP`, nothing read (Landlock denies the direct read too) |
| T5/T11 | `test_plugins_050_oversized_and_bad_net_url_refused_with_exact_codes` | URL > 2048 B -> `E_LIMIT`, non-http -> `E_SCHEMA`, invalid `net:<glob>` -> refused at manifest validation (invoke `E_CAP`) |
| T3 §5 | `test_plugins_060_invoke_timeout_degraded_then_restart` | 2 s timeout -> `E_LIMIT` + degraded + remaining steps run; 2nd consecutive -> restart with new pid |
| T11/S4/M5 | `test_plugins_070_net_glob_edges_exact_matrix` | `*.lan` allows lights.lan, denies lan / a.b.lan / lights.lan.evil.com; `lights.lan` matches only itself; allowed host succeeds via the fake server; raw plugin socket denied |
| T3 §9 | `test_plugins_080_crash_respawn_then_watchdog_disable` | crash mid-call -> `E_DEVICE`, respawn <= 3/600 s then `stopped` + `disabled_reason` |

How a case drives the system (phase-1 surface only): install the fixture under
`$XDG_DATA_HOME/deckstack/plugins/<id>/`, `ds bind app/abuse/<case> --do 'plugin <id>.go'`,
then IPC `emit app/abuse/<case>`; the typed step error arrives as a
`deck/system/binding-error` event, lifecycle/stats via `get /plugins/<id>`, plugin
log lines in `<data>/daemon.log`.
