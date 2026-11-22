// Integration tests for ds-state (ticket DS-011 acceptance):
//! empty-start invariant, rev semantics, transaction atomicity/rollback, schema rejection,
//! the durable/telemetry split, subscriptions, atomic persistence and crash recovery.

use ds_state::{empty_document, validate_document, Event, Op, Store, SubscriptionId, Transaction};
use serde_json::{json, Value};
use std::fs;
use std::sync::{mpsc, Arc};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Subscribe with a channel-backed callback; returns (id, receiver).
fn watch(store: &Store, pattern: &str) -> (SubscriptionId, mpsc::Receiver<Event>) {
    let (tx, rx) = mpsc::channel();
    let id = store
        .subscribe(
            pattern,
            Arc::new(move |ev| {
                let _ = tx.send(ev);
            }),
        )
        .unwrap();
    (id, rx)
}

fn binding(id: &str, topic: &str) -> Value {
    json!({
        "id": id,
        "topic": topic,
        "allow_exec": false,
        "steps": [{"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}}]
    })
}

#[test]
fn fresh_store_is_empty_valid_and_matches_fixture() {
    let store = Store::fresh();
    let doc = store.document();
    validate_document(&doc).unwrap();
    assert_eq!(doc["controls"], json!({}));
    assert_eq!(doc["bindings"], json!([]));
    assert_eq!(doc["plugins"], json!({}));
    assert_eq!(store.rev(), 0);
    // The canonical fixture tests/state/empty-state.json is exactly the fresh document.
    let fixture: Value =
        serde_json::from_str(include_str!("../../../tests/state/empty-state.json")).unwrap();
    assert_eq!(doc, fixture);
    assert_eq!(empty_document(), fixture);
}

#[test]
fn set_bumps_rev_and_notifies_subscribers() {
    let store = Store::fresh();
    let (id, rx) = watch(&store, "/controls/*");
    let commit = store
        .commit(&Transaction::new(vec![Op::set(
            "/controls/keys/0/image",
            json!({"kind": "color", "value": "#3cc86e"}),
        )]))
        .unwrap();
    assert_eq!(commit.rev, 1);
    assert_eq!(store.rev(), 1);

    let got = store.get("/controls/keys/0/image").unwrap();
    assert_eq!(got.rev, Some(1));
    assert_eq!(got.value, json!({"kind": "color", "value": "#3cc86e"}));

    let ev = rx.try_recv().unwrap();
    assert_eq!(ev.rev, Some(1));
    assert!(!ev.telemetry);
    assert_eq!(ev.path, "/controls/keys/0/image");
    assert_eq!(ev.value, json!({"kind": "color", "value": "#3cc86e"}));
    assert_eq!(ev.prev, None);
    assert!(rx.try_recv().is_err(), "exactly one event per committed op");
    store.unsubscribe(id);
    assert!(!store.unsubscribe(id));
}

#[test]
fn subtree_and_wildcard_gets() {
    let store = Store::fresh();
    store
        .commit(&Transaction::new(vec![
            Op::set("/controls/keys/0/label", json!("Salon")),
            Op::set("/controls/keys/3/label", json!("Kitchen")),
            Op::set("/controls/dial/brightness", json!(80)),
        ]))
        .unwrap();
    let keys = store.get("/controls/keys/*").unwrap();
    assert_eq!(
        keys.value,
        json!({"0": {"label": "Salon"}, "3": {"label": "Kitchen"}})
    );
    assert_eq!(keys.rev, Some(1));
    let dial = store.get("/controls/dial").unwrap();
    assert_eq!(dial.value, json!({"brightness": 80}));
    let all = store.get("/").unwrap();
    assert_eq!(all.value["controls"]["keys"]["0"]["label"], json!("Salon"));
    assert!(store.get("/controls/keys/1").is_err());
}

#[test]
fn if_rev_conflict_changes_nothing() {
    let store = Store::fresh();
    store
        .commit(&Transaction::new(vec![Op::set(
            "/settings/log/level",
            json!("info"),
        )]))
        .unwrap();
    assert_eq!(store.rev(), 1);

    let (_, rx) = watch(&store, "/settings/*");
    let err = store
        .commit(
            &Transaction::new(vec![Op::set("/settings/log/level", json!("debug"))]).with_if_rev(0),
        )
        .unwrap_err();
    assert_eq!(err.code(), "E_CONFLICT");
    assert!(format!("{err}").contains("current_rev is 1"));
    assert_eq!(store.rev(), 1, "conflict must not bump rev");
    assert_eq!(
        store.get("/settings/log/level").unwrap().value,
        json!("info")
    );
    assert!(rx.try_recv().is_err(), "conflict must not notify");

    // Correct if_rev succeeds.
    let c = store
        .commit(
            &Transaction::new(vec![Op::set("/settings/log/level", json!("debug"))]).with_if_rev(1),
        )
        .unwrap();
    assert_eq!(c.rev, 2);
    // Last-write-wins when if_rev is omitted.
    let c = store
        .commit(&Transaction::new(vec![Op::set(
            "/settings/log/level",
            json!("warn"),
        )]))
        .unwrap();
    assert_eq!(c.rev, 3);
}

#[test]
fn transaction_is_atomic_and_rolls_back_on_failure() {
    let store = Store::fresh();
    let (_, rx) = watch(&store, "/");

    // Second op has an out-of-vocabulary path: the whole transaction fails, the first op
    // is rolled back, rev and subscribers are untouched.
    let err = store
        .commit(&Transaction::new(vec![
            Op::set("/controls/keys/0/label", json!("Salon")),
            Op::set("/controls/keys/99/label", json!("bad")),
        ]))
        .unwrap_err();
    assert_eq!(err.code(), "E_PATH");
    assert_eq!(store.rev(), 0);
    assert!(store.get("/controls/keys/0").is_err());
    assert!(rx.try_recv().is_err());

    // Schema-violating value: E_SCHEMA rollback.
    let err = store
        .commit(&Transaction::new(vec![Op::set(
            "/controls/keys/0/image",
            json!({"kind": "color", "value": "red"}),
        )]))
        .unwrap_err();
    assert_eq!(err.code(), "E_SCHEMA");
    assert_eq!(store.rev(), 0);
    assert!(store.get("/controls/keys/0").is_err());

    // Deleting a missing path fails the transaction too.
    let err = store
        .commit(&Transaction::new(vec![
            Op::set("/settings/a", json!(1)),
            Op::delete("/settings/missing"),
        ]))
        .unwrap_err();
    assert_eq!(err.code(), "E_PATH");
    assert_eq!(store.rev(), 0);
    assert!(store.get("/settings/a").is_err());

    // A good multi-op transaction commits as one rev with one event per op.
    let c = store
        .commit(&Transaction::new(vec![
            Op::set("/controls/keys/0/label", json!("Salon")),
            Op::set("/controls/keys/0/brightness", json!(80)),
        ]))
        .unwrap();
    assert_eq!(c.rev, 1);
    assert_eq!(
        store.get("/controls/keys/0").unwrap().value,
        json!({"label": "Salon", "brightness": 80})
    );
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err());
}

#[test]
fn delete_op_bumps_rev() {
    let store = Store::fresh();
    store
        .commit(&Transaction::new(vec![Op::set(
            "/controls/dial/label",
            json!("x"),
        )]))
        .unwrap();
    let c = store
        .commit(&Transaction::new(vec![Op::delete("/controls/dial/label")]))
        .unwrap();
    assert_eq!(c.rev, 2);
    assert!(store.get("/controls/dial/label").is_err());
}

#[test]
fn path_vocabulary_is_enforced_on_writes() {
    let store = Store::fresh();
    for bad in [
        "/controls/keys/15/image",
        "/controls/keys/99",
        "/controls/knobs/3/led",
        "/controls/faders/5/value",
        "/controls/rollers/2/value",
        "/controls/screens/0",
        "/streams/panel",
        "/streams/key:15",
        "/bindings/x1",
        "/plugins/../etc",
        "/devices/bad serial!",
        "/meta/rev",
        "/nope/deep",
        "/telemetry/streams/keys",
        "/controls/*",
    ] {
        let err = store
            .commit(&Transaction::new(vec![Op::set(bad, json!({}))]))
            .unwrap_err();
        assert_eq!(err.code(), "E_PATH", "expected E_PATH for {bad}, got {err}");
    }
    assert_eq!(store.rev(), 0);
    // Reads may use telemetry and wildcards but still need a known top level.
    assert!(store.get("/nope").is_err());
    assert!(store.get("/controls/*").is_ok());
}

#[test]
fn bindings_array_addressed_by_id() {
    let store = Store::fresh();
    let c = store
        .commit(&Transaction::new(vec![Op::set(
            "/bindings",
            json!([
                binding("b0001", "deck/control/keys.0/down"),
                binding("b0002", "app/focus/changed")
            ]),
        )]))
        .unwrap();
    assert_eq!(c.rev, 1);
    let one = store.get("/bindings/b0001").unwrap();
    assert_eq!(one.value, binding("b0001", "deck/control/keys.0/down"));
    let all = store.get("/bindings").unwrap();
    assert_eq!(all.value.as_array().unwrap().len(), 2);
    let stars = store.get("/bindings/*").unwrap();
    assert_eq!(stars.value.as_array().unwrap().len(), 2);

    let c = store
        .commit(&Transaction::new(vec![Op::delete("/bindings/b0001")]))
        .unwrap();
    assert_eq!(c.rev, 2);
    assert!(store.get("/bindings/b0001").is_err());
    assert_eq!(
        store
            .get("/bindings")
            .unwrap()
            .value
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // A schema-invalid binding is rejected (external $ref to schemas/binding.json).
    let err = store
        .commit(&Transaction::new(vec![Op::set(
            "/bindings",
            json!([{"id": "x1", "topic": "deck/x", "steps": [{"do": "sleep", "ms": 5}]}]),
        )]))
        .unwrap_err();
    assert_eq!(err.code(), "E_SCHEMA");
    assert_eq!(store.rev(), 2);
}

#[test]
fn telemetry_never_bumps_rev_never_persists() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state.json");
    let store = Store::open(&file).unwrap();
    assert_eq!(store.rev(), 0);

    let (durable_id, durable_rx) = watch(&store, "/");
    let (tel_id, tel_rx) = watch(&store, "/telemetry/*");

    store
        .set_telemetry(
            "/telemetry/streams/keys",
            json!({"sent": 960, "dropped": 12, "fps": 32.0, "in_flight": true}),
        )
        .unwrap();
    assert_eq!(store.rev(), 0, "telemetry must not bump rev");

    // get /telemetry/... returns the sample with no rev.
    let got = store.get("/telemetry/streams/keys").unwrap();
    assert_eq!(got.rev, None);
    assert_eq!(got.value["sent"], json!(960));

    // Telemetry event: telemetry:true, no rev. Durable watcher on "/" also matches — but
    // the contract says telemetry events go to /telemetry subscribers; both patterns are
    // prefixes here, so the "/" watcher sees it too, tagged telemetry with no rev.
    let ev = tel_rx.try_recv().unwrap();
    assert!(ev.telemetry);
    assert_eq!(ev.rev, None);
    assert_eq!(ev.path, "/telemetry/streams/keys");
    let ev2 = durable_rx.try_recv().unwrap();
    assert!(ev2.telemetry);
    assert_eq!(ev2.rev, None);

    // A durable commit persists, and the file contains no telemetry values.
    store
        .commit(&Transaction::new(vec![Op::set(
            "/settings/log/level",
            json!("info"),
        )]))
        .unwrap();
    assert_eq!(store.rev(), 1);
    let on_disk: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    validate_document(&on_disk).unwrap();
    assert_eq!(
        on_disk["telemetry"],
        json!({}),
        "telemetry is never persisted"
    );
    assert_eq!(on_disk["meta"]["rev"], json!(1));
    let dev = durable_rx.try_recv().unwrap();
    assert!(!dev.telemetry);
    assert_eq!(dev.rev, Some(1));

    // Reload: telemetry plane starts empty, durable state and rev survive.
    let reopened = Store::open(&file).unwrap();
    assert_eq!(reopened.rev(), 1);
    assert_eq!(
        reopened.get("/settings/log/level").unwrap().value,
        json!("info")
    );
    assert!(reopened.get("/telemetry/streams/keys").is_err());
    assert_eq!(reopened.document()["telemetry"], json!({}));

    store.unsubscribe(durable_id);
    store.unsubscribe(tel_id);
}

#[test]
fn telemetry_write_vocabulary_and_schema() {
    let store = Store::fresh();
    assert!(store.set_telemetry("/controls/keys/0", json!({})).is_err());
    assert!(store.set_telemetry("/telemetry/other", json!({})).is_err());
    assert!(store
        .set_telemetry("/telemetry/streams/key:15", json!({}))
        .is_err());
    // Schema violation inside telemetry rolls back (e.g. bad sha256).
    let err = store
        .set_telemetry("/telemetry/streams/keys", json!({"last_sha256": "xyz"}))
        .unwrap_err();
    assert_eq!(err.code(), "E_SCHEMA");
    assert!(store.get("/telemetry/streams/keys").is_err());
    // Valid sample works, and partial updates merge into the existing object.
    store
        .set_telemetry("/telemetry/streams/keys", json!({"sent": 1}))
        .unwrap();
    store
        .set_telemetry("/telemetry/streams/keys/dropped", json!(2))
        .unwrap();
    let got = store.get("/telemetry/streams/keys").unwrap();
    assert_eq!(got.value, json!({"sent": 1, "dropped": 2}));
    assert_eq!(store.rev(), 0);
}

#[test]
fn focus_is_ephemeral_not_persisted_but_rev_bumping() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state.json");
    let store = Store::open(&file).unwrap();
    let (_, rx) = watch(&store, "/focus");
    let c = store
        .commit(&Transaction::new(vec![Op::set(
            "/focus",
            json!({"app": "code", "pid": 123, "at_rev": 1}),
        )]))
        .unwrap();
    assert_eq!(c.rev, 1, "focus writes are transactions and bump rev");
    let ev = rx.try_recv().unwrap();
    assert_eq!(ev.rev, Some(1));
    assert!(!ev.telemetry);

    let on_disk: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    assert!(
        on_disk.get("focus").is_none(),
        "/focus is ephemeral, never persisted"
    );
    validate_document(&on_disk).unwrap();

    let reopened = Store::open(&file).unwrap();
    assert_eq!(reopened.rev(), 1);
    assert!(
        reopened.get("/focus").is_err(),
        "focus does not survive a reload"
    );
}

