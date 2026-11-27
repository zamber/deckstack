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
     ≥ 6.7 (RESOLVED DS-116: ABI 11 measured on the target kernel family), else wrapper-enforced path prefix checks.
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

## Implementation (DS-050, as landed)

Module layout in `crates/ds-daemon/src/plugins/`:

| file | role |
|---|---|
| `mod.rs` | global host registry + the router seam `invoke_with(state, bus, id, action, args)`; lazily creates the default host at `~/.local/share/deckstack/plugins/` on first plugin step; `shutdown_all()` kills every plugin process group on daemon exit (called from `lib.rs`) |
| `manifest.rs` | manifest struct + validation against `schemas/plugin-manifest.json` (jsonschema, draft 2020-12) + semantic rules (unique action ids, absolute-or-slash `command[0]`, bare names resolved against the fixed child PATH `/usr/local/bin:/usr/bin:/bin` — never the ambient PATH) + cap helpers (anchored `net:<glob>`, `fs:<root>` prefix) |
| `wire.rs` | newline-JSON messages (`hello`/`register`/`event`/`invoke`/`effects`/`request`/`reply`/`log`), glob matcher (`*` = exactly one DNS label, anchored, case-insensitive), topic→event-name mapping |
| `spawn.rs` | re-exec'd plugin child (same pattern as the DS-043 exec child — the workspace lints deny `unsafe_code`): `__ds-plugin-child` marker → rlimits (manifest `resources` or 10 s CPU / 64 nproc / 64 nofile / 1 GiB AS) → `setpgid(0,0)` → Landlock → argv-only `execve` with env `{PATH, HOME, LANG, DS_PLUGIN_TOKEN}`; pipes for stdin/stdout/stderr; reaper thread per child (no zombies); pid registry with `kill_all_plugin_groups()` |
| `host.rs` | `Host` (install/start/stop/remove/list/logs, event fan-out on the bus) + per-plugin supervisor thread: handshake with 5 s deadline, invoke with 2 s bounded timeout, watchdog (respawn ≤ 3 per 10 min, then `stopped` + `disabled_reason` + `plugin/<id>/disabled`), effects/request handling with per-item cap checks, host-mediated `net` (plain `http://`, URL ≤ 2048 B, body ≤ 1 MiB, 5 s timeout), `/plugins/<id>` state writes (the plugin-owned `state` subtree is preserved), 200-line log ring |
| `tests.rs` + `fixtures/` | in-crate tests; the fixture plugins are tiny Python scripts spawned through the real `ds-daemon` binary, so rlimits + Landlock + argv-only execve are exercised for real |

Landlock policy applied in the child (best-effort ABI detection via the `landlock`
crate; kernel ≥ 6.7 ⇒ ABI ≥ 4): handle all FS rights, grant read+execute on
`/usr /lib /lib64 /bin /sbin /etc /proc` and the plugin's own dir, read-write on the
manifest `fs_roots` (minus `Refer`), read-write on `/dev/null /dev/zero /dev/full
/dev/urandom /dev/random`; handle TCP bind/connect (ABI ≥ 4) and grant **nothing**, so
the plugin process has no direct network at all — the host-mediated `request
kind:"net"` is the only path (S4/T11). The child reports its restriction status on
stderr (`landlock FullyEnforced no_new_privs=true`), which lands in the plugin's log
ring. Where Landlock is unavailable (kernel without it) the host-enforced prefix checks
and the net mediation remain — the documented gap (docs/20-security.md T11).

Router seam: the DS-042 `plugin` step (`router/steps.rs`) calls
`plugins::invoke_with`, which finds the registered host serving the plugin id and
invokes `id.action` with the step's `args` (validated against the action's `params`
schema before anything is sent). The invoke wait is bounded by the 2 s timeout, so the
router thread never blocks unboundedly; with no host serving the plugin the typed
`E_CAP` stub is returned. Error mapping is normative in `docs/contracts/plugins.md` §9.

Tests (`cargo test -p ds-daemon plugins::`, 35 cases): handshake happy path + `/plugins`
exposure, effects landing in state, params-schema rejection, manifest rejection at
install, register-manifest mismatch → `E_SCHEMA` + kill, capability violation counted
with state untouched, crash mid-call → `E_DEVICE` + watchdog respawn, watchdog disable
after 3 respawns/10 min, invoke timeout → degraded → restart on second, plugin step
through the router engine, host-mediated net (allowed / glob-denied / policy-denied),
`get` requests, Landlock denying fs outside roots and raw TCP (with the no-Landlock
gap branch), event fan-out, stop-all kills the process group, remove, URL/HTTP
helpers, glob/writable-subset units.
