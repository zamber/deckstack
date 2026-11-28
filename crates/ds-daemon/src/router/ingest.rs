//! Transactional event ingest (ticket DS-044; contract S7 in `docs/contracts/actions.md`
//! §2 and `docs/components/event-router.md` "Atomic event ingest").
//!
//! ## What this module does
//!
//! Some `app/<sender>/<topic>` events are **mirrored into state**: today exactly one,
//! `app/focus/changed` → [`FOCUS_PATH`] (`/focus`, `schemas/state.json`
//! `{app, title, pid, at_rev}`, ephemeral — never persisted). On such an event the
//! router must mirror the payload into `/focus` **in the same state transaction that
//! dispatches the matching bindings** (S7), so a `when {path: "/focus/app", …}` sees the
//! value that triggered it — never a stale one. Implementing ingest as "set `/focus`,
//! then dispatch" (two transactions) is exactly the bug S7 fixes and is not done here.
//!
//! ## Semantics (pinned here; the contract is unambiguous on atomicity, these are the
//! details chosen to conform to it)
//!
//! - **One transaction, one rev.** The ingest builds ONE [`Transaction`]: the mirror op
//!   `set /focus {app, title, pid, at_rev: <committing rev>}` first, then the `set` ops of
//!   every matching binding (exact topic patterns first, then `*` wildcards, array order
//!   within each group — identical to the DS-042 engine), and commits it with `if_rev`
//!   (retry on `E_CONFLICT`). One commit = exactly one rev bump; `at_rev` is the
//!   committing rev, and every changed path (mirror + binding sets) carries that rev in
//!   its subscription event — watchers see one event batch per rev.
//! - **`when` evaluation sees the new mirror.** `when` conditions with a `path` under
//!   `/focus` are evaluated against the *candidate* mirror value (overlay), all other
//!   paths against the current committed state; `payload` conditions against the event
//!   payload. So both the `path`-based and the `payload`-based variant fire in the same
//!   dispatch (actions.md §6 chain B).
//! - **Schema-validated before touching state.** The event payload must be
//!   `{"app": string, "title": string, "pid"?: integer}` (the producer contract,
//!   focus-sender.md; `pid` is optional since DS-114 — absent or null is accepted,
//!   a non-integer `pid` is rejected). Anything else — non-object, missing/mistyped
//!   field, unknown field — is rejected with `E_SCHEMA` **before any state change**: rev unchanged,
//!   `/focus` untouched, and a clear error event is published on
//!   `deck/system/binding-error` (`{"stage":"ingest","topic",…,"error":{code,message}}`)
//!   plus a `warn` log line.
//! - **Failure atomicity of step errors (ticket DS-044 acceptance):** a binding `set`
//!   step that would make the document schema-invalid (or has a bad path) is dropped
//!   from the ingest transaction with a `binding-error` event — the **mirror still
//!   commits** (event ingest is not rolled back by step errors). Each op is pre-validated
//!   locally against `schemas/state.json` (snapshot + apply + `validate_document`), so
//!   the committed transaction itself cannot fail with `E_SCHEMA`; `if_rev` makes the
//!   commit atomic against concurrent writers.
//! - **No reentrancy inside the transaction.** Steps that do not mutate state in the
//!   transaction — `emit`, `sleep`, and failing `plugin`/`exec`/unknown steps — are
//!   collected and run **after** the commit: `emit` steps are published on the bus and
//!   dispatched at `depth + 1` (mirrored topics re-enter the ingest, capped at
//!   [`MAX_EMIT_DEPTH`]); `sleep` moves the binding's remaining steps to a timer thread;
//!   step failures publish `deck/system/binding-error` after the commit. Steps after a
//!   `sleep` therefore commit as their own transaction(s) — the S7 atomic window covers
//!   the synchronous prefix of the dispatch.
//! - **Ordering.** The real-bus forwarder (subscribed on the catch-all topic pattern
//!   `*`, one drainer thread, FIFO) routes every topic event in arrival order: mirrored
//!   topics to the ingest, everything else to the DS-042 steps engine. Two quick events
//!   are ingested strictly one after the other (the ingest is synchronous), so per
//!   connection arrival order is preserved.
//!
//! ## Wiring (mod.rs `Engine`)
//!
//! The DS-042 steps engine gets a **private** bus: a single forwarder on the real bus
//! routes each topic event (mirrored → ingest, else → `steps::Engine::dispatch`), and a
//! relay on the private bus republishes the events the engine itself publishes there
//! (`emit` steps, `binding-error`) on the real bus so external subscribers see them,
//! marked so the forwarder skips them. Known boundary of this phase-1 wiring: an `emit`
//! *step* that targets a mirrored topic is dispatched by the steps engine without the
//! mirror (`router/steps.rs` is owned by DS-042); the contract producer path — the IPC
//! `emit` op / `ds-focus`, i.e. every `app/focus/changed` from outside — is fully covered.

use crate::lifecycle;
use crate::router::bus::{Bus, Event, FnSink};
use crate::router::steps::{
    Engine as StepsEngine, BINDING_ERROR_TOPIC, MAX_EMIT_DEPTH, MAX_SLEEP_MS,
};
use crate::router::when::eval_when;
use crate::router::{topics, Pattern, TopicPattern};
use crate::state::StateHandle;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_state::{document, validate_document, Op, Path, Transaction};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The topic whose payload is mirrored into [`FOCUS_PATH`].
pub const FOCUS_TOPIC: &str = "app/focus/changed";

/// The state path the [`FOCUS_TOPIC`] payload is mirrored into (`schemas/state.json`).
pub const FOCUS_PATH: &str = "/focus";

/// How often the ingest retries a commit that lost an `if_rev` race.
const MAX_COMMIT_ATTEMPTS: usize = 16;

/// The mirrored topic → state path map. Only focus today; any `app/<sender>/<topic>`
/// mapped here gets the same atomic ingest.
pub fn mirror_path(topic: &str) -> Option<&'static str> {
    match topic {
        FOCUS_TOPIC => Some(FOCUS_PATH),
        _ => None,
    }
}

