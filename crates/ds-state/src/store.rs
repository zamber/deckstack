//! The state store: one JSON document in memory, a global revision, atomic optimistic
//! transactions, subscriptions, atomic persistence, and the durable/telemetry split.
//!
//! Layout per `docs/contracts/state.md` and ADR-004:
//!
//! - Every durable mutation is a [`Transaction`] applied all-or-nothing; each committed
//!   transaction bumps `/meta/rev` by exactly 1 and is validated against
//!   `schemas/state.json` before it is swapped in.
//! - `if_rev` gives optimistic concurrency: a mismatch fails the whole transaction with
//!   `E_CONFLICT { current_rev }` and changes nothing.
//! - `/telemetry/*` is the non-durable plane: `set_telemetry` never bumps `rev`, is never
//!   persisted, and notifies subscribers with `telemetry: true` events that carry no `rev`.
//! - `/focus` is ephemeral: writable through transactions (bumps `rev`, fires events) but
//!   excluded from the persisted file.
//! - Persistence is a pretty-printed JSON file written atomically (temp file + fsync +
//!   rename + dir fsync). A corrupt file at load time is quarantined to
//!   `<name>.corrupt-<ts>` and the store starts empty (crash recovery, state.md §4).
//!
//! Locking: the document lives behind an `RwLock` that is never held across file I/O;
//! writers are serialized by a separate commit mutex (the daemon has a single state task).

use crate::document;
use crate::error::{Result, StateError};
use crate::path::Path;
use crate::schema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// The canonical empty state document (scope invariant: empty state at start, ADR-013).
/// Mirrored by `tests/state/empty-state.json`.
pub fn empty_document() -> Value {
    json!({
        "meta": {"version": 1, "rev": 0, "protocol": 1},
        "devices": {},
        "controls": {},
        "bindings": [],
        "plugins": {},
        "streams": {},
        "settings": {},
        "policy": {},
        "telemetry": {}
    })
}

/// One operation inside a [`Transaction`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Op {
    /// Create or replace the value at `path`.
    Set { path: String, value: Value },
    /// Delete the value at `path` (`E_PATH` when it does not exist).
    Delete { path: String },
}

impl Op {
    /// Convenience constructor for a set op.
    pub fn set(path: impl Into<String>, value: Value) -> Self {
        Self::Set {
            path: path.into(),
            value,
        }
    }

    /// Convenience constructor for a delete op.
    pub fn delete(path: impl Into<String>) -> Self {
        Self::Delete { path: path.into() }
    }

    /// The path the op applies to.
    pub fn path(&self) -> &str {
        match self {
            Self::Set { path, .. } | Self::Delete { path } => path,
        }
    }
}

/// A transaction: `{"ops": […], "if_rev": N}` applied atomically (state.md §4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transaction {
    /// The ops, applied in order.
    pub ops: Vec<Op>,
    /// Optimistic-concurrency guard; `None` is last-write-wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_rev: Option<u64>,
}

impl Transaction {
    /// A transaction from ops, last-write-wins.
    pub fn new(ops: Vec<Op>) -> Self {
        Self { ops, if_rev: None }
    }

    /// Require the current rev to equal `rev` (else `E_CONFLICT`).
    pub fn with_if_rev(mut self, rev: u64) -> Self {
        self.if_rev = Some(rev);
        self
    }
}

/// Reply to a committed transaction.
#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    /// The new global revision (`/meta/rev` after the commit).
    pub rev: u64,
}

/// Reply to a `get`: durable reads carry the current `rev`; `/telemetry/*` reads do not.
#[derive(Debug, Clone, Serialize)]
pub struct GetReply {
    /// `Some(rev)` for durable paths, `None` for `/telemetry/*` (S1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    /// The value at the path (subtree for container paths, wildcard expansion for `*`).
    pub value: Value,
}

