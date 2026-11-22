# Component: plugin host + SDK (in-daemon module; SDKs per language)

Contract: `docs/contracts/plugins.md`; manifest schema: `schemas/plugin-manifest.json`.

- **Model:** one OS process per plugin (`~/.local/share/deckstack/plugins/<id>/`),
  newline-JSON over stdio (portable; Windows uses pipes), manifest declares actions with
  JSON-Schema params, plugin receives events, returns **effects** the daemon applies
  (ADR-006, D08). Watchdog respawns ≤ 3 per 10 min (pattern:
  `upstream/app/src-tauri/src/engine/plugins.rs:454-495`); kill-on-exit for orphans
  (`:529-558`).
- **Language:** host module is Rust inside ds-daemon. Plugins may be any language; SDKs:
  Rust (`ds-plugin-sdk`), Python (`ds-plugin-py`, doubles as the hubdeck-compat wrapper,
  D09). Rejected: in-process plugins (`hubdeck/src/hubdeck/plugins/base.py` — one plugin
  corrupts the daemon), upstream's WS-to-API with declared-only permissions
  (`upstream/.../plugins.rs:56-58` — "does not sandbox plugins").
- **Public surface:** manifest (`schemas/plugin-manifest.json`): `{id, name, version,
  command, actions[], capabilities[], resources{}}`. Wire: `hello → register → events
  (keyDown/keyUp/rotate/slide/state/willAppear/…) → invoke {id, args} → effects[]`.
  Plugin→host requests: `get/set state`, `emit`, `log`, `request` (capability-gated).
- **Invariants:**
  1. Plugin I/O is schema-validated against the manifest before touching state (R6).
  2. Every plugin runs with rlimits (nproc, nofile, AS, CPU) + Landlock fs policy on Linux
     ≥ 6.7 where available (OPEN O6), else wrapper-enforced path prefix checks.
  3. A plugin gets a capability token; `exec`, `net`, `fs:<root>`, `device.write` are
     explicit grants, default none.
  4. Plugin quirks stay in the plugin: the daemon never special-cases a plugin id.
  5. Timeout: invoke effects awaited ≤ 2 s (matches `ASK_SECONDS = 2.0`,
     `hubdeck/src/hubdeck/engine.py:336-344`); on timeout the plugin is marked
     `degraded`, not killed (first offense).
- **Failure modes:** crash/hang → watchdog kill + respawn with backoff; effect schema
  violation → effect rejected + logged, daemon state untouched; capability violation →
  `E_CAP` reply, counted in `/plugins/<id>/stats.violations`.
- **Python wrapper (D09):** `ds-plugin-py` runs an existing hubdeck plugin class
  (`plugins/base.py:112-272` interface: `actions/states/values`, `invoke/adjust/set`,
  `watch/watch_states`) inside a dedicated process and translates to the ds wire
  protocol. Guarantees: process isolation, lifecycle, wire contract, resource limits.
  Does NOT guarantee: deep compatibility with engine internals (`ctx.deck` internals,
  in-process state sharing) — those plugins are ported, and the porting list is a
  porting list is produced per ticket post-alpha (`docs/70-porting-notes.md`).
- **Dependencies:** ds-daemon internals, `schemas/plugin-manifest.json`, `jsonschema`.
- **Test command:** `cargo test -p ds-daemon::plugins` + conformance
  `tests/conformance/plugins/` (crash, fork-bomb, oversized effect, cap violation —
  see docs/20-security.md test cases).
- **Minimum working example:** see `docs/contracts/plugins.md` §Example plugin
  (a 40-line Python counter plugin).
