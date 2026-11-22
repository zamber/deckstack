# 70 — Migration from hubdeck (Python AppImage) and hubdeck-upstream

## 1. What exists today (evidence)

- Live config: `~/.config/hubdeck/config.yaml` + `sections/*.yaml` (includes), YAML 1.2,
  `!secret` from `secrets.yaml`/env, pydantic-validated with `extra="forbid"`
  (`hubdeck/.../config/loader.py:26-33,60-70,73-211`, `schema.py:66-67`); presets with
  `extends` chains and recursive overlay, `None` deletes (`schema.py:604-613,650-705`).
  Current machine: `plugins.huemux.url: http://lights.lan`, `host: isolated`;
  `presets.default.extends: [salon-pages, salon-controls]`; `start_preset: default`.
- Running as an AppImage; single-instance lock (`hubdeck/.../platforms/posix.py:51-62`);
  control channel on a random loopback port + `control.json` (`control.py:81-90`).
- Action vocabulary: `ALIASES` + `REGISTRY` (`config/schema.py:55-67`,
  `plugins/__init__.py:22-28`); `{python: …}` exec actions (`plugins/python.py:58`).
- Rust upstream: JSON profiles in `<data>/profiles/<id>.json` (`upstream/.../engine/profile.rs:10,222-281`),
  export `.mhz` (`engine/mhz.rs`).

## 2. Import (`ds migrate import`)

Reads `~/.config/hubdeck` (and/or an upstream data dir) and writes deckstack state:

| hubdeck source | deckstack target |
|---|---|
| `presets.<name>` / `pages` / `sections` | `/settings/presets/<name>` (pure data; switching = `ds set /active/preset <name>` + re-apply) |
| per-control `face` (style/image/label) | `/controls/<c>/image|label|color` |
| action aliases (`run`, `keys`, `focus`, `python`, …) | bindings with `exec` (for `launch.*`: argv from the config, requires admin + allowlist) or plugin refs; **`{python: …}` is NOT imported** — emitted as a porting TODO entry (R8) |
| `plugins:` section (huemux url etc.) | plugin install + `/plugins/<id>/config` (secrets → `ds token`-style env, never plaintext in state) |
| `lighting:` | `/controls/{knobs,faders,base}/led` |
| `auto_switch` / `match` | bindings on `app/focus/changed` (generated) |
| upstream `profiles/*.json` / `.mhz` | same mapping via a second importer |

`--dry-run` prints the mapping as JSON without writing. Unknown constructs → warning
list, never silent.

## 3. Coexistence and reversible cutover

The HID device has effectively one owner (macOS opens are exclusive,
`hubdeck/docs/masterhub-protocol.md:36-37`; on Linux the vendor MHDaemon must be closed
too, `hubdeck/CLAUDE.md:66`). So coexistence is **sequential, not concurrent**:

```sh
# cutover
ds migrate import --from ~/.config/hubdeck          # writes ~/.local/state/deckstack/state.json
systemctl --user stop hubdeck-tray  # or: quit the AppImage tray   (releases HID)
ds daemon start                                      # takes the device, pushes imported state
# verify, then rollback if needed:
ds daemon stop                                       # guard hands back to firmware mode
ds-daemon --guard child exits; start the AppImage again — config untouched
```

Reversible because nothing in `~/.config/hubdeck` is modified — deckstack only reads it;
rollback = start the old app. The daemon's guard child performs firmware handback on
crash (`upstream/.../src/guard.rs`), so even a killed daemon leaves the deck usable by
the old app.

## 4. What is lost (explicit)

- `{python: …}` python-in-yaml actions — rejected by R8; replaced by external scripts
  invoked through `exec` bindings or plugins (porting list from the importer).
- The web editor GUI (pywebview) and its comment-preserving YAML round-trip
  (`hubdeck/.../gui/doc.py`) — replaced by ds-web (phase 7) editing state via the API;
  hand-editing = `ds set`/`ds get` or editing `state.json` while the daemon is stopped.
- mtime hot-reload of YAML (1 s poll, `engine.py:469-472`) — replaced by transactional
  `set` (immediate) — strictly better, but file-edit-while-running no longer reloads.
- Upstream-only: Stream Deck plugin compat layer, wgpu canvas, Tauri UI — not ported
  (walled-garden features being replaced).
- Remote (socketpair) plugin mode specifics (`plugins/host/remote.py`) — replaced by
  ds-plugin-py (see docs/contracts/plugins.md §6; OPEN C1 on `ctx.deck` depth).

## 5. First end-to-end slice

Phase 1 (PLAN.md) on ds-vdev: daemon + CLI + one key image round-trip. First *real-user*
slice = phase 8 dry-run: `ds migrate import` of the live Salon config onto ds-vdev,
knob turn → (fake) lamp — proving the migration path before hardware cutover.