/// A subscription event (state.md §5). Durable events carry `rev`; telemetry events carry
/// `"telemetry": true` and no `rev`.
#[derive(Debug, Clone, Serialize)]
pub struct Event {
    /// The path that changed.
    pub path: String,
    /// The new rev (durable events only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<u64>,
    /// The new value (`null` for deletes).
    pub value: Value,
    /// The previous value, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev: Option<Value>,
    /// True for `/telemetry/*` events, which carry no `rev`.
    #[serde(skip_serializing_if = "is_false", default)]
    pub telemetry: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Handle of a subscription returned by [`Store::subscribe`].
pub type SubscriptionId = u64;

/// Subscriber callback. Invoked after a commit (outside the state lock), once per matching
/// event; it must not block — drop-counting for slow consumers is the daemon's job
/// (state.md §5).
pub type WatchCallback = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Clone)]
struct Watcher {
    id: SubscriptionId,
    pattern: Path,
    cb: WatchCallback,
}

/// The state store. Library-only: no IPC, no sockets, no daemon loop.
pub struct Store {
    inner: RwLock<Value>,
    watchers: RwLock<Vec<Watcher>>,
    commit_lock: Mutex<()>,
    persist_lock: Mutex<()>,
    persist_path: Option<PathBuf>,
    corrupt_backup: Option<PathBuf>,
    load_error: Option<String>,
}