#[test]
fn persistence_roundtrip_atomic_and_clean() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state.json");
    {
        let store = Store::open(&file).unwrap();
        assert!(!file.exists(), "no file until the first commit");
        store
            .commit(&Transaction::new(vec![Op::set(
                "/controls/keys/0/image",
                json!({"kind": "icon", "name": "tabler:home"}),
            )]))
            .unwrap();
        store
            .commit(
                &Transaction::new(vec![Op::set("/streams/keys", json!({"queue_depth": 1}))])
                    .with_if_rev(1),
            )
            .unwrap();
        store
            .record_daemon_start(4242, "2026-11-22T10:00:00Z")
            .unwrap();
        assert_eq!(store.rev(), 2);
    }
    // Only the state file remains: no temp files left behind by the atomic write.
    let entries: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        entries,
        vec!["state.json".to_string()],
        "atomic write leaves no temp files: {entries:?}"
    );

    let on_disk: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    validate_document(&on_disk).unwrap();
    assert_eq!(on_disk["meta"]["rev"], json!(2));
    assert_eq!(on_disk["meta"]["daemon_pid"], json!(4242));
    assert_eq!(
        on_disk["controls"]["keys"]["0"]["image"]["name"],
        json!("tabler:home")
    );

    let reopened = Store::open(&file).unwrap();
    assert_eq!(reopened.rev(), 2);
    assert_eq!(
        reopened.get("/controls/keys/0/image").unwrap().value,
        json!({"kind": "icon", "name": "tabler:home"})
    );
    // Chained if_rev continues across a reload.
    let c = reopened
        .commit(&Transaction::new(vec![Op::set("/settings/x", json!(1))]).with_if_rev(2))
        .unwrap();
    assert_eq!(c.rev, 3);
}

