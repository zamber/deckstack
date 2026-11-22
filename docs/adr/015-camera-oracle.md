# ADR-015 — Camera oracle for pixel-level verification on hardware

Status: accepted (owner steering 2026-11-22, delta 6).

## Context

R5 asks tests to capture what the device reports; the device has no screen readback
(`upstream/PROTOCOL.md:19-23,141-167`), so the strongest software proxy is the sent log +
liveness (D15). A webcam pointed at the deck was used upstream for exactly the pixel facts
(`upstream/PROTOCOL.md:70-87,243-246`).

## Decision

`docs/90-verification.md` defines the camera pipeline: discovery, ROI calibration against
the known key-window table (`upstream/PROTOCOL.md:81`), capture, reference frames, and
color/SSIM/perceptual checks with documented limits (cannot prove sub-frame timing or
device-side drops). Tool `deckstack-verify` (subcommands capture/calibrate/check/sweep,
NDJSON output), published as its own repo per `repos.toml`. Phase-2 hardware exit includes
≥ 1 key (key:0) and the panel (`keys`) camera-verified. CI uses ds-vdev only; hardware
runs use the camera.

## Alternatives

- USBPcap hunt for a feedback report — kept as the OPEN experiment that would settle C3, not as the verification method.
- No hardware pixel verification — rejected: R5's spirit is "prove what the deck shows"; the proxy alone can't.

## References

`docs/90-verification.md`; `docs/30-testing.md` §6–8; `docs/60-roadmap.md` phase 2; decision log D35.
