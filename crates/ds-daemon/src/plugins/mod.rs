//! The plugin host (ticket DS-050): one sandboxed OS process per plugin, the
//! contractual wire protocol, capability enforcement, resource limits, watchdog and
//! lifecycle state. Contract: `docs/contracts/plugins.md`; component:
//! `docs/components/plugin-host.md`; threats T3/T11 in `docs/20-security.md`.
//!
//! Layout:
//!
//! - [`manifest`] — the manifest (`schemas/plugin-manifest.json`) loading/validation
//!   and the capability helpers (anchored `net:<glob>` matching, `fs:<root>` prefixes);
//! - [`wire`] — the newline-JSON wire protocol messages and pure helpers;
//! - [`spawn`] — the re-exec'd plugin child (rlimits + Landlock + process group +
//!   argv-only `execve`, reaper, kill-on-exit registry);
//! - [`host`] — the [`host::Host`]: install/start/stop/remove, the per-plugin
//!   supervisor (hello/register handshake with a deadline, invoke with a 2 s bounded
//!   timeout, effects/requests with per-item capability checks, watchdog respawn
//!   ≤ 3 per 10 min then disabled), the per-plugin log ring buffer, host-mediated
//!   network (`request kind:"net"`), lifecycle state under `/plugins/<id>` and the
//!   `plugin/<id>/crash|disabled` bus events.
//!
//! Router seam: the DS-042 `plugin` step calls [`invoke_with`], which finds the
//! registered host serving the plugin (lazily creating the default host at
//! `~/.local/share/deckstack/plugins/` on first use, so the seam is live in the real
//! daemon) and invokes `id.action` with a bounded timeout. Error mapping (documented
//! in `docs/contracts/plugins.md` §9): unknown plugin/action → `E_PATH`; plugin not
//! running or crashed mid-call → `E_DEVICE`; capability violation → `E_CAP`;
//! args/manifest/wire shape → `E_SCHEMA`; invoke timeout (2 s) → `E_LIMIT`.
//!
//! Process model (S4/T3): plugins never open sockets and never touch state directly —
//! the daemon mediates `net` (glob + `/policy/net/allow_hosts`) and applies `effects`
//! after validating them against the state schema subset a plugin may write
//! (`/controls/…` and its own `/plugins/<id>/…`).

pub mod host;
pub mod manifest;
pub mod spawn;
pub mod wire;

#[cfg(test)]
mod tests;

pub use host::{Host, Status};

use crate::router::Bus;
use crate::state::StateHandle;
use ds_ipc::error::{ErrorCode, IpcError};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

/// The default plugin base dir: `$XDG_DATA_HOME/deckstack/plugins/`, else
/// `~/.local/share/deckstack/plugins/` (contract §1).
pub fn default_plugin_dir() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(xdg).join("deckstack/plugins");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".local/share/deckstack/plugins")
}

// ---- global host registry (the router seam) --------------------------------

fn registry() -> &'static Mutex<Vec<std::sync::Weak<Host>>> {
    static REG: OnceLock<Mutex<Vec<std::sync::Weak<Host>>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a host so the router `plugin` step (and `shutdown_all`) can find it.
pub fn register(host: &Arc<Host>) {
    let mut reg = registry().lock().unwrap();
    reg.retain(|w| w.strong_count() > 0);
    reg.push(Arc::downgrade(host));
}

/// All live registered hosts.
pub fn hosts() -> Vec<Arc<Host>> {
    let reg = registry().lock().unwrap();
    reg.iter().filter_map(|w| w.upgrade()).collect()
}

fn default_host_slot() -> &'static Mutex<Option<Arc<Host>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<Host>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// The lazily-created default host (production wiring): created on the first router
/// `plugin` step that no registered host serves, bound to that engine's state/bus,
/// scanning `default_plugin_dir()`. `None` when there is no plugin dir at all.
fn default_host(state: &StateHandle, bus: &Bus) -> Option<Arc<Host>> {
    {
        let slot = default_host_slot().lock().unwrap();
        if let Some(h) = slot.as_ref() {
            return Some(h.clone());
        }
    }
    let dir = default_plugin_dir();
    if !dir.is_dir() {
        return None;
    }
    let host = Host::new(state.clone(), bus.clone(), dir);
    host.load_all();
    register(&host);
    let mut slot = default_host_slot().lock().unwrap();
    *slot = Some(host.clone());
    Some(host)
}

/// The router seam: invoke `plugin_id.action` with `args` on the host that serves the
/// plugin (registered hosts first, then the lazily-created default host). Bounded by
/// [`host::INVOKE_TIMEOUT`] inside the host — the router thread never blocks
/// unboundedly. When no host serves the plugin, the typed `E_CAP` stub is returned
/// (same shape the DS-042 stub produced, so existing behaviour is preserved when the
/// plugin host is not running).
pub fn invoke_with(
    state: &StateHandle,
    bus: &Bus,
    plugin_id: &str,
    action: &str,
    args: &Value,
) -> Result<Value, IpcError> {
    for h in hosts() {
        if h.has_plugin(plugin_id) {
            return h.invoke(plugin_id, action, args);
        }
    }
    if let Some(h) = default_host(state, bus) {
        if h.has_plugin(plugin_id) {
            return h.invoke(plugin_id, action, args);
        }
    }
    Err(IpcError::new(
        ErrorCode::ECap,
        format!(
            "plugin step '{plugin_id}.{action}': no plugin host serves plugin '{plugin_id}' \
             (the DS-050 plugin host is not running or the plugin is not installed under {})",
            default_plugin_dir().display()
        ),
    ))
}

/// Kill every plugin process group of every registered host (daemon exit, called from
/// `lib.rs`). Belt and braces: the pid registry is swept too.
pub fn shutdown_all() {
    for h in hosts() {
        h.stop_all();
    }
    spawn::kill_all_plugin_groups();
}

/// DS-113: the plugin half of the action-metadata aggregate
/// (`schemas/action-metadata.json/v1`, served over `/preview`). One entry per
/// registered plugin, built from its manifest (`/plugins/<id>/manifest`): id, name,
/// version, capabilities and the manifest's `actions[]` verbatim (id/name/description/
/// params schema), each annotated with the capability an invoke requires.
///
/// Hosts are the live registered ones; when none exists yet, the default host is
/// created lazily exactly like the router `plugin` step seam does, so a freshly
/// installed plugin is listed without a prior invoke.
pub fn plugin_metadata(state: &StateHandle, bus: &Bus) -> Vec<serde_json::Value> {
    use serde_json::json;
    let mut live = hosts();
    if live.is_empty() {
        if let Some(h) = default_host(state, bus) {
            live.push(h);
        }
    }
    let mut out = Vec::new();
    for h in live {
        let list = h.list();
        let Some(map) = list.as_object() else {
            continue;
        };
        for (id, v) in map {
            let Some(m) = v.get("manifest") else { continue };
            let actions: Vec<serde_json::Value> = m
                .get("actions")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .map(|a| {
                            let mut a = a.clone();
                            if let Some(obj) = a.as_object_mut() {
                                obj.insert(
                                    "capability".to_string(),
                                    json!("mutate scope (plugins.md §9) plus the plugin-level capabilities"),
                                );
                            }
                            a
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push(json!({
                "id": m.get("id").cloned().unwrap_or_else(|| json!(id)),
                "name": m.get("name").cloned().unwrap_or_else(|| json!(id)),
                "version": m.get("version").cloned().unwrap_or(json!("0.0.0")),
                "capabilities": m.get("capabilities").cloned().unwrap_or(json!([])),
                "actions": actions,
            }));
        }
    }
    out
}
