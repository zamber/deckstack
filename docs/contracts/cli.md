# Contract: CLI (`ds`)

R1/R10. The conformance suite (`tests/conformance/`) executes this document against the
real binary. Design: ADR-002.

## 1. Grammar

```
ds [--socket PATH] [--token-file PATH] <group> <verb> [args] [flags]
ds --help | ds <group> --help | ds <group> <verb> --help     # always available
ds schema <state|ipc|plugin-manifest|binding>               # prints the JSON Schema
ds describe <path>                                           # schema + value + verbs for a path
```

Groups/verbs (all take `--json`; streaming verbs also `--ndjson`):

| Group | Verbs |
|---|---|
| `get` | `ds get <path> [--fields a,b] [--rev N]` |
| `set` | `ds set <path> <value> [--if-rev N] [--file f.png]` (value: JSON literal, or string/number; `--file` reads bytes → `{"kind":"file","path":…}` for images) |
| `bind` | `ds bind <topic-path> --do '<step>' [--do …] [--when '<cond>'] [--replace]` |
| `unbind` | `ds unbind <topic-path> [--all]` |
| `emit` | `ds emit <topic> [json-payload]` |
| `watch` | `ds watch <path-or-topic> [--ndjson] [--from-rev N] [--max-events N]` (persistent) |
| `events` | `ds events [--ndjson]` (subscribe `deck/#` + `app/#`) |
| `stream` | `ds stream push <target> --file f [--target panel|key:N|screen] [--dir d --fps N] [--loop]`; `ds stream stats <target>`; `ds stream stop <target>` |
| `plugin` | `ds plugin list|invoke <id.action> [args-json]|install <dir>|remove <id>|logs <id>` |
| `daemon` | `ds daemon start [--device virtual:PATH|auto] [--foreground]|stop|status|restart` |
| `token` | `ds token list|issue --scope read|mutate|admin|revoke <id>` |
| `test` | `ds test frame-sha <control-path>` (sha256 of last frame on ds-vdev) |
| `migrate` | `ds migrate import [--from ~/.config/hubdeck] [--dry-run]` (docs/70-migration.md) |

## 2. Output contract

- Default (human): one line per fact, stable, greppable. Example: `ds set …` prints
  `ok rev=42`.
- `--json`: exactly one JSON object on stdout, pretty by default; `--compact` for one line.
  Errors still go to **stderr** as one JSON object with `--json`, human text otherwise.
- `--ndjson` (streaming verbs only): one JSON object per line, no wrapping.
- stdout carries data only; logs/diagnostics → stderr.

## 3. Exit codes

`0` ok · `1` usage/schema error (bad args, value fails schema) · `2` path not found
(`E_PATH`) · `3` conflict (`E_CONFLICT`, payload carries `current_rev`) · `4` auth/cap
(`E_AUTH`/`E_CAP`) · `5` limit (`E_LIMIT`) · `6` daemon unreachable (`E_UNREACHABLE`,
message includes the exact `ds daemon start` command) · `7` device error (`E_DEVICE`) ·
`8` internal/version.

## 4. Setting every attribute & binding behaviour (completeness check, R3)

```sh
ds set /controls/keys/0/image --json '{"kind":"color","value":"#ff0000"}'
ds set /controls/keys/0/image --file ~/pic.png            # stored under media root, re-encoded on push
ds set /controls/keys/0/image --json '{"kind":"icon","name":"tabler:bolt"}'
ds set /controls/keys/0/label '"Salon"'
ds set /controls/dial/image --file dial.gif               # animation: ≤300 frames, ≤30 fps
ds set /controls/knobs/0/led --json '{"mode":"static","color":"#3cc86e","brightness":40}'
ds set /controls/keys/3/brightness 80                     # screen backlight 1-100, never 0 on dial
ds bind /controls/keys/0/down --do 'plugin demo.bump {}'
ds bind /controls/keys/1/down --do 'set /controls/keys/1/color "#f00"' --do 'emit demo.red'
ds bind /controls/dial/rotate --do 'plugin mixer.adjust {"delta": 1}' --when '/focus/app exists'
ds bind app/focus/changed --when '/focus/app eq code' --do 'set /controls/keys/3/color "#0f0"'
ds set /policy/allow/net/hosts --json '["lights.lan"]'     # admin scope; the Salon rule (D18)
ds watch /bindings --ndjson                                # observe binding changes
```

Everything the hardware can display (key image, panel image, dial image, solid colour,
backlight, LED mode/color/brightness per zone) is reachable via `set`/`stream` without
forking the core — R3.

## 5. Error text (actionable, stable prefix)

```
$ ds set /controls/keys/99/color '#fff'
ds: path not found: /controls/keys/99 (have 0-14; see `ds describe /controls/keys`)  [exit 2]
$ ds daemon status   # daemon down
ds: daemon unreachable at $XDG_RUNTIME_DIR/deckstack/daemon.sock — start it with: ds daemon start  [exit 6]
$ ds set /controls/keys/0/image --json '{"kind":"color","value":"red"}'
ds: schema error at /controls/keys/0/image.value: "red" does not match ^#[0-9a-fA-F]{6}$  [exit 1]
```

## 6. Composition examples (R4)

```sh
# shell
ds get /controls/keys/0 --json | jq -r .value.label | tr 'a-z' 'A-Z' | xargs -I{} ds emit app/label '{}'
# one command's output feeds another's arguments
ds get /controls/knobs/0/led --json | jq -c '{kind:"color", value:.value.color}' | xargs -0 -I{} ds set /controls/keys/5/image --json {}
# PowerShell
$led = ds get /controls/knobs/0/led --json | ConvertFrom-Json
#   note: PowerShell quoting — pass JSON as a single argument:
ds set /controls/keys/5/image --json (@{kind="color"; value=$led.value.color} | ConvertTo-Json -Compress)
# chaining: focus change recolours a key (the R4 chain)
#   ds-focus emits app/focus/changed; binding does the rest — see docs/contracts/actions.md §6
# cron example: dim every night
ds bind app/cron/nightly --do 'set /settings/dim 0.2'    # cron job: ds emit app/cron/nightly
```
