//! `ds-state` — the deckstack state document: schema-validated load/save, path get/set,
//! global revision with atomic optimistic transactions, subscriptions, atomic persistence,
//! and the durable/telemetry split (`schemas/state.json`, `docs/contracts/state.md`,
//! ADR-004).
//!
//! Library-only by design: no IPC, no sockets, no HID, no daemon loop — `ds-daemon`
//! embeds [`Store`] in its state task.
//!
//! - Two planes (S1): durable document + non-durable `/telemetry/*`. Telemetry writes
//!   never bump `rev` and are never persisted; frame pushes never touch this crate.
//! - Every durable mutation is a [`Transaction`]; each commit bumps `/meta/rev` by 1,
//!   validates the whole document against `schemas/state.json`, and persists atomically
//!   (temp + fsync + rename).
//! - Empty state at start (ADR-013): [`empty_document`] has empty `/controls`,
//!   `/bindings`, `/plugins` and validates against the schema.
//!
//! ```rust
//! use ds_state::{Op, Store, Transaction};
//! use serde_json::json;
//! use std::sync::Arc;
//!
//! let store = Store::fresh();
//! let (tx, rx) = std::sync::mpsc::channel();
//! let id = store
//!     .subscribe("/controls/*", Arc::new(move |ev| { let _ = tx.send(ev); }))
//!     .unwrap();
//! let c = store
//!     .commit(&Transaction::new(vec![Op::set(
//!         "/controls/keys/0/image",
//!         json!({"kind": "color", "value": "#3cc86e"}),
//!     )]))
//!     .unwrap();
//! assert_eq!(c.rev, 1);
//! assert_eq!(store.get("/controls/keys/0/image").unwrap().rev, Some(1));
//! assert_eq!(rx.try_recv().unwrap().rev, Some(1));
//! store.unsubscribe(id);
//! ```

pub mod document;
pub mod error;
pub mod path;
pub mod schema;
pub mod store;

pub use error::{Result, StateError};
pub use path::Path;
pub use schema::validate_document;
pub use store::{empty_document, Commit, Event, GetReply, Op, Store, SubscriptionId, Transaction};

/// Crate name, for logs and `--version` output.
pub const CRATE_NAME: &str = "ds-state";

/// Crate version, read from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "ds-state");
        assert!(!version().is_empty());
    }

    #[test]
    fn empty_document_validates_and_is_empty() {
        let doc = empty_document();
        validate_document(&doc).unwrap();
        assert_eq!(doc["controls"], serde_json::json!({}));
        assert_eq!(doc["bindings"], serde_json::json!([]));
        assert_eq!(doc["plugins"], serde_json::json!({}));
        assert_eq!(doc["meta"]["rev"], serde_json::json!(0));
    }
}