/// Validate an `app/focus/changed` payload against the producer contract
/// (focus-sender.md): `{"app": string, "title": string, "pid"?: integer}` — `pid` is
/// optional (DS-114): absent or null is accepted and omitted from the mirror, a
/// present non-integer `pid` is rejected. Returns the clean mirror base (without
/// `at_rev`).
pub fn validate_mirror_payload(payload: &Value) -> Result<Value, IpcError> {
    let schema_err = |msg: String| IpcError::new(ErrorCode::ESchema, msg);
    let Some(obj) = payload.as_object() else {
        return Err(schema_err(format!(
            "{FOCUS_TOPIC} payload must be a JSON object {{app,title,pid?}}, got {payload}"
        )));
    };
    for key in obj.keys() {
        if !matches!(key.as_str(), "app" | "title" | "pid") {
            return Err(schema_err(format!(
                "{FOCUS_TOPIC} payload has unknown field {key:?} (the mirror is exactly {{app,title,pid?,at_rev}})"
            )));
        }
    }
    let app = obj.get("app").and_then(Value::as_str).ok_or_else(|| {
        schema_err(format!(
            "{FOCUS_TOPIC} payload needs a string 'app', got {payload}"
        ))
    })?;
    let title = obj.get("title").and_then(Value::as_str).ok_or_else(|| {
        schema_err(format!(
            "{FOCUS_TOPIC} payload needs a string 'title', got {payload}"
        ))
    })?;
    let mut base = json!({"app": app, "title": title});
    match obj.get("pid") {
        // DS-114: pid is optional — absent or null means the backend did not know it
        None | Some(Value::Null) => {}
        Some(v) => match v.as_i64() {
            Some(pid) => {
                base["pid"] = json!(pid);
            }
            None => {
                return Err(schema_err(format!(
                    "{FOCUS_TOPIC} payload 'pid' must be an integer when present, got {payload}"
                )));
            }
        },
    }
    Ok(base)
}

/// One action collected during the ingest, run after the transaction commits.
enum Deferred {
    /// An `emit` step: publish on the real bus + dispatch at `depth + 1`.
    Emit { topic: String, payload: Value },
    /// A failed step: `binding-error` event after the commit.
    Error {
        binding: String,
        step: usize,
        err: IpcError,
    },
    /// A `sleep` step: the binding's remaining steps run on a timer thread.
    Sleep {
        binding: Value,
        steps: Vec<Value>,
        start: usize,
        ms: u64,
        trigger: String,
    },
}

/// The transactional ingest + the bus wiring around the DS-042 steps engine.
///
/// Cloneable handle via `Arc`; created by [`crate::router::Engine::new`].
pub struct Ingest {
    /// The real event bus (producers, external subscribers, error events).
    bus: Bus,
    /// The private bus of the DS-042 steps engine.
    engine_bus: Bus,
    state: StateHandle,
    steps: Arc<StepsEngine>,
    /// Wire keys of real-bus events that were already dispatched (relayed from the
    /// engine bus, or published by a deferred emit): the forwarder must skip them.
    /// Key: `(topic, ts_ms, payload)` of the published wire message; value: pending count.
    dispatched: Mutex<HashMap<String, u32>>,
}

impl Ingest {
    /// Wire the ingest: `bus` is the real bus, `engine_bus` the private bus of `steps`.
    /// Subscribes the forwarder on `bus` (catch-all topic pattern) and the relay on
    /// `engine_bus` (the events the engine publishes there are republished on `bus`).
    pub fn new(
        bus: Bus,
        state: StateHandle,
        steps: Arc<StepsEngine>,
        engine_bus: Bus,
    ) -> Arc<Self> {
        let ing = Arc::new(Self {
            bus,
            engine_bus,
            state,
            steps,
            dispatched: Mutex::new(HashMap::new()),
        });
        // Forwarder on the real bus: one drainer, FIFO, so events are routed in arrival
        // order. Mirrored topics go to the transactional ingest; everything else is
        // dispatched by the binding engine (already on this bus, so no republish).
        let fwd = Arc::clone(&ing);
        let pat = Pattern::Topic(TopicPattern::parse("*").expect("'*' is a valid pattern"));
        ing.bus.subscribe(
            pat,
            None,
            "/*".to_string(),
            Arc::new(FnSink(Arc::new(move |msg: &Value| {
                fwd.on_bus_msg(msg);
            }))),
        );
        // Relay on the engine's private bus: the engine publishes `emit`-step events and
        // `binding-error` events there; external subscribers must see them on the real
        // bus. Marked as dispatched so the forwarder skips the relayed copy.
        let relay = Arc::clone(&ing);
        let pat = Pattern::Topic(TopicPattern::parse("*").expect("'*' is a valid pattern"));
        ing.engine_bus.subscribe(
            pat,
            None,
            "/*".to_string(),
            Arc::new(FnSink(Arc::new(move |msg: &Value| {
                relay.on_engine_msg(msg);
            }))),
        );
        ing
    }

    /// Forwarder callback (real bus): route one wire topic event in arrival order.
    pub fn on_bus_msg(self: &Arc<Self>, msg: &Value) {
        let Some(topic) = msg.get("topic").and_then(Value::as_str) else {
            return;
        };
        let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
        let ts_ms = msg.get("ts_ms").and_then(Value::as_u64).unwrap_or(0);
        let key = wire_key(topic, ts_ms, &payload);
        {
            let mut dispatched = self.dispatched.lock().unwrap();
            if let Some(count) = dispatched.get_mut(&key) {
                if *count > 0 {
                    *count -= 1;
                    if *count == 0 {
                        dispatched.remove(&key);
                    }
                    return; // relayed / already dispatched copy
                }
            }
        }
        self.route(topic, &payload, 0);
    }

    /// Relay callback (engine's private bus): republish the event on the real bus so
    /// external subscribers see it, marked so the forwarder skips the copy.
    fn on_engine_msg(self: &Arc<Self>, msg: &Value) {
        let Some(topic) = msg.get("topic").and_then(Value::as_str) else {
            return;
        };
        let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
        let ev = Event::topic(topic, payload);
        let key = event_key(&ev);
        {
            let mut dispatched = self.dispatched.lock().unwrap();
            *dispatched.entry(key).or_insert(0) += 1;
        }
        self.bus.publish(ev);
    }

    /// Route one event that is already on the real bus (forwarder path): mirrored
    /// topics through the transactional ingest, everything else to the binding engine.
    /// `depth` is the emit-recursion depth (0 for published events).
    pub fn route(self: &Arc<Self>, topic: &str, payload: &Value, depth: u32) {
        if mirror_path(topic).is_some() {
            if depth >= MAX_EMIT_DEPTH {
                let err = IpcError::new(
                    ErrorCode::ELimit,
                    format!(
                        "emit recursion cap {MAX_EMIT_DEPTH} reached: dispatch depth {depth} \
                         may not re-enter the ingest for {topic:?}"
                    ),
                );
                self.publish_ingest_error(topic, depth, &err);
                return;
            }
            let _ = self.ingest(topic, payload, depth);
        } else {
            self.steps.dispatch(topic, payload, depth);
        }
    }