static NEXT_SUBSCRIPTION: AtomicU64 = AtomicU64::new(1);
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn rev_of(doc: &Value) -> u64 {
    doc.get("meta")
        .and_then(|m| m.get("rev"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn set_rev(doc: &mut Value, rev: u64) -> Result<()> {
    let meta = doc
        .get_mut("meta")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| StateError::Internal("document has no /meta object".to_string()))?;
    meta.insert("rev".to_string(), json!(rev));
    Ok(())
}

/// The persisted view of the document: `/focus` removed (ephemeral) and `/telemetry`
/// emptied (non-durable, S1) — everything else is durable.
fn durable_view(doc: &Value) -> Value {
    let mut d = doc.clone();
    if let Some(obj) = d.as_object_mut() {
        obj.remove("focus");
        obj.insert("telemetry".to_string(), json!({}));
    }
    d
}

/// Write `doc` (durable view) to `path` atomically: temp file in the same directory,
/// fsync, rename, fsync of the directory.
fn persist_to(path: &FsPath, doc: &Value) -> Result<()> {
    let pretty = serde_json::to_string_pretty(&durable_view(doc))
        .map_err(|e| StateError::Internal(format!("cannot serialize state document: {e}")))?;
    let dir = path.parent().ok_or_else(|| {
        StateError::Internal(format!("{} has no parent directory", path.display()))
    })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| StateError::Internal(format!("{} has no file name", path.display())))?;
    let tmp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    {
        let mut f = File::create(&tmp).map_err(|e| {
            StateError::Internal(format!(
                "cannot create temp state file {}: {e}",
                tmp.display()
            ))
        })?;
        f.write_all(pretty.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        StateError::Internal(format!(
            "cannot rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Rename a corrupt state file to `<name>.corrupt-<unix-ts>` (suffix `-<n>` on collision).
fn quarantine(path: &FsPath) -> Result<PathBuf> {
    let dir = path.parent().ok_or_else(|| {
        StateError::Internal(format!("{} has no parent directory", path.display()))
    })?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| StateError::Internal(format!("{} has no file name", path.display())))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut candidate = dir.join(format!("{name}.corrupt-{ts}"));
    let mut n = 0u32;
    while candidate.exists() {
        n += 1;
        candidate = dir.join(format!("{name}.corrupt-{ts}-{n}"));
    }
    fs::rename(path, &candidate)
        .map_err(|e| StateError::Internal(format!("cannot quarantine {}: {e}", path.display())))?;
    Ok(candidate)
}

impl Store {
    /// A fresh store with the empty document (rev 0), not persisted anywhere.
    pub fn fresh() -> Self {
        Self {
            inner: RwLock::new(empty_document()),
            watchers: RwLock::new(Vec::new()),
            commit_lock: Mutex::new(()),
            persist_lock: Mutex::new(()),
            persist_path: None,
            corrupt_backup: None,
            load_error: None,
        }
    }

    /// Open the store from the state file at `path`.
    ///
    /// Missing file → empty document. Unparsable or schema-invalid file → quarantined to
    /// `<name>.corrupt-<ts>` (see [`Store::corrupt_backup`], [`Store::load_error`]) and the
    /// store starts empty, per the crash-recovery rule (state.md §4). The loaded document
    /// is validated against `schemas/state.json`.
    pub fn open(path: &FsPath) -> Result<Self> {
        let mut store = Self::fresh();
        store.persist_path = Some(path.to_path_buf());
        if !path.exists() {
            return Ok(store);
        }
        let loaded = fs::read_to_string(path)
            .map_err(|e| StateError::Internal(format!("cannot read {}: {e}", path.display())))
            .and_then(|text| {
                let doc: Value = serde_json::from_str(&text)
                    .map_err(|e| StateError::Internal(format!("state file is not JSON: {e}")))?;
                schema::validate_document(&doc)?;
                Ok(doc)
            });
        match loaded {
            Ok(doc) => {
                *store.inner.write().unwrap() = doc;
                Ok(store)
            }
            Err(e) => {
                let backup = quarantine(path)?;
                let mut store = store;
                store.corrupt_backup = Some(backup);
                store.load_error = Some(e.to_string());
                Ok(store)
            }
        }
    }

    /// The path of the quarantined corrupt state file, if loading hit one.
    pub fn corrupt_backup(&self) -> Option<&FsPath> {
        self.corrupt_backup.as_deref()
    }

    /// Why loading fell back to the empty document, if it did (the daemon emits a loud
    /// `deck/system` event with this message).
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    /// The current global revision (`/meta/rev`).
    pub fn rev(&self) -> u64 {
        rev_of(&self.inner.read().unwrap())
    }

    /// A clone of the whole in-memory document (durable + telemetry + focus).
    pub fn document(&self) -> Value {
        self.inner.read().unwrap().clone()
    }

    /// The durable, persisted view of the document (focus removed, telemetry emptied).
    pub fn durable_document(&self) -> Value {
        durable_view(&self.inner.read().unwrap())
    }

    /// `get <path>`: exact / subtree / one-segment `*`. Durable paths reply with the
    /// current `rev`; `/telemetry/*` replies without `rev` (S1).
    pub fn get(&self, path_str: &str) -> Result<GetReply> {
        let path = Path::parse(path_str)?;
        path.validate_read()?;
        let doc = self.inner.read().unwrap();
        let value = document::get(&doc, &path)?
            .ok_or_else(|| StateError::Path(format!("path not found: {path_str}")))?;
        let rev = if path.is_telemetry() {
            None
        } else {
            Some(rev_of(&doc))
        };
        Ok(GetReply { rev, value })
    }

    /// Commit a transaction atomically (state.md §4). On any failure — `if_rev` mismatch,
    /// bad path, schema violation, persistence error — nothing changes: no rev bump, no
    /// events, no partial application.
    pub fn commit(&self, tx: &Transaction) -> Result<Commit> {
        if tx.ops.is_empty() {
            return Err(StateError::Path("transaction has no ops".to_string()));
        }
        let _guard = self.commit_lock.lock().unwrap();
        let (new_doc, new_rev, events) = {
            let doc = self.inner.read().unwrap();
            let cur_rev = rev_of(&doc);
            if let Some(if_rev) = tx.if_rev {
                if if_rev != cur_rev {
                    return Err(StateError::Conflict {
                        current_rev: cur_rev,
                    });
                }
            }
            let mut new_doc = doc.clone();
            let mut events: Vec<(Path, Option<Value>, Option<Value>)> = Vec::new();
            for op in &tx.ops {
                let path = Path::parse(op.path())?;
                path.validate_write()?;
                match op {
                    Op::Set { value, .. } => {
                        let prev = document::get(&new_doc, &path)?;
                        document::set(&mut new_doc, &path, value.clone())?;
                        events.push((path, Some(value.clone()), prev));
                    }
                    Op::Delete { .. } => {
                        let prev = document::get(&new_doc, &path)?;
                        document::delete(&mut new_doc, &path)?;
                        events.push((path, None, prev));
                    }
                }
            }
            let new_rev = cur_rev + 1;
            set_rev(&mut new_doc, new_rev)?;
            // Validate the candidate document before anything is swapped in: a violation
            // rolls the whole transaction back.
            schema::validate_document(&new_doc)?;
            (new_doc, new_rev, events)
        };
        // Persist before swapping in: a persistence failure aborts the commit entirely.
        self.persist_doc(&new_doc)?;
        let events: Vec<Event> = {
            let mut inner = self.inner.write().unwrap();
            *inner = new_doc;
            events
                .into_iter()
                .map(|(path, value, prev)| Event {
                    path: path.as_str().to_string(),
                    rev: Some(new_rev),
                    value: value.unwrap_or(Value::Null),
                    prev,
                    telemetry: false,
                })
                .collect()
        };
        self.notify(&events);
        Ok(Commit { rev: new_rev })
    }

    /// Write a telemetry sample under `/telemetry/...` (S1). Never bumps `rev`, never
    /// persisted; subscribers on matching `/telemetry/*` patterns get an event with
    /// `"telemetry": true` and no `rev`.
    pub fn set_telemetry(&self, path_str: &str, value: Value) -> Result<()> {
        let path = Path::parse(path_str)?;
        path.validate_telemetry_write()?;
        let event = {
            let mut inner = self.inner.write().unwrap();
            let mut new_doc = inner.clone();
            let prev = document::get(&new_doc, &path)?;
            document::set(&mut new_doc, &path, value.clone())?;
            schema::validate_document(&new_doc)?;
            *inner = new_doc;
            Event {
                path: path.as_str().to_string(),
                rev: None,
                value,
                prev,
                telemetry: true,
            }
        };
        self.notify(std::slice::from_ref(&event));
        Ok(())
    }

    /// Record daemon start metadata (`/meta/daemon_pid`, `/meta/started_at`). Internal
    /// bookkeeping: no rev bump, no events, persisted with the next save.
    pub fn record_daemon_start(&self, pid: u32, started_at: &str) -> Result<()> {
        let mut new_doc = self.inner.read().unwrap().clone();
        let meta = new_doc
            .as_object_mut()
            .and_then(|o| o.get_mut("meta"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| StateError::Internal("document has no /meta object".to_string()))?;
        meta.insert("daemon_pid".to_string(), json!(pid));
        meta.insert("started_at".to_string(), json!(started_at));
        schema::validate_document(&new_doc)?;
        self.persist_doc(&new_doc)?;
        *self.inner.write().unwrap() = new_doc;
        Ok(())
    }

    /// Subscribe to durable (and/or telemetry) changes under a path pattern. `*` matches
    /// exactly one segment; the pattern matches an event when it is a segment-wise prefix
    /// of the event path. Returns the subscription id; events are delivered to `callback`
    /// after the commit, outside the state lock.
    pub fn subscribe(&self, pattern: &str, callback: WatchCallback) -> Result<SubscriptionId> {
        let pattern = Path::parse(pattern)?;
        pattern.validate_read()?;
        let id = NEXT_SUBSCRIPTION.fetch_add(1, Ordering::Relaxed);
        self.watchers.write().unwrap().push(Watcher {
            id,
            pattern,
            cb: callback,
        });
        Ok(id)
    }

    /// Remove a subscription. Returns false when the id is unknown.
    pub fn unsubscribe(&self, id: SubscriptionId) -> bool {
        let mut w = self.watchers.write().unwrap();
        let before = w.len();
        w.retain(|x| x.id != id);
        w.len() != before
    }

    /// Persist the current document now (atomic temp+rename), when a path is configured.
    pub fn persist_now(&self) -> Result<()> {
        let doc = self.inner.read().unwrap().clone();
        self.persist_doc(&doc)
    }

    fn persist_doc(&self, doc: &Value) -> Result<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let _g = self.persist_lock.lock().unwrap();
        persist_to(path, doc)
    }

    fn notify(&self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        let watchers = self.watchers.read().unwrap().clone();
        for ev in events {
            let Ok(path) = Path::parse(&ev.path) else {
                continue;
            };
            for w in &watchers {
                if w.pattern.matches(&path) {
                    (w.cb)(ev.clone());
                }
            }
        }
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::fresh()
    }
}
