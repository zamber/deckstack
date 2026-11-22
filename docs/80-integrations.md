# 80 — Integrations (first: lights.lan, post-alpha)

Owner steering 2026-11-22 (delta 7): lights.lan is **reimplemented, not imported**. The
first integration is a deckstack plugin — separate process, manifest, JSON-Schema actions,
host-mediated HTTP (S4) — living in the separate `deckstack-lights` repo (`repos.toml`).
Post-alpha; tickets to be filed then.

## Shape

- Plugin process (`deckstack-lights`) with `manifest.json` (`schemas/plugin-manifest.json`):
  actions like `lights.set {room, light, on, brightness, color}` with JSON-Schema params,
  capability `net:lights.lan` only.
- All HTTP goes through the daemon's mediated `request kind:"net"` (`sdk.http`) — the
  daemon checks the host glob against the grant AND `/policy/net/allow_hosts`
  (`docs/contracts/plugins.md` §2, T11). The plugin never opens a socket.
- **Explicit IDs**: room/light identifiers live in `/plugins/lights/config` (state) or
  the plugin's own config — never hardcoded in core, never scraped from the old config.
- **No pre-baked scenes/presets** (scope delta 3): scenes are bindings + plugin state the
  user/agent creates; the daemon starts empty.
- **Policy example**: "only Salon lights may be driven" is *deployment policy*, expressed
  as data — `/policy/net/allow_hosts: ["lights.lan"]` plus per-room allowlists in
  `/plugins/lights/config/allow_rooms` enforced by the plugin — not a core assumption
  (BRIEF.md home-rule note; D18).

## Agent-first action surface

Agents drive lights through the same CLI the user has:
`ds plugin invoke lights.set '{"room":"salon","light":"ceiling","on":true,"brightness":80}'`
and subscribe to `deck/control/keys.0/down` bindings that call it — no special API.

## Tests

- Fake HueMux server in `tests/` (precedent `hubdeck/tests/fakes/ws_server.py`): the
  plugin is exercised end-to-end through the daemon with host-glob edge cases
  (`lights.lan.evil.com` → `E_CAP`, T11).
- Conformance: plugin manifest validates against `schemas/plugin-manifest.json`.

## Out of scope

No import of `~/.config/hubdeck` (`docs/70-porting-notes.md`), no HueMux-protocol
compatibility layer beyond what the HTTP API needs, no scenes shipped.
