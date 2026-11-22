# 60 — Roadmap

Alpha phases 0–6 only (owner steering 2026-11-22, delta 10). Each phase is one verifiable
slice with an observable exit criterion. Post-alpha backlog at the end. Platform claims
follow `docs/40-platforms.md` §0 (Linux alpha only).

## Phase 0 — contracts freeze + workspace + workflow scaffolding

- **Builds:** schemas final for alpha scope; `tests/fixtures/validate.sh` green;
  `packaging/bootstrap.sh`; CI skeleton (Linux only); ticket/worktree/PR scaffolding per
  `docs/AGENT-WORKFLOW.md` (`.worktrees/`, `.review.git`, `prs/`, branch `ticket/<id>`).
- **Exit:** `sh tests/fixtures/validate.sh` → ALL CHECKS PASSED; `cargo build --workspace`
  green; conformance runner starts (0 tests); `prs/` + worktree protocol documented.

## Phase 1 — walking skeleton on the virtual device

- **Builds:** ds-protocol (mh deps), ds-device (virtual transport), ds-state, ds-ipc
  (Unix socket NDJSON + DSF1 stream + token with instant revocation), ds-daemon (state
  task, telemetry ≤1 Hz, IPC, device, guard), `ds` CLI (self-describing:
  `schema`/`describe`/`--help`), ds-vdev + ds-vdevctl, systemd user unit, conformance
  suite, **empty-state invariant** (fresh start ⇒ empty `/controls`, `/bindings`,
  `/plugins` — asserted).
- **Demo:** the 7 commands in PLAN.md §Walking skeleton.
- **Exit (observable):** `ds set /controls/keys/0/image --file examples/key.png` → golden
  sha256 via `ds-vdevctl frames key:0`; `get` round-trips with rev; `watch --ndjson`
  streams, `watch --json` exits 1; `stream push` at 32 fps × 60 s leaves rev and
  `/controls` events unchanged; query p99 ≤ 10 ms under load; `ds migrate` exits 1.

## Phase 2 — Linux hardware bring-up + camera verification

- **Builds:** real HID transport on Linux; hot-plug rescan + re-push; guard handback;
  safety tests; udev rules; hardware smoke suite; `deckstack-verify` camera harness
  (`docs/90-verification.md`, in-tree at `tools/verify/`).
- **Exit:** `DS_HARDWARE=1 python tests/hw/smoke.py` green on the real deck (enumerate,
  handshake, one frame per target, liveness ≤ 100 ms, replug ≤ 5 s
  `upstream/PROTOCOL.md:172-177`, SIGTERM handback, forbidden → `E_DEVICE` + zero bytes)
  **plus `deckstack-verify check` passes for ≥ 1 key (key:0) and the panel (`keys`)** —
  camera-verified pixels. Windows/macOS are NOT in this exit.

## Phase 3 — streaming phase

- **Builds:** frame plane end-to-end (DSF1, encode pipeline, latest-frame-wins queues,
  writer threads, telemetry), `ds stream push/stats/stop`, per-target limits, bench suite.
- **Exit:** 32 fps panel push sustained 60 s on throttled vdev, `dropped` counted, rev
  delta 0, control p99 ≤ 10 ms; golden frames green; hardware smoke pushes 24 fps to the
  real panel; **LED soak (OPEN O2) resolved and `/policy/led_rate_per_s` finalized before
  exit** (m11).

## Phase 4 — events, bindings, actions phase

- **Builds:** router, `/bindings` array, core vocabulary, exec allowlist+deny (S3),
  fire-and-forget exec (S11), atomic event ingest (S7), chains A/B.
- **Exit:** on vdev, injected key-down → plugin effect → state change ≤ 2 s (chain A);
  ds-focus `--inject` → recolor with `/focus`-based `when` atomic (chain B, S7);
  `/bin/sh -c` exec → `E_CAP`, no file created; event p99 ≤ 5 ms under stream load.

## Phase 5 — plugin host + Python wrapper

- **Builds:** plugin host (spawn, caps, rlimits, Landlock, watchdog, host-mediated net),
  manifest validation, `ds-plugin-sdk`, `ds-plugin-py` wrapper (published to PyPI as its
  own package, `repos.toml`).
- **Exit:** example plugin runs isolated; abuse tests pass (fork-bomb, `/bin/sh` refusal,
  traversal, net glob edges T11); ds-plugin-py installable from PyPI.

## Phase 6 — senders + agent surfaces

- **Builds:** ds-focus (KWin D-Bus + wlr-foreign-toplevel; closes OPEN O1), ds-mcp stdio
  bridge, CLI self-description conformance (`ds schema`, `ds describe`, exit codes).
- **Exit:** on Bazzite, focusing a window recolours the bound key ≤ 500 ms end-to-end;
  `ds-mcp tools/list` + one `deck_set` works from a spawned MCP client; every CLI verb has
  `--help` and appears in `ds schema` output (conformance).

## Post-alpha backlog (documented, not scheduled)

lights.lan plugin (`docs/80-integrations.md`, repo `deckstack-lights`) · Windows (§2 of
`docs/40-platforms.md`) · macOS (§3) · thin web/debug client over the WS/HTTP adapter
(post-alpha note only, no opinionated UI) · selective porting of old-app behaviour
(`docs/70-porting-notes.md`) · `deckstack-verify` split to its own repo.

## Dependency notes

1–6 depend on phase 0 contracts; 3 and 4 are independent of each other; 5 depends on 4;
6 on 4. Hardware verification (phase 2) gates any pixel-level claim elsewhere.
