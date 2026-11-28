# SALON-001 — DS-112 hardware orientation capture session

Status: blocked (deck reserved by another session; webcam disconnected)
Date: 2026-11-27
Session: salon-deck systemd user unit, first free window after 2026-11-27
Owner: salon-deck operator on duty
Deck state: to be captured at session start — `ds get /devices --json` (serial, slot,
connection), base orientation from `deck/device/<serial>/orientation`, modules present
(keys M4315 / dial M131E / M0 family per `docs/components/virtual-device.md:10-12`)
Hardware involved: MasterHub base + knob module (reseat target), keys module, AUKEY
webcam `/dev/video2` if reconnected (`docs/90-verification.md` §pipeline step 1)
Links: DS-112 (`docs/tickets/2026-11-25-module-position-orientation.md`), O2/O3/O4
(`docs/open-questions.md:20-22`)

## Goal

Capture, on real hardware, the facts DS-112 needs: module identity by USB port path +
base orientation (not serial alone), re-handshake + re-push timing on reseat/rotation
within the ≤ 5 s budget (`upstream/PROTOCOL.md:172-177`), and — if time allows — the O2
fader LED soak rates.

## Steps

1. Record deck state: `ds daemon status`, `ds get /devices --json`,
   `ds-vdevctl` equivalents are N/A on hardware — use `ds get /telemetry/streams --json`.
2. Reseat the knob module to a different connector; assert `/devices/<serial>/position`
   and `orientation` update and the daemon re-handshakes + re-pushes within ≤ 5 s
   (DS-112 acceptance).
3. Rotate the base flat↔upright; capture the `0x2B` orientation event and the logical
   layout change (4×6 vs 6×4).
4. If the webcam is connected: `deckstack-verify check --target key:0 --reference
   tests/golden/key0.png` after a known image (`docs/90-verification.md`).
5. Optional, only if the session is long: O2 soak — 10/25/50/100 LED writes/s for a
   bounded window while pinging feature `0x2E` (`docs/open-questions.md:20`).

## Verification evidence

- (to fill in) transcript of the commands above with outputs; re-handshake latency
  measured from daemon log timestamps; camera check exit code if run.
- Label each item vdev / hardware / camera.

## Outcome

- (to fill in) which DS-112 requirements and open questions this session closes or
  advances; any new DS-xxx ticket filed from observed bugs.
