# Example plugins — quickstart for plugin authors

This directory contains the runnable form of the `docs/contracts/plugins.md` §7 example
plugin: `demo/` (a ~40-line Python counter via `ds-plugin-py`) plus `run_demo.py`, an
end-to-end demo that boots `ds-vdev` + `ds-daemon`, installs the plugin, invokes it and
asserts the effects. The Rust SDK (`crates/ds-plugin-sdk`, DS-051) speaks the same wire
protocol; this tree demonstrates the Python side (`plugins-py/`, DS-052) — pick either.

## Five-minute demo

```sh
cargo build --release --bin ds --bin ds-daemon --bin ds-vdev --bin ds-vdevctl
python3 examples/plugins/run_demo.py     # exit 0 = demo green, cleans up after itself
```

or by hand:

```sh
sh examples/plugins/demo/install.sh                       # -> ~/.local/share/deckstack/plugins/demo/
ds daemon start --device virtual:/tmp/vdev.sock &          # (start ds-vdev first, see docs/runbook.md)
ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}' --json
ds-vdevctl --socket /tmp/vdev.sock inject <press-file>     # key 0 down+up
ds get /controls/keys/0 --json                             # label "1", image colour #ff8800
```

Note: `ds plugin install …` / `ds plugin invoke …` are later-phase CLI verbs (they exit 1
in this tree, see `docs/runbook.md` §1). Today **install = copy the plugin dir into
`$XDG_DATA_HOME/deckstack/plugins/<id>/` with a valid `manifest.json`** (`demo/install.sh`
does exactly that) and **invoke = a binding step** `--do 'plugin <id>.<action> <json-args>'`
triggered by a device event. The daemon host starts every installed plugin automatically.

## The example plugin (`demo/plugin.py`)

```python
@sdk.action("bump", params={"type": "object", "properties": {"by": {"type": "integer", "default": 1}}})
def bump(args):
    count["n"] += args.get("by", 1)
    return [sdk.set("/controls/keys/0/label", str(count["n"])),
            sdk.set("/controls/keys/0/image", {"kind": "color", "value": "#ff8800"}),
            sdk.emit("plugin/demo/bumped", {"n": count["n"]})]
```

(the full file adds `get_lamp`, the `net:lights.lan` host-mediated HTTP read, and the
manifest wiring). Two deliberate deviations from the §7 snippet, both required by the
landed host/state: effect **paths are slash-separated** (`/controls/keys/0/label` — the
dotted form is the bus-topic form; the host applies effect paths verbatim against
`schemas/state.json`, where `/controls` admits only `keys|dial|knobs|faders|rollers`), and
`command`/`fs_roots`/`resources` are taken from the on-disk `manifest.json` so the
`register` message compares equal to it (contract §9).

## Manifest fields (`schemas/plugin-manifest.json` is normative)

| field | meaning |
|---|---|
| `id` | `^[A-Za-z0-9._-]{1,80}$`; the install dir name must equal it |
| `name`, `version` (`X.Y.Z`), `description?` | display metadata |
| `command[]` | argv array executed directly — never a shell; `command[0]` resolved against the fixed child PATH (`/usr/local/bin:/usr/bin:/bin`), never ambient |
| `actions[]` | `{id, params}` — `params` is the JSON Schema invoke args are validated against (`E_SCHEMA` on mismatch, before anything is sent) |
| `capabilities[]` | explicit grants, enforced per request/effect: `state.read` (default), `state.write`, `device.write`, `exec`, `emit`, `net:<glob>`, `fs:/<root>` |
| `events[]` | `keyDown keyUp dialRotate slide state willAppear willDisappear focus` |
| `resources{}` | rlimits at spawn; defaults `cpu_s` 10, `nproc` 64, `nofile` 64, `as_mb` 1024 |
| `fs_roots[]` | extra absolute roots the Landlock-sandboxed child may read/write (the plugin dir and `/usr /lib /bin …` are already granted) |

Generate the on-disk manifest from the code so the two never drift:
`python3 demo/make_manifest.py` (uses `sdk.build_manifest`, adds `command`/`fs_roots`/
`resources`).

## Capabilities and limits the host enforces (contract §2/§5/§9)

- `set` effects may only touch `/controls/…` or the plugin's own `/plugins/<id>/…`
  (`state.write`); every `set` is schema-validated on commit — invalid ⇒ rejected and
  counted in `/plugins/<id>/stats.violations`, state untouched. ≤ 100 effects per invoke.
- `emit` only topics under `plugin/<id>/…` (capability `emit`).
- `net:<glob>` is **host-mediated (S4/M5)**: the plugin never opens a socket;
  `sdk.http(method, url)` sends `request kind:"net"` and the daemon checks the URL host
  against the grant **and** `/policy/net/allow_hosts`, then performs the request itself.
  Glob: `*` = exactly one DNS label, anchored (`lights.lan` ≠ `a.b.lan` ≠
  `lights.lan.evil.com`). URL ≤ 2048 chars, body ≤ 1 MiB, reply ≤ 4 MiB.
  `demo.get_lamp` therefore needs `/policy/net/allow_hosts` to allow `lights.lan`
  (`ds set /policy/net/allow_hosts --json '["lights.lan"]'`) plus working DNS — the demo
  does not invoke it for exactly that reason.
- Invoke timeout 2 s ⇒ `degraded` (stops receiving events); second consecutive ⇒ restart.
  Watchdog: ≤ 3 respawns per 10 min, then `stopped` + `plugin/<id>/disabled`.
- Handshake: `register` within 5 s of `hello`, manifest schema-valid **and** equal to the
  on-disk `manifest.json`, else `E_SCHEMA` + kill. stderr goes to the daemon log / a
  200-line per-plugin ring; stdout is the protocol channel only.

## Wire protocol summary (contract §1 — newline-JSON on stdin/stdout)

`hello → register → event*/invoke* → effects|error`, plugin→host `request`
(`get`/`set`/`net`) answered by `reply`. Error mapping: unknown plugin/action `E_PATH`;
not running `E_DEVICE`; capability violation `E_CAP`; args/manifest/wire `E_SCHEMA`;
timeout `E_LIMIT`; internal `E_INTERNAL`. Lifecycle state is readable at `/plugins/<id>`
(`{manifest, status, state, stats}`).

## Wrapper guarantees (contract §6, honest limits)

`ds-plugin-py` guarantees: process isolation, lifecycle, the wire contract, resource
limits, host-mediated network. It does **not** guarantee engine internals
(`ctx.deck` depth — OPEN C1), in-process state sharing, or old-engine timing. Old
hubdeck plugins run through `ds_plugin_py.compat` in their own child process; nothing is
auto-imported — porting is ticket-driven post-alpha (`docs/70-porting-notes.md`).

## Files

```
examples/plugins/
  README.md            this quickstart
  run_demo.py          end-to-end demo (vdev + daemon + install + invoke + asserts)
  demo/
    plugin.py          the §7 example plugin (bump: colour+label+emit; get_lamp: net)
    make_manifest.py   generates manifest.json from plugin.py (register must equal it)
    install.sh         installs demo/ into $XDG_DATA_HOME/deckstack/plugins/demo/
```

The example is a test/example fixture only — it is never installed as a default
(empty-state invariant, `docs/00-scope.md`).
