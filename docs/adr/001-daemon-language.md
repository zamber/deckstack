# ADR-001 — Daemon language: Rust

Status: accepted (planning). Deciders: plan author per BRIEF.md decision 2.

## Context

One long-lived process owns state + HID I/O and must serve queries while streaming ~32 fps
of frames (R1/R2/R5/R11). The frame path must degrade by dropping frames, never by blocking
control — the Python engine does exactly the wrong thing (synchronous paint on the engine
thread, `hubdeck/src/hubdeck/engine.py:465-514, 1325-1341`). The hardware-verified device
code exists in Rust (`upstream/crates/mh-protocol`, `crates/mh-device`).

## Decision

Rust (edition 2024) for `ds-daemon` and all core crates. Static binary, systemd user
service (D11). Deps: tokio or std-threads (chosen in phase 1 prototype; default tokio for
the socket server), serde/serde_json, hidapi via mh-device, jsonschema, tracing.

## Alternatives

- **Python daemon** — rejected: GIL makes "queries stay fast under frame load" a locking
  prayer; the existing Python engine already demonstrates the stall; the monolith's shape
  is what we're replacing.
- **Go daemon** — rejected: no GIL, good perf, but no existing device code; hidraw/libusb
  + safety rules would be re-derived, re-risking brick-class bugs (discovery/40 §8).
- **Zig** — rejected: ecosystem too young for serde/jsonschema/hidapi needs.

## Consequences

Positive: single static binary; mh-device reuse (ADR-010); fearless-concurrency matches the
thread-per-writer frame model. Negative: Rust compile times; plugin authors need the SDK
(not the daemon language) — fine.

## References

BRIEF.md R1/R2/R5; deckstack/docs/discovery/10 §7, 20 §1; `hubdeck/.../engine.py:465-514`;
`upstream/crates/mh-device`. Decision log D01.
