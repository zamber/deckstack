# Ticket DS-124 — preview webapp: solver-driven faithful deck map + live events

Date: 2026-11-28. Type: client app change in `clients/preview-web/` (DS-121 scope).

## Motivation

The DS-121 webapp currently renders with the upstream slot table and a first-free
fallback; the owner reports positions/rotations wrong on the real deck. The hardware's
own rotation readout is unreliable, so the app must deduce placement via the DS-123
solver over the DS-122 matrix.

## Requirements

1. Replace `slots.json` (upstream table) with the DS-122 placement matrix; implement
   the DS-123 solver in `app.js` (vanilla JS, no deps).
2. Render on the grid in the base's current orientation (flat 6x4 / vertical 4x6
   transposed), modules at deduced col/row/rotation, footprints keys 4x3, knobs/dial
   2x2, rollers 1x4, faders 2x4; content drawn in the module's own rotated frame
   (fader caps along the long axis, roller ribs along the long axis, knob triangle
   1-top/2-3-bottom at rot 0, dial knob + screen frame).
3. Unplaceable/disconnected modules are NOT drawn on the deck; listed in a
   "not on deck" strip with kind + slot + reason.
4. Live updates over `/preview/ws`: device reseat (slot/connection changes), base
   orientation flips, control value events (faders/knobs/dial/rollers) re-render the
   map and values without reload; screen frames via the `frame` op keep streaming.
5. Visuals in the spirit of the upstream hubdeck DeckView (dark bevelled module
   bodies, recessed trays) but with OUR placement.
6. Cache-busting asset query versions bumped on change.

## Acceptance

- Against ds-vdev (all reference modules + moved modules via `ds-vdevctl move`):
  render matches the solver output exactly; reseat/rotation events update the map live
  (screenshot evidence).
- Against the real deck (hid:) when its daemon has `--preview`: the drawn layout
  matches the camera view (DS-125).

## Links

DS-121, DS-122, DS-123, DS-125, SALON-001, SALON-002.