    /// Ingest one mirrored-topic event directly (no bus round trip): returns the
    /// committing rev, or the rejection error — in which case no state changed.
    pub fn try_ingest(
        self: &Arc<Self>,
        topic: &str,
        payload: &Value,
        depth: u32,
    ) -> Result<u64, IpcError> {
        if mirror_path(topic).is_none() {
            return Err(IpcError::new(
                ErrorCode::ESchema,
                format!("topic {topic:?} is not a mirrored (ingest) topic"),
            ));
        }
        self.ingest(topic, payload, depth)
    }

    /// Queue an emit produced by the ingest (after its transaction committed): publish
    /// it on the real bus (marked dispatched, so the forwarder skips the copy) and
    /// dispatch it at `depth`. No reentrancy inside the ingest transaction.
    fn route_emitted(self: &Arc<Self>, topic: &str, payload: &Value, depth: u32) {
        if mirror_path(topic).is_some() {
            if depth >= MAX_EMIT_DEPTH {
                let err = IpcError::new(
                    ErrorCode::ELimit,
                    format!(
                        "emit recursion cap {MAX_EMIT_DEPTH} reached: dispatch depth {depth} \
                         may not re-enter the ingest for {topic:?}"
                    ),
                );
                self.publish_ingest_error(topic, depth, &err);
                return;
            }
            let _ = self.ingest(topic, payload, depth);
        } else {
            let ev = Event::topic(topic, payload.clone());
            let key = event_key(&ev);
            {
                let mut dispatched = self.dispatched.lock().unwrap();
                *dispatched.entry(key).or_insert(0) += 1;
            }
            self.bus.publish(ev);
            self.steps.dispatch(topic, payload, depth);
        }
    }

