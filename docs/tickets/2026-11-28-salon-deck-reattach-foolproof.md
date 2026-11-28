# Ticket DS-128 — salon-deck: foolproof, instant reattachment handling

Date: 2026-11-28. Type: salon-deck app change on branch `salon-002-ds112-port`.

## Motivation

Owner observed (2026-11-28): after rotating/replugging the knobs module, salon-deck did
not remount it — device reconnections must be handled transparently: the app should
pool device change events as they come and re-map actions to the logical position
instantly, with no restart and no stale bindings.

## Requirements

- Build on the SALON-002 port branch (deck_placement.py + deck/device event watcher).
- Pool/batch device change events (connect/disconnect/moved/orientation) and apply them
  in one pass (debounced ~50-100 ms), so a flapping replug converges to the final state.
- A replug that yields a NEW serial must remap bindings to the same logical position
  (slot) and drop the stale serial; actions follow the logical position (DS-112).
- Instant feedback: the map page (`deck_map.py` /state.json) updates on the same event.
- Tests with fakes: replug sequences (same serial, new serial, slot change while
  connected, orientation flip during a move), asserting convergence and no duplicate
  handlers. tests/run.py stays green.

## Acceptance

- `unittest discover` green incl. new reattach tests; TICKETS.md SD-008 updated with
  evidence. Live on-deck check is SALON-003's job (daemon now runs with --preview).

## Links

SALON-002, SALON-003, DS-112, DS-120, DS-128 sibling in deckstack: DS-126/DS-129.
