# 60 — Roadmap

Multi-phase plan per the pstack "Multi-phase or multi-PR plan" playbook: each phase is one
verifiable slice with an observable exit criterion. Phase numbering matches PLAN.md.
Platform claims follow the honesty rule in docs/40-platforms.md §0 (S15).

## Phase 0 — contracts freeze (docs only)

- **Builds:** schemas final for phase-1 scope; `tests/fixtures/validate.sh` green;
  `packaging/bootstrap.sh` executable; CI skeleton.
- **Exit:** `sh tests/fixtures/validate.sh` prints ALL CHECKS PASSED; `cargo build
  --workspace` green on the skeleton; conformance runner starts (0 tests).

## Phase 1 — walking skeleton on the virtual device

- **Builds:** ds-protocol (mh deps), ds-device (virtual transport), ds-state, ds-ipc
  (Unix socket NDJSON + DSF1 stream socket + token with instant revocation, S12),
  ds-daemon (state task, telemetry sampler ≤1 Hz S1, IPC, device, guard), `ds` CLI
  (get/set/bind/emit/watch/stream/daemon/token/schema), ds-vdev + ds-vdevctl (S10),
  systemd user unit, conformance suite.
- **Demo:** the 6 commands in PLAN.md §Walking skeleton.
- **Exit (observable):** `ds set /controls/keys/0/image --file examples/key.png` →
  conformance asserts the sha256 of the reassembled `02 36` frame via `ds-vdevctl frames
  key:0` equals the golden value; `get` round-trips with rev; `watch --ndjson` streams;
  `watch --json` exits 1 (S9); `stream push keys` at 32 fps for 60 s leaves `rev` and
  `/controls` events unchanged (S1); query p99 ≤ 10 ms under that load; exit codes 0/2/3/4/6
  exercised. On the **Windows CI VM** the same conformance suite runs over the named pipe
  with the p99 budget and the ACL check where two accounts exist (M6/S5) — this is the
  phase-1 Windows perf gate; real-deck Windows remains phase 2.

## Phase 2 — hardware bring-up

- **Builds:** real HID transport on Linux; hot-plug rescan + re-push; guard handback;
  safety tests; udev rules; hardware smoke suite; Windows real-deck smoke on a VM with
  the deck (pipe perf OPEN O5 closes here).
- **Exit:** `DS_HARDWARE=1 python tests/hw/smoke.py` green on the real deck (enumerate,
  handshake, one frame per target, liveness ≤ 100 ms, replug ≤ 5 s
  `upstream/PROTOCOL.md:172-177`, SIGTERM handback, forbidden → `E_DEVICE` + zero bytes);
  Windows VM + deck passes the same smoke over the pipe (closes M6/O5).

## Phase 3 — streaming phase

- **Builds:** frame plane end-to-end (DSF1, encode pipeline, latest-frame-wins queues,
  writer threads, telemetry stats), `ds stream push/stats/stop`, per-target limits, bench.
- **Exit:** 32 fps panel push sustained 60 s on throttled vdev with `dropped` counted and
  control p99 ≤ 10 ms; S1 no-churn bench green; golden frames green; hardware smoke
  pushes 24 fps to the real panel without wedge; **LED soak (OPEN O2) resolved and
  `/policy/led_rate_per_s` finalized before this phase exits** (m11).

## Phase 4 — events, bindings, actions phase

- **Builds:** router, `/bindings` array, core vocabulary, exec allowlist+deny (S3),
  fire-and-forget exec (S11), atomic event ingest (S7), chains A/B.
- **Exit:** on ds-vdev, injected key-down → plugin effect → state change ≤ 2 s (chain A);
  ds-focus `--inject` → recolor, and `/focus`-based `when` sees the new value atomically
  (chain B, S7); `/bin/sh -c` exec → `E_CAP` with no file created (S3); event p99 ≤ 5 ms
  under stream load.

## Phase 5 — plugin phase

- **Builds:** plugin host (spawn, caps, rlimits, Landlock, watchdog), manifest validation,
  SDKs, ds-plugin-py wrapper, host-mediated `net` requests with glob checks (S4/M5/T11),
  ported hubdeck huemux plugin (live config uses `plugins.huemux.url: http://lights.lan`).
- **Exit:** huemux plugin source runs under ds-plugin-py against a fake HueMux server and
  drives lamp state through the daemon; net glob edge cases (`lights.lan.evil.com`,
  `a.b.lan` vs `*.lan`) rejected with `E_CAP`; abuse tests pass.

## Phase 6 — sender phase

- **Builds:** ds-focus KWin + wlr-foreign-toplevel (closes OPEN O1), ds-mcp stdio bridge.
- **Exit:** on Bazzite, focusing a window recolours the bound key via the CLI chain,
  event→recolor ≤ 500 ms; `ds-mcp tools/list` + one `deck_set` works
  (tests/conformance/mcp.py).

## Phase 7 — UI phase

- **Builds:** ds-web static shell + HTTP/WS adapter (Host check + bearer, T7).
- **Exit:** Playwright: set a key colour from the UI against ds-vdev, `ds get` reflects
  it; rebinding/no-token requests rejected.

## Phase 8 — migration and cutover

- **Builds:** `ds migrate import` (validating importer, m8), coexistence runbook, rollback.
- **Exit:** import of the live `~/.config/hubdeck` (incl. `{python:…}` → porting TODO,
  never executed) produces working state on ds-vdev; on hardware: stop AppImage →
  `ds daemon start` → same behaviour; rollback = restart AppImage.

## Dependency notes

2–7 depend on phase 1 contracts; 3 and 4 are independent of each other; 5 depends on 4;
6 on 4; 7 on 1; 8 on 5. macOS enters only after phase 2 if at all (M7).
