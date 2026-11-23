//! `demo-plugin` — the example deckstack plugin (mirrors `docs/contracts/plugins.md` §7).
//!
//! Run it by hand and speak the protocol on its stdin/stdout, e.g.:
//!
//! ```sh
//! printf '%s\n' \
//!   '{"op":"hello","protocol":1,"plugin_token":"t","caps":["state.write","net:lights.lan"],"roots":{},"limits":{}}' \
//!   '{"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}' \
//!   '{"op":"shutdown"}' \
//!   | cargo run -p ds-plugin-sdk --bin demo-plugin
//! ```
//!
//! The contract test `tests/contract/plugins/plugins.rs` runs this binary against a mock
//! host and asserts the wire messages byte-for-byte.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ds_plugin_sdk::{log, set, Capability, EventKind, Manifest, Plugin};
use serde_json::json;

fn main() {
    let manifest = Manifest::new("demo", "Demo counter", "0.1.0")
        .description("Example plugin: counts key presses and reads a lamp over host-mediated HTTP")
        .capability(Capability::StateWrite)
        .capability(Capability::Net("lights.lan".to_string()))
        .event(EventKind::KeyDown);

    let count = Arc::new(AtomicU64::new(0));

    let plugin = Plugin::new(manifest)
        .action(
            "bump",
            json!({"type":"object","properties":{"by":{"type":"integer","default":1}},"additionalProperties":false}),
            {
                let count = count.clone();
                move |_sdk, args| {
                    let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
                    let n = count.fetch_add(by.max(0) as u64, Ordering::SeqCst) + by.max(0) as u64;
                    Ok(vec![
                        set("/controls/keys.0/label", n.to_string()),
                        set(
                            "/controls/keys.0/image",
                            json!({"kind":"color","value":"#ff8800"}),
                        ),
                    ])
                }
            },
        )
        .action(
            "get_lamp",
            json!({"type":"object"}),
            |sdk, _args| {
                // Host-mediated network (S4): the daemon checks net:lights.lan and performs
                // the request itself; this process never opens a socket.
                let r = sdk.http("GET", "http://lights.lan/api/lamp")?;
                Ok(vec![set("/plugins/demo/state/last", r.body)])
            },
        )
        .on_event(|_sdk, ev| {
            Ok(vec![log(
                "info",
                format!("saw event {} on {:?}", ev.event, ev.control),
            )])
        });

    match plugin.serve_stdio() {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("demo-plugin: {e}");
            std::process::exit(e.exit_code());
        }
    }
}