    /// Ingest one mirrored-topic event: validate the payload, then commit ONE
    /// transaction = [mirror op] + [matching bindings' set ops] with `if_rev`.
    /// Returns the committing rev. An invalid payload changes no state at all.
    fn ingest(self: &Arc<Self>, topic: &str, payload: &Value, depth: u32) -> Result<u64, IpcError> {
        let path = mirror_path(topic).expect("caller checked mirror_path");
        let base = match validate_mirror_payload(payload) {
            Ok(base) => base,
            Err(err) => {
                self.publish_ingest_error(topic, depth, &err);
                return Err(err);
            }
        };
        let mut last_conflict: Option<u64> = None;
        for _attempt in 0..MAX_COMMIT_ATTEMPTS {
            let rev = match self.state.rev() {
                Ok(rev) => rev,
                Err(err) => {
                    self.publish_ingest_error(topic, depth, &err);
                    return Err(err);
                }
            };
            let new_rev = rev + 1;
            let mut mirror = base.clone();
            mirror["at_rev"] = json!(new_rev);
            // Candidate document: snapshot + mirror op, validated locally. Every binding
            // set op is pre-validated the same way, so the committed transaction cannot
            // fail with E_SCHEMA and a failing step never rolls the mirror back.
            let mut candidate = match self.state.snapshot() {
                Ok(doc) => doc,
                Err(err) => {
                    self.publish_ingest_error(topic, depth, &err);
                    return Err(err);
                }
            };
            let mirror_path_parsed = match Path::parse(path) {
                Ok(p) => p,
                Err(e) => {
                    let err = crate::state::state_error(e);
                    self.publish_ingest_error(topic, depth, &err);
                    return Err(err);
                }
            };
            if let Err(e) = mirror_path_parsed.validate_write() {
                let err = crate::state::state_error(e);
                self.publish_ingest_error(topic, depth, &err);
                return Err(err);
            }
            if let Err(e) = document::set(&mut candidate, &mirror_path_parsed, mirror.clone()) {
                let err = crate::state::state_error(e);
                self.publish_ingest_error(topic, depth, &err);
                return Err(err);
            }
            if let Err(e) = validate_document(&candidate) {
                let err = crate::state::state_error(e);
                self.publish_ingest_error(topic, depth, &err);
                return Err(err);
            }
            let mut ops: Vec<Op> = vec![Op::set(path, mirror.clone())];
            let mut deferred: Vec<Deferred> = Vec::new();
            // Matching bindings: exact topic first, then wildcards, array order (DS-042).
            let bindings = self
                .state
                .get("/bindings", None)
                .map(|r| r.value)
                .unwrap_or_else(|_| json!([]));
            for binding in match_bindings(&bindings, topic) {
                if let Some(when) = binding.get("when") {
                    let state = self.state.clone();
                    let overlay = mirror.clone();
                    let fires =
                        eval_when(when, move |p| resolve_overlay(p, &overlay, &state), payload);
                    if !fires {
                        continue;
                    }
                }
                let Some(steps) = binding.get("steps").and_then(Value::as_array) else {
                    continue;
                };
                if steps.is_empty() {
                    continue;
                }
                let id = binding
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let mut i = 0usize;
                while i < steps.len() {
                    let step = &steps[i];
                    let kind = step.get("do").and_then(Value::as_str).unwrap_or("?");
                    match kind {
                        "set" => match plan_set_step(step) {
                            Ok(op) => {
                                let mut cand = candidate.clone();
                                let ok =
                                    apply_op(&mut cand, &op).and_then(|_| validate_document(&cand));
                                match ok {
                                    Ok(()) => {
                                        candidate = cand;
                                        ops.push(op);
                                    }
                                    Err(e) => {
                                        // step fails schema/path validation: dropped from
                                        // the transaction, mirror still commits
                                        deferred.push(Deferred::Error {
                                            binding: id.clone(),
                                            step: i,
                                            err: crate::state::state_error(e),
                                        });
                                    }
                                }
                            }
                            Err(err) => deferred.push(Deferred::Error {
                                binding: id.clone(),
                                step: i,
                                err,
                            }),
                        },
                        "emit" => {
                            let step_topic = step.get("topic").and_then(Value::as_str);
                            let step_payload = step.get("payload").cloned().unwrap_or(Value::Null);
                            match step_topic {
                                Some(t) if topics::validate_topic(t).is_ok() => {
                                    if depth + 1 >= MAX_EMIT_DEPTH {
                                        deferred.push(Deferred::Error {
                                            binding: id.clone(),
                                            step: i,
                                            err: IpcError::new(
                                                ErrorCode::ELimit,
                                                format!(
                                                    "emit recursion cap {MAX_EMIT_DEPTH} reached: \
                                                     dispatch depth {depth} (trigger {topic:?}) \
                                                     may not emit {t:?}"
                                                ),
                                            ),
                                        });
                                    } else {
                                        deferred.push(Deferred::Emit {
                                            topic: t.to_string(),
                                            payload: step_payload,
                                        });
                                    }
                                }
                                _ => deferred.push(Deferred::Error {
                                    binding: id.clone(),
                                    step: i,
                                    err: IpcError::new(
                                        ErrorCode::ESchema,
                                        format!("emit step needs a valid topic, got {step}"),
                                    ),
                                }),
                            }
                        }
                        "sleep" => {
                            let ms = step.get("ms").and_then(Value::as_u64);
                            match ms {
                                Some(ms) if (1..=MAX_SLEEP_MS).contains(&ms) => {
                                    deferred.push(Deferred::Sleep {
                                        binding: binding.clone(),
                                        steps: steps.clone(),
                                        start: i + 1,
                                        ms,
                                        trigger: topic.to_string(),
                                    });
                                }
                                _ => deferred.push(Deferred::Error {
                                    binding: id.clone(),
                                    step: i,
                                    err: IpcError::new(
                                        ErrorCode::ESchema,
                                        format!("sleep step 'ms' must be 1..={MAX_SLEEP_MS}, got {step}"),
                                    ),
                                }),
                            }
                            break; // remaining steps continue on the timer thread, post-commit
                        }
                        "plugin" => {
                            let pid = step.get("id").and_then(Value::as_str).unwrap_or("");
                            let action = step.get("action").and_then(Value::as_str).unwrap_or("");
                            deferred.push(Deferred::Error {
                                binding: id.clone(),
                                step: i,
                                err: IpcError::new(
                                    ErrorCode::ECap,
                                    format!(
                                        "plugin step '{pid}.{action}' cannot run yet: the plugin \
                                         host lands with DS-050 (stubbed E_CAP; the mirror still commits)"
                                    ),
                                ),
                            });
                        }
                        "exec" => {
                            let argv_ok =
                                step.get("argv").and_then(Value::as_array).is_some_and(|a| {
                                    !a.is_empty()
                                        && a.len() <= 64
                                        && a.iter().all(Value::is_string)
                                        && a[0].as_str().is_some_and(|s| s.starts_with('/'))
                                });
                            let err = if argv_ok {
                                IpcError::new(
                                    ErrorCode::ECap,
                                    "exec step fails closed until the DS-043 policy gate is wired \
                                     (E_CAP stands in for E_POLICY); no process is spawned"
                                        .to_string(),
                                )
                            } else {
                                IpcError::new(
                                    ErrorCode::ESchema,
                                    "exec step needs a non-empty 'argv' array of strings with an \
                                     absolute argv[0] (argv-only, never through a shell)"
                                        .to_string(),
                                )
                            };
                            deferred.push(Deferred::Error {
                                binding: id.clone(),
                                step: i,
                                err,
                            });
                        }
                        other => deferred.push(Deferred::Error {
                            binding: id.clone(),
                            step: i,
                            err: IpcError::new(
                                ErrorCode::ESchema,
                                format!(
                                    "unknown step 'do' {other:?} — the vocabulary is closed \
                                     (set, emit, plugin, exec, sleep)"
                                ),
                            ),
                        }),
                    }
                    i += 1;
                }
            }
            // ONE transaction: mirror + binding set steps, atomic via if_rev.
            let tx = Transaction::new(ops).with_if_rev(rev);
            match self.state.set(tx) {
                Ok(committed) => {
                    debug_assert_eq!(committed, new_rev);
                    self.run_deferred(topic, depth, deferred);
                    return Ok(committed);
                }
                Err(err) if err.code == ErrorCode::EConflict => {
                    last_conflict = err.current_rev;
                    continue; // lost the race: rebuild on the new rev
                }
                Err(err) => {
                    self.publish_ingest_error(topic, depth, &err);
                    return Err(err);
                }
            }
        }
        let err = IpcError::new(
            ErrorCode::EConflict,
            format!(
                "ingest of {topic:?} could not commit within {MAX_COMMIT_ATTEMPTS} attempts \
                 (last current_rev: {})",
                last_conflict
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "?".to_string())
            ),
        );
        self.publish_ingest_error(topic, depth, &err);
        Err(err)
    }

    /// Run the collected actions after the ingest transaction committed (no reentrancy
    /// inside the transaction): error events, queued emits, sleep continuations.
    fn run_deferred(self: &Arc<Self>, trigger: &str, depth: u32, deferred: Vec<Deferred>) {
        for d in deferred {
            match d {
                Deferred::Error { binding, step, err } => {
                    self.publish_step_error(&binding, step, &err, trigger, depth);
                }
                Deferred::Emit { topic, payload } => {
                    self.route_emitted(&topic, &payload, depth + 1);
                }
                Deferred::Sleep {
                    binding,
                    steps,
                    start,
                    ms,
                    trigger: sleep_trigger,
                } => {
                    let this = Arc::clone(self);
                    let cont_binding = binding.clone();
                    let cont_steps = steps.clone();
                    let cont_trigger = sleep_trigger.clone();
                    let spawned = std::thread::Builder::new()
                        .name("ds-router-ingest-sleep".into())
                        .spawn(move || {
                            std::thread::sleep(Duration::from_millis(ms));
                            this.run_continuation(
                                &cont_binding,
                                &cont_steps,
                                start,
                                &cont_trigger,
                                depth,
                            );
                        });
                    if let Err(e) = spawned {
                        lifecycle::log(
                            "error",
                            "ds-daemon::router::ingest",
                            &format!("cannot spawn the sleep timer thread: {e}; continuing inline"),
                        );
                        self.run_continuation(&binding, &steps, start, &sleep_trigger, depth);
                    }
                }
            }
        }
    }

    /// The steps of one binding after its `sleep` (own transaction(s) — the ingest
    /// transaction already committed). Mirrors the DS-042 engine's failure modes.
    fn run_continuation(
        self: &Arc<Self>,
        binding: &Value,
        steps: &[Value],
        start: usize,
        trigger: &str,
        depth: u32,
    ) {
        let id = binding
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let mut i = start;
        while i < steps.len() {
            let step = &steps[i];
            match step.get("do").and_then(Value::as_str).unwrap_or("") {
                "set" => match plan_set_step(step) {
                    Ok(op) => {
                        if let Err(err) = self.state.set(Transaction::new(vec![op])) {
                            self.publish_step_error(&id, i, &err, trigger, depth);
                        }
                    }
                    Err(err) => self.publish_step_error(&id, i, &err, trigger, depth),
                },
                "emit" => {
                    let t = step.get("topic").and_then(Value::as_str);
                    let p = step.get("payload").cloned().unwrap_or(Value::Null);
                    match t {
                        Some(t) if topics::validate_topic(t).is_ok() => {
                            if depth + 1 >= MAX_EMIT_DEPTH {
                                self.publish_step_error(
                                    &id,
                                    i,
                                    &IpcError::new(
                                        ErrorCode::ELimit,
                                        format!(
                                            "emit recursion cap {MAX_EMIT_DEPTH} reached at depth {depth}"
                                        ),
                                    ),
                                    trigger,
                                    depth,
                                );
                            } else {
                                self.route_emitted(t, &p, depth + 1);
                            }
                        }
                        _ => self.publish_step_error(
                            &id,
                            i,
                            &IpcError::new(
                                ErrorCode::ESchema,
                                format!("emit step needs a valid topic, got {step}"),
                            ),
                            trigger,
                            depth,
                        ),
                    }
                }
                "sleep" => {
                    let ms = step.get("ms").and_then(Value::as_u64);
                    match ms {
                        Some(ms) if (1..=MAX_SLEEP_MS).contains(&ms) => {
                            let this = Arc::clone(self);
                            let binding = binding.clone();
                            let steps = steps.to_vec();
                            let trigger = trigger.to_string();
                            let spawned = std::thread::Builder::new()
                                .name("ds-router-ingest-sleep".into())
                                .spawn(move || {
                                    std::thread::sleep(Duration::from_millis(ms));
                                    this.run_continuation(&binding, &steps, i + 1, &trigger, depth);
                                });
                            if spawned.is_err() {
                                // never crash: continue inline without the delay
                            } else {
                                return;
                            }
                        }
                        _ => self.publish_step_error(
                            &id,
                            i,
                            &IpcError::new(
                                ErrorCode::ESchema,
                                format!("sleep step 'ms' must be 1..={MAX_SLEEP_MS}, got {step}"),
                            ),
                            trigger,
                            depth,
                        ),
                    }
                }
                other => self.publish_step_error(
                    &id,
                    i,
                    &IpcError::new(
                        ErrorCode::ESchema,
                        format!("unknown step 'do' {other:?} — the vocabulary is closed (set, emit, plugin, exec, sleep)"),
                    ),
                    trigger,
                    depth,
                ),
            }
            i += 1;
        }
    }

    /// Publish the ingest-level error event (`deck/system/binding-error`,
    /// `"stage":"ingest"`) and log it. Called when the event payload is invalid or the
    /// commit failed — in both cases no state was changed.
    fn publish_ingest_error(&self, topic: &str, depth: u32, err: &IpcError) {
        lifecycle::log(
            "warn",
            "ds-daemon::router::ingest",
            &format!(
                "ingest of {topic:?} rejected (depth {depth}): {} {} — no state change",
                err.code.as_str(),
                err.message
            ),
        );
        self.bus.publish(Event::topic(
            BINDING_ERROR_TOPIC,
            json!({
                "stage": "ingest",
                "topic": topic,
                "depth": depth,
                "error": {"code": err.code.as_str(), "message": err.message},
            }),
        ));
    }

    /// Publish a binding-step error event after the commit (mirror already committed).
    fn publish_step_error(
        &self,
        binding: &str,
        step: usize,
        err: &IpcError,
        trigger: &str,
        depth: u32,
    ) {
        lifecycle::log(
            "warn",
            "ds-daemon::router::ingest",
            &format!(
                "binding {binding} step {step} failed with {}: {} (trigger {trigger:?}, depth {depth}) — \
                 the ingest mirror still committed",
                err.code.as_str(),
                err.message
            ),
        );
        self.bus.publish(Event::topic(
            BINDING_ERROR_TOPIC,
            json!({
                "stage": "ingest",
                "binding": binding,
                "step": step,
                "topic": trigger,
                "depth": depth,
                "error": {"code": err.code.as_str(), "message": err.message},
            }),
        ));
    }
}

