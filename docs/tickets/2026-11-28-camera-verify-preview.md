# Ticket DS-125 — camera-oracle verification of the preview deck map on hardware

Date: 2026-11-28. Type: verification (ADR-015 camera pipeline; salon session).

## Motivation

The webcam is plugged in again. The preview webapp's drawn layout must be checked
against the physical deck: which modules are connected where, in what orientation
(the hardware's rotation readout is unreliable — DS-123 deduces it).

## Requirements

1. Capture the deck with the camera (ADR-015 pipeline, `docs/90-verification.md`;
   /dev/video* present) in the current base orientation.
2. For each connected module per the daemon's `/devices` (slot/position), verify the
   webapp's deduced (col,row,rotation) matches the photo: connector position + corner
   indicator (cross-position rule) + occupied cells.
3. Record mismatches as evidence in this ticket; feed corrections to DS-122/DS-123.
4. Re-verify after a reseat and after a base flat<->upright flip (live event check).

## Acceptance

- Photo + render side-by-side per configuration, stored under
  `docs/discovery/salon-001/` (or /tmp bundle referenced from the ticket).
- Zero unexplained mismatches, or a ticketed explanation per mismatch.

## Links

ADR-015, DS-122, DS-123, DS-124, SALON-001, SALON-003.
