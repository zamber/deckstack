# Ticket DS-129 — e2e playwright test of the preview loop against the vdev with scripted attach/reattach

Date: 2026-11-28. Type: test tooling + verification.

## Motivation

The whole loop (device attach/reattach -> daemon events -> /preview/ws -> solver ->
render, plus frame streaming) must be tested end-to-end with known attachment and
re-attachment points, and the perf (event -> paint latency, stream start) measured.

## Requirements

- A runnable script (stdlib + playwright MCP or node playwright if available; else a
  documented playwright-mcp session script) under `clients/preview-web/tests/` or
  `tools/` that:
  1. starts/uses the vdev demo daemon (socket /tmp/ds121-demo/vdev.sock, preview port
     39955, token dace7377…a7fa6) — or starts its own vdev+daemon on a fresh port;
  2. opens the webapp, asserts initial render matches the solver for the current slots;
  3. scripts `ds-vdevctl move <kind> <slot>` and `replug` sequences at KNOWN points
     (e.g. keys 2-2 -> 1-3-4 -> 2-3, rollers 1-1-4 -> 3-3, replug all), asserting after
     each: DOM placement == tools/placement_solver.py output for the new device set,
     no overlaps, off-deck strip correct, no page reload (window marker), frame stream
     still live (screen image present and non-empty after each change);
  4. measures: event->paint latency (timestamp of ws event vs next rendered label
     change), frame-stream start latency after page open, and inter-frame gap (no
     blank screen: image href persists across re-renders);
  5. prints PASS/FAIL summary, exits non-zero on failure.
- Wire into docs (how to run), and reference from DS-126 acceptance.

## Links

DS-124, DS-126, DS-125, SALON-003, crates/ds-vdev (ds-vdevctl move/replug).
