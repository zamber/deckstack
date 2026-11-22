# 60 — Roadmap

Multi-phase plan per the pstack "Multi-phase or multi-PR plan" playbook: each phase is one
verifiable slice with an observable exit criterion; execution starts only on explicit go.
Phase numbering matches PLAN.md.

## Phase 0 — contracts freeze (docs only)

- **Builds:** `schemas/*.json` final for phase-1 scope; CI skeleton (schema validation +
  empty workspace builds).
- **Exit criterion:** `jsonschema` validates all schemas; `cargo build --workspace` green
  on an empty skeleton; `python tests/conformance/run.py` runs (0 tests collected, no
  error).

## Phase 1 — walking skeleton on the virtual device

- **Builds:** crates ds-protocol (mh deps wired), ds-device (virtual transport), ds-state,
  ds-ipc (Unix socket, NDJSON, token), ds-daemon (state task + IPC + device + guard),
  `ds` CLI (`get/set/watch/daemon/schema`), ds-vdev; systemd user unit; conformance suite
  covering the phase-1 contract.
- **Demo:** the exact 6 commands in PLAN.md §Walking skeleton.
- **Exit criterion (observable):** `ds set /controls/keys/0/image --file examples/key.png`
  → conformance test asserts the sha256 of the reassembled `02 36` frame on ds-vdev
  equals the golden value; `ds get` round-trips with correct `rev`; `ds watch` streams
  the change; exit codes 0/2/4/6 exercised. Bench: query p99 ≤ 10 ms under 32 fps vdev load.

## Phase 2 — hardware bring-up

- **Builds:** real HID transport on Linux (libusb), hot-plug rescan + re-push, guard
  child handback, safety tests, udev rules, hardware smoke suite, Windows named pipe
  (ds-ipc win shim) + smoke on a Windows VM.
- **Exit criterion:** `DS_HARDWARE=1 python tests/hw/smoke.py` green on the real deck:
  all modules enumerate, software-mode handshake, one frame per target accepted
  (byte counts), feature-read liveness ≤ 100 ms, replug → keys back ≤ 5 s
  (`upstream/PROTOCOL.md:172-177`), SIGTERM → handback reports observed, forbidden
  report → `E_DEVICE` with zero bytes written.

## Phase 3 — streaming phase

- **Builds:** frame plane end-to-end (stream socket, encode pipeline, latest-frame-wins
  queues, writer threads, stats), `ds stream push`, per-target limits, bench suite.
- **Exit criterion:** 32 fps panel push sustained 60 s on ds-vdev-throttled at 2.1 MB/s
  with `dropped` counted and control p99 ≤ 10 ms; golden-frame suite green; hardware
  smoke pushes 24 fps to the real panel without wedge.

## Phase 4 — events, bindings, actions phase

- **Builds:** router, bindings in state, core vocabulary (set/emit/plugin/exec/sleep/when),
  exec policy + audit, both chains in docs/contracts/actions.md §6.
- **Exit criterion:** on ds-vdev, injected key-down → plugin effect → state change ≤ 2 s
  (chain A); ds-focus `--inject` → recolor (chain B); exec without admin → `E_CAP`;
  event p99 ≤ 5 ms under stream load.

## Phase 5 — plugin phase

- **Builds:** plugin host (spawn, caps, rlimits, Landlock where available, watchdog),
  manifest validation, SDKs, `ds-plugin-py` wrapper, port hubdeck `huemux` plugin
  (live config `~/.config/hubdeck/config.yaml` uses `plugins.huemux.url: http://lights.lan`).
- **Exit criterion:** the existing huemux plugin source runs unmodified under
  ds-plugin-py against a fake HueMux HTTP server (`hubdeck/tests/fakes/ws_server.py`
  precedent) and drives lamp state through the daemon; abuse tests (fork-bomb, traversal,
  cap violation) pass (docs/20-security.md T3/T5/T6).

## Phase 6 — sender phase

- **Builds:** ds-focus with KWin + wlr-foreign-toplevel backends (resolves OPEN O1),
  ds-mcp stdio bridge; PowerShell composition examples tested.
- **Exit criterion:** on Bazzite, focusing a window recolours the bound key via the CLI
  chain, measured event→recolor ≤ 500 ms; `ds-mcp` `tools/list` + one `deck_set` works
  from a spawned MCP client (tests/conformance/mcp.py).

## Phase 7 — UI phase

- **Builds:** ds-web static shell + daemon HTTP/WS adapter with Host check + bearer.
- **Exit criterion:** Playwright: load page against ds-vdev, change a key colour in the
  UI, `ds get` reflects it; DNS-rebinding and no-token requests rejected (T7).

## Phase 8 — migration and cutover

- **Builds:** `ds migrate import` (docs/70-migration.md), coexistence runbook, rollback.
- **Exit criterion:** importing the live `~/.config/hubdeck` config produces a working
  deckstack state (Salon preset incl. knob→lamp bindings) on ds-vdev; on hardware,
  stop AppImage → start daemon → same behaviour; rollback verified by restarting the
  AppImage.

## Dependency notes

Phases 2–7 depend on phase 1's contracts; 3 and 4 are independent of each other and can
run in parallel (different subsystems: frames vs router); 5 depends on 4 (plugin steps);
6 depends on 4 (bindings on app/ topics); 7 depends on 1 only; 8 depends on 5.
