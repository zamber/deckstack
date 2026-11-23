//! `ds-plugin-sdk` — the Rust SDK for deckstack plugins (`docs/contracts/plugins.md` §8).
//!
//! One OS process per plugin; the wire protocol is **newline-JSON over stdin/stdout**:
//!
//! ```text
//! host→plugin  {"op":"hello","protocol":1,"plugin_token":"…","caps":[…],"roots":{…},"limits":{…}}
//! plugin→host  {"op":"register","id":"demo","manifest":{…}}
//! host→plugin  {"op":"event","event":"keyDown","control":"keys.0","state":{…}}
//! host→plugin  {"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":7}
//! plugin→host  {"op":"effects","request_id":7,"effects":[{"effect":"set","path":"…","value":…}]}
//! plugin→host  {"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}
//! plugin→host  {"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/…"}
//! host→plugin  {"op":"reply","request_id":9,"ok":true,"status":200,"body":"…"}
//! ```
//!
//! Minimal plugin (mirrors `docs/contracts/plugins.md` §7):
//!
//! ```rust,no_run
//! use ds_plugin_sdk::{set, Capability, Manifest, Plugin};
//! use serde_json::json;
//!
//! let manifest = Manifest::new("demo", "Demo counter", "0.1.0")
//!     .capability(Capability::StateWrite)
//!     .capability(Capability::Net("lights.lan".to_string()));
//! let plugin = Plugin::new(manifest).action(
//!     "bump",
//!     json!({"type":"object","properties":{"by":{"type":"integer","default":1}}}),
//!     |_sdk, args| {
//!         let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
//!         Ok(vec![
//!             set("/controls/keys.0/label", by.to_string()),
//!             set("/controls/keys.0/image", json!({"kind":"color","value":"#ff8800"})),
//!         ])
//!     },
//! );
//! // plugin.serve_stdio()?;   // or ds_plugin_sdk::serve(manifest) with no actions
//! ```
//!
//! - [`serve`] / [`Plugin::serve_stdio`] / [`Plugin::serve_io`] run the protocol loop.
//! - [`Plugin::action`] / [`Plugin::action_typed`] register actions (params JSON Schema
//!   validated before dispatch); [`Plugin::on_event`] registers the event handler.
//! - [`set`] / [`emit`] / [`log`] / [`sleep`] build effects; [`Sdk::get_state`] / [`Sdk::set_state`] /
//!   [`Sdk::http`] are the host-mediated requests callable from handlers.
//! - Errors map to the stable codes of `schemas/ipc-message.json` ([`ErrorCode`]).
//!
//! See the crate README for the capabilities model, the error mapping, and the exact wire
//! shapes the contract tests pin byte-for-byte.

pub mod effect;
pub mod error;
pub mod http;
pub mod manifest;
pub mod params;
pub mod session;
pub mod wire;

pub use effect::{emit, log, set, sleep, Effect};
pub use error::{ErrorCode, PluginError, ServeError};
pub use http::HttpResponse;
pub use manifest::{ActionDef, Capability, EventKind, Manifest, Resources};
pub use session::{serve, HostEvent, Plugin, Sdk};
pub use wire::PROTOCOL_VERSION;