#[test]
fn corrupt_file_is_quarantined_and_store_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state.json");
    fs::write(&file, "{ not json !!!").unwrap();
    let store = Store::open(&file).unwrap();
    assert_eq!(store.rev(), 0);
    validate_document(&store.document()).unwrap();
    assert_eq!(store.document()["controls"], json!({}));
    assert!(store.load_error().is_some());
    let backup = store.corrupt_backup().expect("corrupt file quarantined");
    assert!(backup.to_string_lossy().contains("state.json.corrupt-"));
    assert!(backup.exists());
    assert!(!file.exists(), "corrupt file moved away");
    // The next commit writes a fresh valid file.
    store
        .commit(&Transaction::new(vec![Op::set("/settings/a", json!(1))]))
        .unwrap();
    let on_disk: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
    validate_document(&on_disk).unwrap();
    assert_eq!(on_disk["meta"]["rev"], json!(1));
}

#[test]
fn schema_invalid_file_is_quarantined() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state.json");
    // Valid JSON but violates the schema (missing required /policy).
    fs::write(
        &file,
        serde_json::to_string_pretty(&json!({
            "meta": {"version": 1, "rev": 3, "protocol": 1},
            "devices": {}, "controls": {}, "bindings": [], "plugins": {},
            "streams": {}, "settings": {}, "telemetry": {}
        }))
        .unwrap(),
    )
    .unwrap();
    let store = Store::open(&file).unwrap();
    assert_eq!(store.rev(), 0);
    assert!(store.corrupt_backup().is_some());
    assert!(store.load_error().unwrap().starts_with("E_SCHEMA"));
}

