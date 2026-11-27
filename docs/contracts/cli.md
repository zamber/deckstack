# Contract: CLI (`ds`)

R1/R10. The conformance suite (`tests/conformance/`) executes this document against the
real binary. Design: ADR-002.

## 1. Grammar

```
ds [--socket PATH] [--token-file PATH] <group> <verb> [args] [flags]
ds --help | ds <group> --help | ds <group> <verb> --help
ds schema <state|ipc|stream-frame|plugin-manifest|binding|action-metadata|preview>
ds describe <path>
```

| Group | Verbs |
|---|---|
| `get` | `ds get <path> [--fields a,b] [--rev N]` — `/telemetry/*` works and returns no rev |
| `set` | `ds set <path> <value> [--if-rev N] [--file f]` |
| `colors` | `ds colors '<json-array of 15 "#rrggbb">' [--if-rev N]` — bulk key colours: one `set /controls/panel/image` with the panel-only `{"kind":"colors","value":[…]}` visual; the daemon composes the 480x272 panel and sends one `02 35` push (DS-111). Wrong JSON/arity/hex exits 1 before the daemon is contacted |
| `bind` | `ds bind <topic> --do '<step>' [--do …] [--when '<cond>']` → prints the new binding id (`bNNNN`) |
| `unbind` | `ds unbind <id> | --topic <topic>` |
| `emit` | `ds emit <topic> [json-payload]` |
| `watch` | `ds watch <path-or-topic> [--ndjson] [--from-rev N] [--max-events N]` — persistent |
| `events` | `ds events [--ndjson]` |
| `stream` | `ds stream push <target> --file f [--dir d --fps N] [--loop]` · `ds stream stats <target>` · `ds stream stop <target>` — target ∈ `keys\|key:N\|dial` (S6) |
| `plugin` | `ds plugin list\|invoke <id.action> [args]\|install <dir>\|remove <id>\|logs <id>` |
| `daemon` | `ds daemon start [--device virtual:PATH\|auto] [--foreground]\|stop\|status\|restart` (m9) |
| `token` | `ds token list\|issue --scope read\|mutate\|admin\|revoke <id>` |

There is deliberately **no `migrate` verb** (scope delta 2): `ds migrate …` exits 1.
Porting of old-app behaviour is manual and ticket-driven (`docs/70-porting-notes.md`).

`ds-vdevctl` (part of the ds-vdev binary, test/CI tool) replaces the old test-only verb:
`ds-vdevctl --socket /tmp/vdev.sock frames <target>` → sha256 + bytes;
`ds-vdevctl … writes` → full report log; `ds-vdevctl … inject events.ndjson` (S10/m10).
There is no `ds test` verb in the production CLI.

## 2. Output contract

- Human default: one line per fact (`ok rev=42`).
- `--json`: exactly one JSON object on stdout (pretty; `--compact` for one line). Errors →
  stderr as one JSON object with `--json`, human text otherwise. stdout = data only.
- **Streaming verbs (`watch`, `events`, and any subscription) accept only `--ndjson`;
  passing `--json` to them is a usage error, exit 1** (S9/m1). `--ndjson`: one JSON
  object per line, no wrapper.

## 3. Exit codes

`0` ok · `1` usage/schema (incl. `--json` on a streaming verb) · `2` `E_PATH` ·
`3` `E_CONFLICT` (payload has `current_rev`) · `4` `E_AUTH`/`E_SCOPE`/`E_CAP` ·
`5` `E_LIMIT` · `6` `E_UNREACHABLE` (message includes the exact `ds daemon start` command) ·
`7` `E_DEVICE` · `8` `E_INTERNAL`/`E_VERSION`.

## 4. Setting every attribute & binding behaviour (R3 completeness)

```sh
ds set /controls/keys/0/image --json '{"kind":"color","value":"#ff0000"}'
ds colors '["#ff0000","#00ff00","#0000ff", … 15 total]'   # one rev, one panel push (DS-111)
ds set /controls/panel/image --json '{"kind":"colors","value":["#ff0000", … 15]}'  # the same write
ds set /controls/keys/0/image --file ~/pic.png            # confined to /policy/fs/roots
ds set /controls/keys/0/image --json '{"kind":"icon","name":"tabler:bolt"}'
ds set /controls/keys/0/label '"Salon"'
ds stream push dial --file anim.gif --loop                # ≤300 frames, ≤30 fps
ds set /controls/knobs/0/led --json '{"mode":"static","color":"#3cc86e","brightness":40}'
ds set /controls/keys/3/brightness 80                     # backlight 1-100; 0 refused on dial
ds bind deck/control/keys.0/down --do 'plugin demo.bump {"by":1}'
ds bind deck/control/keys.1/down --do 'set /controls/keys/1/image {"kind":"color","value":"#ff0000"}' --do 'emit app/demo/red {}'
ds bind deck/control/dial/rotate --when '{"payload":"delta","op":"gt","value":0}' --do 'plugin mixer.adjust {"delta":1}'
ds bind app/focus/changed --when '{"payload":"app","op":"eq","value":"code"}' --do 'set /controls/keys/3/image {"kind":"color","value":"#00ff00"}'
ds set /policy/net/allow_hosts --json '["lights.lan"]'     # admin scope; the Salon rule (D18)
ds watch /bindings --ndjson
```

Every hardware-displayable attribute (key/panel/dial image, solid colour, backlight,
LED mode/colour/brightness per zone) is reachable via `set`/`stream` without forking core — R3.

## 5. Error text

```
$ ds get /controls/keys/99
ds: path not found: /controls/keys/99 (valid key indices are 0-14; see `ds describe /controls/keys`)  [exit 2]
$ ds daemon status   # daemon down
ds: daemon unreachable at $XDG_RUNTIME_DIR/deckstack/daemon.sock — start it with: ds daemon start  [exit 6]
$ ds set /controls/keys/0/image --json '{"kind":"color","value":"red"}'
ds: schema error at /controls/keys/0/image.value: "red" does not match ^#[0-9a-fA-F]{6}$  [exit 1]
$ ds watch /controls --json
ds: --json is not valid for streaming verbs; use --ndjson  [exit 1]
```

## 6. Composition examples (R4)

```sh
# shell: one command's output feeds another's arguments
ds get /controls/knobs/0/led --json | jq -c '{kind:"color", value:.value.color}' | xargs -0 -I{} ds set /controls/keys/5/image --json {}
ds get /controls/keys/0 --json | jq -r .value.label | tr 'a-z' 'A-Z' | xargs -I{} ds emit app/label '{}'
# PowerShell (pass JSON as ONE argument):
$led = ds get /controls/knobs/0/led --json | ConvertFrom-Json
ds set /controls/keys/5/image --json (@{kind="color"; value=$led.value.color} | ConvertTo-Json -Compress)
# cron: ds emit app/cron/nightly   →  ds bind app/cron/nightly --do 'set /settings/dim 0.2'
# focus chain: ds-focus emits app/focus/changed; bindings do the rest (actions.md §6)
```
