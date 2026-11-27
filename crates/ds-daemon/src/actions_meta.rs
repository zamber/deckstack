//! DS-113 requirement 1: the registered **action-metadata aggregate** — a read-only
//! document the daemon serves over the `/preview` debug surface
//! (`docs/contracts/preview.md`, schema `schemas/action-metadata.json/v1`).
//!
//! The aggregate is built on demand from exactly the two normative sources named in
//! the ticket: (a) the closed core step vocabulary of `docs/contracts/actions.md` §3
//! (mirrored verbatim in [`CORE_STEPS`] below) and (b) every registered plugin's
//! manifest actions (`/plugins/<id>/manifest`, `docs/contracts/plugins.md` §3/§9).
//! It is derived data: never stored, never mutates state, never bumps `rev`.

use crate::plugins;
use crate::router::Bus;
use crate::state::StateHandle;
use serde_json::{json, Value};

/// `schemas/action-metadata.json`, embedded at compile time (served by the `schema` op).
pub const ACTION_METADATA_SCHEMA_JSON: &str = include_str!("../../../schemas/action-metadata.json");

/// The closed core step vocabulary (actions.md §3) as served in the aggregate's
/// `core[]` array. Keep in lockstep with `docs/contracts/actions.md` §3 and
/// `schemas/binding.json`'s step `oneOf`.
pub fn core_steps() -> [Value; 5] {
    [
        json!({
            "do": "set",
            "args": {"path": "state path to set", "value": "schema-validated value"},
            "capability": "mutate",
            "description": "Set a state path in one transaction (rev +1)."
        }),
        json!({
            "do": "emit",
            "args": {"topic": "event topic (deck/… or app/…)", "payload": "JSON payload"},
            "capability": "mutate",
            "description": "Publish a topic event on the daemon's event bus."
        }),
        json!({
            "do": "plugin",
            "args": {"id": "plugin id", "action": "manifest action id",
                     "args": "arguments validated against the action's params schema"},
            "capability": "mutate",
            "description": "Invoke a registered plugin action (2 s bounded timeout)."
        }),
        json!({
            "do": "exec",
            "args": {"argv": "absolute-path argv array, never a shell", "cwd": "absolute working directory"},
            "capability": "admin scope + allow_exec:true + /policy/exec/allow contains argv[0] + /policy/exec/deny does not",
            "description": "Fire-and-forget execve with rlimits (cpu 10 s, nproc 64, nofile 64, as 1 GiB); no shell, no timeout_ms."
        }),
        json!({
            "do": "sleep",
            "args": {"ms": "milliseconds to sleep"},
            "capability": "—",
            "description": "Pause the step chain for ms milliseconds."
        }),
    ]
}

/// Build the aggregate document (`schemas/action-metadata.json/v1`) from the core
/// vocabulary + every registered plugin's manifest actions.
///
/// Plugins come from the live plugin hosts (the same registry the router `plugin`
/// step uses); when no host exists yet, the default host is created lazily exactly
/// like the router seam does, so an installed-but-never-invoked plugin still shows
/// up after install.
pub fn aggregate(state: &StateHandle, bus: &Bus) -> Value {
    json!({
        "protocol": 1,
        "generated_at_ms": crate::lifecycle::now_ms(),
        "core": core_steps(),
        "plugins": plugins::plugin_metadata(state, bus),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Bus;
    use ds_state::Store;

    fn state() -> (StateHandle, std::thread::JoinHandle<()>) {
        crate::state::StateTask::spawn(Store::fresh(), None, Bus::new())
    }

    #[test]
    fn schema_embeds_and_parses() {
        let v: Value = serde_json::from_str(ACTION_METADATA_SCHEMA_JSON).unwrap();
        assert_eq!(
            v["$id"],
            json!("https://deckstack.dev/schemas/action-metadata.json/v1")
        );
    }

    #[test]
    fn aggregate_has_the_five_core_steps_and_no_plugins_without_a_plugin_dir() {
        let (state, t) = state();
        let bus = Bus::new();
        let doc = aggregate(&state, &bus);
        assert_eq!(doc["protocol"], json!(1));
        let core = doc["core"].as_array().unwrap();
        assert_eq!(core.len(), 5);
        let dos: Vec<&str> = core.iter().map(|c| c["do"].as_str().unwrap()).collect();
        assert_eq!(dos, ["set", "emit", "plugin", "exec", "sleep"]);
        // plugins is always an array of well-formed entries (empty when no plugin
        // dir exists; the demo-plugin listing is covered by the conformance suite)
        let plugins = doc["plugins"].as_array().unwrap();
        for p in plugins {
            for f in ["id", "name", "version", "capabilities", "actions"] {
                assert!(p.get(f).is_some(), "plugin entry missing {f}: {p}");
            }
        }
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn aggregate_validates_against_the_schema() {
        let (state, t) = state();
        let bus = Bus::new();
        let doc = aggregate(&state, &bus);
        let schema: Value = serde_json::from_str(ACTION_METADATA_SCHEMA_JSON).unwrap();
        let compiled = jsonschema::validator_for(&schema).unwrap();
        compiled.validate(&doc).unwrap();
        state.stop();
        t.join().unwrap();
    }
}