/// Matching bindings for a topic, in dispatch order: exact topic patterns first, then
/// `*` wildcards, array order within each group (actions.md §2; identical to the DS-042
/// engine's lookup in `router/steps.rs`).
fn match_bindings<'a>(bindings: &'a Value, topic: &str) -> Vec<&'a Value> {
    let Some(arr) = bindings.as_array() else {
        return Vec::new();
    };
    let mut matched: Vec<(&Value, bool)> = Vec::new();
    for b in arr {
        let Some(btopic) = b.get("topic").and_then(Value::as_str) else {
            continue;
        };
        match TopicPattern::parse(btopic) {
            Ok(p) if p.matches(topic) => matched.push((b, p.is_wildcard())),
            _ => {}
        }
    }
    matched.sort_by_key(|(_, wildcard)| *wildcard); // stable: array order preserved
    matched.into_iter().map(|(b, _)| b).collect()
}

/// Resolve a `when` `path` operand during the ingest: paths under `/focus` see the
/// candidate mirror (the new value, S7), everything else the committed state.
fn resolve_overlay(path: &str, mirror: &Value, state: &StateHandle) -> Option<Value> {
    if path == FOCUS_PATH || path.starts_with("/focus/") {
        let mut cur = mirror;
        for seg in path.trim_start_matches('/').split('/').skip(1) {
            cur = cur.get(seg)?;
        }
        Some(cur.clone())
    } else {
        state.get(path, None).ok().map(|r| r.value)
    }
}

/// Build a `set` op from a step, shape-checked like the engine does.
fn plan_set_step(step: &Value) -> Result<Op, IpcError> {
    let err = |m: &str| IpcError::new(ErrorCode::ESchema, m.to_string());
    let path = step
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| err("set step needs a string 'path'"))?;
    let value = step
        .get("value")
        .cloned()
        .ok_or_else(|| err("set step needs a 'value'"))?;
    Ok(Op::set(path, value))
}

/// Apply one op to a local candidate document exactly like `Store::commit` does
/// (path parse + write validation + document set/delete).
fn apply_op(doc: &mut Value, op: &Op) -> ds_state::Result<()> {
    let path = Path::parse(op.path())?;
    path.validate_write()?;
    match op {
        Op::Set { value, .. } => document::set(doc, &path, value.clone()),
        Op::Delete { .. } => document::delete(doc, &path),
    }
}

/// Marker key of a published topic event (must match the wire shape the forwarder sees).
fn event_key(ev: &Event) -> String {
    match ev {
        Event::Topic {
            topic,
            ts_ms,
            payload,
            ..
        } => wire_key(topic, *ts_ms, payload),
        Event::State { .. } => String::new(),
    }
}