#[test]
fn frame_descriptor_changes_only_via_set() {
    // S1: an image:{kind:"frame"} descriptor is an ordinary durable value; setting it is
    // a normal transaction (rev bump). The frame plane itself never calls into ds-state.
    let store = Store::fresh();
    let c = store
        .commit(&Transaction::new(vec![Op::set(
            "/controls/dial/image",
            json!({"kind": "frame", "target": "dial", "seq": 7, "sha256": SHA}),
        )]))
        .unwrap();
    assert_eq!(c.rev, 1);
    let got = store.get("/controls/dial/image").unwrap();
    assert_eq!(got.value["seq"], json!(7));
    assert_eq!(got.rev, Some(1));
}

#[test]
fn store_open_missing_file_starts_empty_without_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("nested").join("state.json"); // parent missing too
    let store = Store::open(&file).unwrap();
    assert_eq!(store.rev(), 0);
    assert!(store.corrupt_backup().is_none());
    assert!(store.load_error().is_none());
    // First commit creates parent dirs? No — persist_to requires the dir to exist; the
    // daemon creates the data dir. Use an existing dir here.
    let file2 = dir.path().join("state.json");
    let store2 = Store::open(&file2).unwrap();
    store2
        .commit(&Transaction::new(vec![Op::set("/settings/a", json!(1))]))
        .unwrap();
    assert!(file2.exists());
}
