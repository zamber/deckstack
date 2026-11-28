# Ticket DS-126 — preview webapp: frame-stream perf, no flicker, instant reconnect re-render, roller/dial graphics

Date: 2026-11-28. Type: client fix in `clients/preview-web/` (DS-121/DS-124 scope).

## Reported issues (owner, 2026-11-28, live deck on port 43541)

1. **Frame image flickers / resets**: every WS event re-runs `renderDeck()`, which
   recreates the module `<g>` elements — destroying the `<image>` elements that hold
   the streamed frame, so the screen blanks until the next `frame` response arrives.
2. **Streaming start is slow** — polling starts only after the snapshot and at 400 ms;
   feels broken on first open.
3. **Dial SVG graphic obscures the dial's screen frame** — make the dial graphic an
   outline only (or remove it), the streamed screen must be visible.
4. **Roller graphic orientation wrong** — the two roller wheels must sit side by side
   along the module's LONG axis (in the rot-0 frame that is the 4-cell axis; after the
   module rotation they appear side by side on the long edge), not stacked.
5. **Reconnect does not remount**: after unplug/replug (slot or even serial changes) the
   deck map must re-solve and re-render instantly — handle `/devices` events for
   connect/disconnect/slot-change/serial-change robustly (a replug may produce a NEW
   serial; the old one goes `disconnected`), no stale modules left on the deck.

## Requirements

- Cache the last frame (data URL + sha) per target kind OUTSIDE the re-created DOM;
  after every `renderDeck()` re-apply cached frames to the new `<image>` elements.
  The visible screen must never blank between frames.
- Start `frame` polling immediately on `preview.hello` (before/without waiting for the
  snapshot), interval <= 150 ms, and keep the previous frame displayed while a new one
  is in flight.
- Dial: outline-only ring + tick (no filled disc) over the streamed screen.
- Rollers: two ribbed wheels along the long axis of the unrotated frame.
- Re-render on every `/devices` event (already via subscribe); assert the solver output
  changes when a slot changes. Add a small "devices changed" affordance if useful.
- Keep WS-only transport, cache-busted asset versions.

## Acceptance

- Playwright against the vdev (port 39955, token in verification-live-ds125.md) AND
  the live deck (port 43541): screenshots showing no blank screen while frames stream;
  `ds-vdevctl move` / `replug` sequences re-render the map within one event round-trip;
  frame stream starts < 1 s after page open.
- Evidence captures committed under `docs/discovery/salon-001/`.

## Links

DS-121, DS-124, DS-125, DS-129 (e2e test), SALON-003.
