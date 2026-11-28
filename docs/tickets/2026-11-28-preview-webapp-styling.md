# Ticket DS-127 — preview webapp styling after the upstream hubdeck look

Date: 2026-11-28. Type: client styling in `clients/preview-web/`.

## Motivation

The owner finds the current green module outlines irritating and wants the deck map to
look like the upstream hubdeck app screenshot:
https://github.com/Przemotar/hubdeck/blob/main/docs/images/hubdeck-main.png
(local copy: /var/home/catwars/repos/hubdeck-upstream — find the corresponding asset or
read `app/src/components/DeckView.tsx` + `deck.css` for the exact palette/gradients:
dark bevelled module bodies, recessed trays, subtle highlights, no bright green
outlines; the base slab dark with a fine grid).

## Requirements

- Module bodies: dark gradient/bevel like DeckView's `ModuleBody` (no green stroke;
  disconnected modules get a subtle red dashed stroke only).
- Slab: dark, fine grid, neutral label.
- Screens/frames keep streaming; knobs/dial/rollers/faders keep their current geometry
  (DS-126 fixes rollers/dial), restyle to match: recessed trays, ribbed rollers,
  fader caps, knob discs with ticks.
- Keep it dependency-free vanilla SVG; no build step.

## Acceptance

- Side-by-side screenshot vs the hubdeck-main.png look, committed under
  docs/discovery/salon-001/; owner-visible resemblance, no green outlines.

## Links

DS-121, DS-124, DS-126.