fn wire_key(topic: &str, ts_ms: u64, payload: &Value) -> String {
    format!(
        "{topic}\u{1}{ts_ms}\u{1}{}",
        serde_json::to_string(payload).unwrap_or_else(|_| "null".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::bus::Bus;
    use crate::router::Engine;
    use crate::state::StateTask;
    use ds_state::Store;
    use serde_json::json;
    use std::sync::mpsc::Receiver;
    use std::thread::JoinHandle;
    use std::time::Instant;

    /// (ingest via the mod.rs Engine wrapper, state, state thread, real bus, engine bus)
    fn test_stack() -> (Arc<Engine>, StateHandle, JoinHandle<()>, Bus) {
        let bus = Bus::new();
        let (state, t) = StateTask::spawn(Store::fresh(), None, bus.clone());
        let engine = Engine::new(bus.clone(), state.clone());
        (engine, state, t, bus)
    }

    fn bind(state: &StateHandle, topic: &str, steps: Value, when: Option<Value>) -> String {
        let mut b = json!({"id": "b0000", "topic": topic, "steps": steps});
        if let Some(w) = when {
            b["when"] = w;
        }
        let (id, _) = state.bind(b).unwrap();
        id
    }

    fn collect(bus: &Bus, wire: &str) -> Receiver<Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        let sink = Arc::new(FnSink(Arc::new(move |msg: &Value| {
            let _ = tx.send(msg.clone());
        })));
        bus.subscribe(
            Pattern::parse_wire(wire).unwrap(),
            None,
            wire.to_string(),
            sink,
        );
        rx
    }

    fn get(state: &StateHandle, path: &str) -> Value {
        state.get(path, None).unwrap().value
    }

    fn recv_all(rx: &Receiver<Value>, wait: Duration) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(msg) = rx.recv_timeout(wait) {
            out.push(msg);
        }
        out
    }

    fn focus_payload(app: &str) -> Value {
        json!({"app": app, "title": "Window", "pid": 4242})
    }

    #[test]
    fn focus_ingest_mirrors_and_dispatches_bindings_in_one_transaction() {
        let (engine, state, t, bus) = test_stack();
        let events = collect(&bus, "/controls/*");
        let focus_events = collect(&bus, "/focus");
        bind(
            &state,
            FOCUS_TOPIC,
            json!([{"do": "set", "path": "/controls/keys/3/image",
                    "value": {"kind": "color", "value": "#00ff00"}}]),
            Some(json!({"payload": "app", "op": "eq", "value": "code"})),
        );
        let rev0 = state.rev().unwrap();
        engine.dispatch(FOCUS_TOPIC, &focus_payload("code"), 0);
        let rev1 = state.rev().unwrap();
        assert_eq!(
            rev1,
            rev0 + 1,
            "mirror + binding set = exactly ONE rev bump"
        );
        // /focus mirrored with at_rev = the committing rev
        let focus = get(&state, FOCUS_PATH);
        assert_eq!(
            focus,
            json!({"app": "code", "title": "Window", "pid": 4242, "at_rev": rev1})
        );
        // the binding's set step ran in the SAME transaction: its state event carries
        // the same rev as the /focus mirror event (one event batch, one rev)
        let msgs = recv_all(&events, Duration::from_millis(300));
        let img_evs: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["path"] == "/controls/keys/3/image")
            .collect();
        assert_eq!(img_evs.len(), 1, "{msgs:?}");
        assert_eq!(
            img_evs[0]["rev"],
            json!(rev1),
            "set step committed in the ingest transaction"
        );
        assert_eq!(
            img_evs[0]["value"],
            json!({"kind": "color", "value": "#00ff00"})
        );
        let fmsgs = recv_all(&focus_events, Duration::from_millis(300));
        assert_eq!(fmsgs.len(), 1);
        assert_eq!(fmsgs[0]["path"], FOCUS_PATH);
        assert_eq!(fmsgs[0]["rev"], json!(rev1));
        assert_eq!(fmsgs[0]["value"]["at_rev"], json!(rev1));
        // payload mismatch -> no set step, but the mirror still updates (one rev)
        engine.dispatch(FOCUS_TOPIC, &focus_payload("terminal"), 0);
        let rev2 = state.rev().unwrap();
        assert_eq!(rev2, rev1 + 1);
        assert_eq!(get(&state, "/focus/app"), json!("terminal"));
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev2));
        let msgs = recv_all(&events, Duration::from_millis(300));
        assert!(
            msgs.iter().all(|m| m["path"] != "/controls/keys/3/image"),
            "the non-matching binding did not run again: {msgs:?}"
        );
        let fmsgs = recv_all(&focus_events, Duration::from_millis(300));
        assert_eq!(fmsgs.len(), 1, "second event -> one more /focus event");
        assert_eq!(fmsgs[0]["rev"], json!(rev2));
        assert_eq!(fmsgs[0]["value"]["app"], json!("terminal"));
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn when_on_focus_path_sees_the_new_value_in_the_same_dispatch() {
        let (engine, state, t, _bus) = test_stack();
        // path-based when on /focus/app: must see the NEW mirror (S7, not stale)
        bind(
            &state,
            FOCUS_TOPIC,
            json!([{"do": "set", "path": "/controls/keys/7/label", "value": "focus-path-fired"}]),
            Some(json!({"path": "/focus/app", "op": "eq", "value": "code"})),
        );
        // a binding on another topic reading the committed /focus afterwards
        bind(
            &state,
            "app/demo/level",
            json!([{"do": "set", "path": "/controls/keys/8/label", "value": "level-fired"}]),
            Some(json!({"path": "/focus/app", "op": "eq", "value": "code"})),
        );
        engine.dispatch(FOCUS_TOPIC, &focus_payload("terminal"), 0);
        assert!(
            state.get("/controls/keys/7/label", None).is_err(),
            "when /focus/app eq code must not fire for app=terminal"
        );
        let rev0 = state.rev().unwrap();
        engine.dispatch(FOCUS_TOPIC, &focus_payload("code"), 0);
        assert_eq!(
            get(&state, "/controls/keys/7/label"),
            json!("focus-path-fired"),
            "the path-based when saw the new /focus in the same dispatch"
        );
        assert_eq!(state.rev().unwrap(), rev0 + 1);
        // the committed /focus is readable by later dispatches (at_rev == dispatch rev)
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev0 + 1));
        engine.dispatch("app/demo/level", &json!({}), 0);
        assert_eq!(get(&state, "/controls/keys/8/label"), json!("level-fired"));
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn invalid_focus_payload_changes_nothing_and_publishes_a_clear_error() {
        let (engine, state, t, bus) = test_stack();
        let errors = collect(&bus, "/deck/system/*");
        bind(
            &state,
            FOCUS_TOPIC,
            json!([{"do": "set", "path": "/controls/keys/3/image",
                    "value": {"kind": "color", "value": "#00ff00"}}]),
            None,
        );
        let rev0 = state.rev().unwrap();
        for bad in [
            json!({"app": "code", "title": "W", "pid": "4242"}), // pid not an integer
            json!({"app": "code", "title": "W", "pid": 1, "extra": 1}), // unknown field
            json!("not an object"),
            json!([]),
        ] {
            let r = engine.try_ingest(FOCUS_TOPIC, &bad, 0);
            assert!(r.is_err(), "{bad} must be rejected");
            assert_eq!(r.unwrap_err().code, ErrorCode::ESchema);
            assert_eq!(state.rev().unwrap(), rev0, "no state change for {bad}");
            assert!(state.get(FOCUS_PATH, None).is_err(), "/focus untouched");
            assert!(
                state.get("/controls/keys/3/image", None).is_err(),
                "no binding step ran for {bad}"
            );
        }
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let errs: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(
            errs.len(),
            4,
            "one clear error event per rejected payload: {msgs:?}"
        );
        for e in &errs {
            assert_eq!(e["payload"]["stage"], json!("ingest"));
            assert_eq!(e["payload"]["error"]["code"], json!("E_SCHEMA"));
            assert_eq!(e["payload"]["topic"], json!(FOCUS_TOPIC));
            ds_ipc::message::validate_server_message(e).unwrap();
        }
        // a valid payload still works afterwards
        engine.dispatch(FOCUS_TOPIC, &focus_payload("code"), 0);
        assert_eq!(get(&state, "/focus/app"), json!("code"));
        assert_eq!(state.rev().unwrap(), rev0 + 1);
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn failing_binding_step_does_not_roll_back_the_mirror() {
        let (engine, state, t, bus) = test_stack();
        let errors = collect(&bus, "/deck/system/*");
        bind(
            &state,
            FOCUS_TOPIC,
            json!([
                {"do": "set", "path": "/controls/keys/5/brightness", "value": "high"}, // schema-invalid
                {"do": "set", "path": "/controls/keys/5/label", "value": "ok"}
            ]),
            None,
        );
        let rev0 = state.rev().unwrap();
        engine.dispatch(FOCUS_TOPIC, &focus_payload("code"), 0);
        let rev1 = state.rev().unwrap();
        assert_eq!(rev1, rev0 + 1, "one transaction, one rev bump");
        // the mirror committed despite the failing step
        assert_eq!(get(&state, "/focus/app"), json!("code"));
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev1));
        // the valid set step ran in the same transaction; the invalid one did not
        assert_eq!(get(&state, "/controls/keys/5/label"), json!("ok"));
        assert!(state.get("/controls/keys/5/brightness", None).is_err());
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let errs: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(errs.len(), 1, "{msgs:?}");
        assert_eq!(errs[0]["payload"]["step"], json!(0));
        assert_eq!(errs[0]["payload"]["error"]["code"], json!("E_SCHEMA"));
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn two_quick_events_are_ingested_in_arrival_order() {
        let (engine, state, t, bus) = test_stack();
        let events = collect(&bus, "/focus");
        bind(
            &state,
            FOCUS_TOPIC,
            json!([{"do": "set", "path": "/controls/keys/4/label", "value": "fired"}]),
            None,
        );
        let rev0 = state.rev().unwrap();
        engine.dispatch(FOCUS_TOPIC, &focus_payload("code"), 0);
        engine.dispatch(FOCUS_TOPIC, &focus_payload("terminal"), 0);
        let rev2 = state.rev().unwrap();
        assert_eq!(
            rev2,
            rev0 + 2,
            "two events = exactly two rev bumps, in order"
        );
        assert_eq!(
            get(&state, "/focus/app"),
            json!("terminal"),
            "last event wins"
        );
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev2));
        // both events produced one /focus state event each, with consecutive revs
        let msgs = recv_all(&events, Duration::from_secs(2));
        let focus_evs: Vec<&Value> = msgs.iter().filter(|m| m["path"] == FOCUS_PATH).collect();
        assert_eq!(focus_evs.len(), 2, "{msgs:?}");
        assert_eq!(focus_evs[0]["rev"], json!(rev0 + 1));
        assert_eq!(focus_evs[1]["rev"], json!(rev0 + 2));
        assert_eq!(focus_evs[0]["value"]["app"], json!("code"));
        assert_eq!(focus_evs[1]["value"]["app"], json!("terminal"));
        // the binding fired for both, each in its own event's transaction
        assert_eq!(get(&state, "/controls/keys/4/label"), json!("fired"));
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn emit_step_is_queued_after_the_transaction_not_inside_it() {
        let (engine, state, t, bus) = test_stack();
        let app_events = collect(&bus, "/app/*");
        let ctl_events = collect(&bus, "/controls/*");
        bind(
            &state,
            FOCUS_TOPIC,
            json!([{"do": "emit", "topic": "app/demo/fromfocus", "payload": {"n": 1}}]),
            None,
        );
        bind(
            &state,
            "app/demo/fromfocus",
            json!([{"do": "set", "path": "/controls/keys/6/label", "value": "after-emit"}]),
            None,
        );
        let rev0 = state.rev().unwrap();
        engine.dispatch(FOCUS_TOPIC, &focus_payload("code"), 0);
        // the ingest transaction committed exactly one rev: the mirror carries it
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev0 + 1));
        // the emitted event reached bus subscribers exactly once
        let msgs = recv_all(&app_events, Duration::from_secs(2));
        let emitted: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == "app/demo/fromfocus")
            .collect();
        assert_eq!(emitted.len(), 1, "{msgs:?}");
        assert_eq!(emitted[0]["payload"], json!({"n": 1}));
        // the binding on the emitted topic ran AFTER the ingest transaction: its state
        // event carries a strictly later rev than the /focus mirror
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut label_ev = None;
        while Instant::now() < deadline {
            let msgs = recv_all(&ctl_events, Duration::from_millis(200));
            if let Some(ev) = msgs.iter().find(|m| m["path"] == "/controls/keys/6/label") {
                label_ev = Some((*ev).clone());
                break;
            }
        }
        let label_ev = label_ev.expect("the emitted binding never ran");
        assert_eq!(label_ev["value"], json!("after-emit"));
        assert!(
            label_ev["rev"].as_u64().unwrap() > rev0 + 1,
            "the emit-triggered set must be queued after the ingest transaction (rev {} <= {})",
            label_ev["rev"],
            rev0 + 1
        );
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn wiring_through_the_real_bus_publish_and_forwarder() {
        // End-to-end wiring: publish on the real bus -> forwarder -> ingest; the mirror
        // and the binding set commit in one rev; a non-mirrored topic still reaches the
        // steps engine through the forwarder.
        let (engine, state, t, bus) = test_stack();
        let _ = &engine;
        bind(
            &state,
            FOCUS_TOPIC,
            json!([{"do": "set", "path": "/controls/keys/3/image",
                    "value": {"kind": "color", "value": "#00ff00"}}]),
            None,
        );
        bind(
            &state,
            "app/demo/plain",
            json!([{"do": "set", "path": "/controls/keys/9/label", "value": "plain-fired"}]),
            None,
        );
        let rev0 = state.rev().unwrap();
        bus.publish(Event::topic(FOCUS_TOPIC, focus_payload("code")));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if state.get(FOCUS_PATH, None).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the bus-published focus event was not ingested"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(get(&state, "/focus/app"), json!("code"));
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev0 + 1));
        assert_eq!(
            state.get("/controls/keys/3/image", None).unwrap().rev,
            Some(rev0 + 1),
            "mirror + binding set in one transaction via the bus wiring"
        );
        assert_eq!(state.rev().unwrap(), rev0 + 1);
        // non-mirrored topic still routed to the steps engine
        bus.publish(Event::topic("app/demo/plain", json!({})));
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(r) = state.get("/controls/keys/9/label", None) {
                assert_eq!(r.value, json!("plain-fired"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "non-mirrored event did not reach the engine"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn engine_published_events_are_relayed_to_the_real_bus_exactly_once() {
        // A binding on a non-mirrored topic with a failing plugin step: the engine
        // publishes deck/system/binding-error on its private bus; the relay must put it
        // on the real bus exactly once (the forwarder skips the relayed copy).
        let (_engine, state, t, bus) = test_stack();
        let errors = collect(&bus, "/deck/system/*");
        bind(
            &state,
            "app/demo/plug",
            json!([{"do": "plugin", "id": "demo", "action": "bump"}]),
            None,
        );
        bus.publish(Event::topic("app/demo/plug", json!({})));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut got = 0;
        while Instant::now() < deadline && got < 1 {
            let msgs = recv_all(&errors, Duration::from_millis(200));
            got += msgs
                .iter()
                .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
                .count();
        }
        assert_eq!(got, 1, "exactly one binding-error on the real bus");
        // ... and it stays exactly one (no loop through the forwarder)
        std::thread::sleep(Duration::from_millis(300));
        let msgs = recv_all(&errors, Duration::from_millis(300));
        assert!(
            msgs.iter()
                .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
                .count()
                == 0,
            "the relayed event must not be re-dispatched: {msgs:?}"
        );
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn validate_mirror_payload_accepts_the_producer_shape_with_optional_pid() {
        // DS-114: pid absent or null is accepted (and omitted from the mirror base)
        assert_eq!(
            validate_mirror_payload(&json!({"app": "a", "title": "t"})).unwrap(),
            json!({"app": "a", "title": "t"})
        );
        assert_eq!(
            validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": null})).unwrap(),
            json!({"app": "a", "title": "t"})
        );
        assert_eq!(
            validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": 1})).unwrap(),
            json!({"app": "a", "title": "t", "pid": 1})
        );
        assert!(validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": -1})).is_ok()); // integer
                                                                                                 // a present non-integer pid is still rejected
        assert!(validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": "1"})).is_err());
        assert!(validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": 1.5})).is_err());
        assert!(validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": true})).is_err());
        assert!(validate_mirror_payload(&json!({"app": 1, "title": "t", "pid": 1})).is_err());
        assert!(
            validate_mirror_payload(&json!({"app": "a", "title": "t", "pid": 1, "x": 0})).is_err()
        );
        assert!(validate_mirror_payload(&json!(null)).is_err());
        assert!(validate_mirror_payload(&json!("x")).is_err());
    }

    #[test]
    fn focus_event_without_pid_is_accepted_and_mirrors() {
        // DS-114: {"app":"steam","title":"Steam"} (no pid) must be accepted and update
        // /focus; {"pid":"x"} must still be rejected with no state change.
        let (engine, state, t, _bus) = test_stack();
        let rev0 = state.rev().unwrap();
        engine.dispatch(FOCUS_TOPIC, &json!({"app": "steam", "title": "Steam"}), 0);
        let rev1 = state.rev().unwrap();
        assert_eq!(
            rev1,
            rev0 + 1,
            "pid-less focus event = exactly one rev bump"
        );
        assert_eq!(
            get(&state, FOCUS_PATH),
            json!({"app": "steam", "title": "Steam", "at_rev": rev1}),
            "the /focus mirror carries no pid when the event had none"
        );
        assert!(get(&state, "/focus").get("pid").is_none());
        // an explicit null pid is equally accepted
        engine.dispatch(
            FOCUS_TOPIC,
            &json!({"app": "steam", "title": "Steam", "pid": null}),
            0,
        );
        let rev2 = state.rev().unwrap();
        assert_eq!(rev2, rev1 + 1);
        assert_eq!(
            get(&state, FOCUS_PATH),
            json!({"app": "steam", "title": "Steam", "at_rev": rev2})
        );
        // a non-integer pid is still rejected, with no state change
        let r = engine.try_ingest(
            FOCUS_TOPIC,
            &json!({"app": "steam", "title": "Steam", "pid": "x"}),
            0,
        );
        assert!(r.is_err());
        assert_eq!(r.unwrap_err().code, ErrorCode::ESchema);
        assert_eq!(
            state.rev().unwrap(),
            rev2,
            "no state change for a non-integer pid"
        );
        assert_eq!(get(&state, "/focus/at_rev"), json!(rev2));
        // a payload with an integer pid still mirrors it
        engine.dispatch(
            FOCUS_TOPIC,
            &json!({"app": "code", "title": "Editor", "pid": 4242}),
            0,
        );
        assert_eq!(
            get(&state, FOCUS_PATH),
            json!({"app": "code", "title": "Editor", "pid": 4242, "at_rev": rev2 + 1})
        );
        state.stop();
        t.join().unwrap();
    }

    #[test]
    fn mirror_path_maps_only_the_focus_topic() {
        assert_eq!(mirror_path(FOCUS_TOPIC), Some(FOCUS_PATH));
        assert_eq!(mirror_path("app/focus/other"), None);
        assert_eq!(mirror_path("deck/control/keys.0/down"), None);
        assert_eq!(mirror_path("daemon/start"), None);
    }
}
