# ds-plugin-sdk

The Rust SDK for [deckstack](https://deckstack.dev) plugins. A deckstack plugin is **one OS
process** that speaks a **newline-JSON wire protocol over stdin/stdout** with the daemon's
plugin host (`docs/contracts/plugins.md` §1; the host contract is implemented by DS-050 in
`ds-daemon`). This crate implements the plugin side of that protocol: handshake, registration,
action dispatch, effect builders, host-mediated state and network requests, and clean shutdown.

- Contract: `docs/contracts/plugins.md` (normative), manifest schema
  `schemas/plugin-manifest.json` (normative).
- Wire version: `protocol: 1` in `hello`. A mismatch is surfaced cleanly (no output, exit 8);
  the host kills mismatched plugins.
- Dependencies: `serde` + `serde_json` only.

## Writing a plugin

```rust,no_run
use ds_plugin_sdk::{log, set, Capability, EventKind, Manifest, Plugin};
use serde_json::json;

fn main() {
    let manifest = Manifest::new("demo", "Demo counter", "0.1.0")
        .description("counts key presses")
        .capability(Capability::StateWrite)                    // state.write
        .capability(Capability::Net("lights.lan".to_string())) // net:lights.lan
        .event(EventKind::KeyDown);                            // subscribe to keyDown

    let plugin = Plugin::new(manifest)
        // Raw-Value handler:
        .action(
            "bump",
            json!({"type":"object","properties":{"by":{"type":"integer","default":1}}}),
            |_sdk, args| {
                let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
                Ok(vec![
                    set("/controls/keys.0/label", by.to_string()),
                    set("/controls/keys.0/image", json!({"kind":"color","value":"#ff8800"})),
                ])
            },
        )
        // Type-safe handler (args deserialized into your struct after schema validation):
        .action_typed::<Bump>("bump_typed", json!({"type":"object","required":["by"]}),
            |_sdk, a: Bump| Ok(vec![set("/plugins/demo/n", a.by)])),
        // Host-mediated HTTP (S4) and state access, callable from any handler via &mut Sdk:
        .action("get_lamp", json!({"type":"object"}), |sdk, _args| {
            let r = sdk.http("GET", "http://lights.lan/api/lamp")?;   // request kind:"net"
            let v = sdk.get_state("/controls/keys.0")?;               // request kind:"get"
            sdk.set_state("/plugins/demo/state/last", r.body)?;       // request kind:"set"
            Ok(vec![log("info", format!("lamp={} keys={v}", r.body))])
        })
        .on_event(|_sdk, ev| Ok(vec![log("info", format!("saw {}", ev.event))]));

    // `serve_stdio` speaks the protocol on stdin/stdout; `serve` is the no-action shortcut:
    //   ds_plugin_sdk::serve(manifest)?;
    match plugin.serve_stdio() {
        Ok(()) => std::process::exit(0),
        Err(e) => { eprintln!("my-plugin: {e}"); std::process::exit(e.exit_code()); }
    }
}

#[derive(serde::Deserialize)]
struct Bump { by: i64 }
```

A runnable version of this is `src/bin/demo_plugin.rs`:

```sh
cargo run -p ds-plugin-sdk --bin demo-plugin <<'EOF'
{"op":"hello","protocol":1,"plugin_token":"t","caps":["state.write","net:lights.lan"],"roots":{},"limits":{}}
{"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}
{"op":"shutdown"}
EOF
```

## The wire protocol as spoken by this SDK

One JSON object per line. Plugin→host structs serialize in the documented field order; the
contract tests (`tests/contract/plugins/plugins.rs`) pin every message **byte-for-byte**.

| direction | message |
|---|---|
| host→plugin | `{"op":"hello","protocol":1,"plugin_token":"…","caps":[…],"roots":{…},"limits":{…}}` |
| plugin→host | `{"op":"register","id":"demo","manifest":{…}}` |
| host→plugin | `{"op":"event","event":"keyDown","control":"keys.0","state":{…}}` (optional `request_id`) |
| host→plugin | `{"op":"invoke","id":"demo.bump","args":{…},"request_id":7}` |
| plugin→host | `{"op":"effects","request_id":7,"effects":[{"effect":"set","path":"…","value":…}]}` |
| plugin→host | `{"op":"error","request_id":7,"error":{"code":"E_SCHEMA","message":"…"}}` |
| plugin→host | `{"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}` |
| plugin→host | `{"op":"request","request_id":9,"kind":"set","path":"…","value":…}` |
| plugin→host | `{"op":"request","request_id":10,"kind":"net","method":"GET","url":"http://lights.lan/api/…","body"?:…,"headers"?:…}` |
| host→plugin | `{"op":"reply","request_id":10,"ok":true,"status":200,"body":"…"}` (or `"value"`/`"rev"` for get/set, or `"ok":false,"error":{…}`) |

Notes on shapes the contract leaves open (chosen here, pinned by the contract tests, flagged
for the steward — see "Contract notes" below):

- **Invoke/event failure** → `{"op":"error","request_id":N,"error":{"code","message"}}`.
- **Event replies** carry `request_id` only when the event had one; otherwise the `effects`
  message is fire-and-forget without `request_id`.
- **`get` reply** payload is `value` (+ optional `rev`); **`set` reply** is `rev`.
- **Shutdown**: EOF on stdin (daemon exit closes the pipe) is the clean shutdown; an explicit
  `{"op":"shutdown"}` (or `"stop"`) op is also honored. Exit code 0 either way.
- **Plugin-side request ids** count from 1 in the plugin→host direction, independent of the
  host's `request_id`s on invokes.

Malformed host input (bad JSON, unknown op, line > 1 MiB) is logged to stderr and skipped — a
protocol violation never panics the plugin. The first host message must be `hello` with
`protocol: 1`.

## Capabilities model

Capabilities are **declared in the manifest** (`schemas/plugin-manifest.json`) and **enforced
by the host** per request/effect — never just displayed:

| capability | wire spelling | what it gates |
|---|---|---|
| `StateRead` | `state.read` | `get_state` / `kind:"get"` requests |
| `StateWrite` | `state.write` | `set` effects, `set_state` / `kind:"set"` requests |
| `DeviceWrite` | `device.write` | device effects |
| `Exec` | `exec` | exec (host-side policy `/policy/exec`) |
| `Emit` | `emit` | `emit` effects |
| `Net(glob)` | `net:<glob>` | `http()` / `kind:"net"` requests — **host-mediated** (S4): the plugin never opens a socket; the daemon resolves the URL's host, matches it against the glob (`*` = exactly one DNS label, anchored) and `/policy/net/allow_hosts`, then performs the request itself |
| `Fs(root)` | `fs:<root>` | filesystem roots (Landlock / host-enforced) |

Effect builders: `set(path, value)`, `emit(topic, payload)`, `log(level, msg)`, `sleep(ms)`. The `hello` message carries the caps actually granted for this session; handlers can inspect
them via `Sdk::granted_capabilities()` (informational — the host is the enforcer). A denied
request comes back as `{"ok":false,"error":{"code":"E_CAP",…}}` and surfaces from
`Sdk::get_state`/`set_state`/`http` as a `PluginError` with code `E_CAP`.

### Bounded `http()`

The contract allows host-mediated network (S4/M5), so `Sdk::http(method, url)` exists and is
bounded: method ∈ `GET HEAD POST PUT PATCH DELETE`, URL ≤ 2048 chars, body ≤ 1 MiB, ≤ 16
headers, reply body ≤ 4 MiB (the host additionally caps at its own `MAX_BODY`). Violations
raise `E_LIMIT`/`E_SCHEMA` **before anything is sent**. `HttpResponse { status, body }` with
`body_json()` for JSON bodies.

## Error mapping

Handler errors and SDK-side failures use the stable codes of `schemas/ipc-message.json`
(`ErrorCode`, mirrored here so the crate stays serde-only):

| code | meaning on the plugin wire | exit code (fatal serve errors) |
|---|---|---|
| `E_SCHEMA` | unknown action, args violate the declared `params` schema, bad manifest, non-hello first message | 1 |
| `E_PATH` | (host-side) path not found | 2 |
| `E_CONFLICT` | `if_rev` mismatch | 3 |
| `E_AUTH`/`E_SCOPE`/`E_CAP` | token/scope/capability (host replies) | 4 |
| `E_LIMIT` | oversized URL/body/headers/line/reply | 5 |
| `E_UNREACHABLE` | host closed stdin while a request was in flight | 6 |
| `E_DEVICE` | hardware (host-side) | 7 |
| `E_VERSION` | `hello.protocol != 1` | 8 |
| `E_INTERNAL` | handler bug / unexpected reply shape / I/O error | 8 |

Fatal `ServeError`s (bad manifest, wrong first message, version mismatch, EOF before hello)
print to stderr and exit with the mapped code; clean shutdown (EOF or `shutdown` op) exits 0.

## Params validation

The host validates `args` against the action's full JSON Schema (draft 2020-12) before every
invoke. The SDK additionally checks the documented subset (`type`, `properties`, `required`,
`additionalProperties:false`, `enum`, `const`, `items`, `min/maxItems`, `min/maxLength`,
`minimum`/`maximum`) before dispatch, so a bad payload becomes an `E_SCHEMA` error reply
instead of a panic. `pattern` is **not** evaluated SDK-side (no regex dependency) — the host
remains the enforcing validator.

## Testing

```sh
cargo test -p ds-plugin-sdk            # unit tests + doc tests
cargo test -p ds-plugin-sdk --test plugins   # byte-for-byte contract tests vs a mock host
cargo test --test plugins              # same, from the workspace root
```

The contract corpus lives in `tests/contract/plugins/` (see its README) and runs the example
plugin binary `src/bin/demo_plugin.rs` against a mock host — no daemon needed.

## Contract notes / ambiguities (for the steward)

`docs/contracts/plugins.md` §1 pins the shapes in the table above; the following were open
and are decided here (pinned by `tests/contract/plugins/plugins.rs`, to be reconciled with
the DS-050 host):

1. Invoke/event **error reply** is a separate op `{"op":"error","request_id":N,"error":{…}}`
   rather than an `effects` message with an error field.
2. **Shutdown** is EOF on stdin (lifecycle: "daemon exit kills all plugin processes"); an
   explicit `{"op":"shutdown"}`/`{"op":"stop"}` op is accepted as well.
3. **`get`/`set` reply payloads**: `value` (+`rev`) for `kind:"get"`, `rev` for `kind:"set"`;
   the contract only shows the `net` reply (`status`/`body`).
4. **Event effects** without a host `request_id` are sent without `request_id`.
5. **Plugin request ids** are an independent counter starting at 1.
