//! The binding step engine (ticket DS-042; contract `docs/contracts/actions.md` §2-3,
//! component `docs/components/event-router.md` "Routing model" / "Failure modes").
//!
//! [`Engine`] subscribes to the event bus on the catch-all topic pattern `*`, so every
//! producer (device input, IPC `emit`, daemon lifecycle, and `emit` steps themselves)
//! reaches it through the one bus without the producers knowing about bindings. On each
//! topic event it looks up the matching bindings in `/bindings` (exact topic patterns
//! first, then `*` wildcards, array order within each group), evaluates each binding's
//! optional `when` ([`super::when`]) and runs its `steps` **sequentially in binding
//! order**.
//!
//! The closed step vocabulary (`schemas/binding.json`, validated at bind time by DS-041
//! and re-checked here):
//!
//! | step | behaviour |
//! |---|---|
//! | `set` | one state transaction through the [`StateHandle`] (the state task is the single writer), schema-validated against `schemas/state.json` on commit — one rev bump; a schema-invalid value is rejected with `E_SCHEMA` and **state is untouched** |
//! | `emit` | validates the topic, publishes it on the bus (subscribers see it) and **re-enters the router synchronously** with depth + 1; the bus copy of an emit-published event is skipped by the engine's own subscription (marker map), so every event is dispatched exactly once |
//! | `plugin` | stubbed until the plugin host lands (DS-050): typed `E_CAP` error, never crashes the router |
//! | `exec` | stubbed until the DS-043 policy gate lands: **fails closed** with a typed capability error (the IPC vocabulary has no `E_POLICY`; `E_CAP` is its stand-in) — no process is ever spawned; the argv-only shape (non-empty string array, absolute `argv[0]`) is re-enforced at runtime |
//! | `sleep` | bounded `ms` (1..=60000); the remaining steps continue on a **timer thread**, so the router/bus thread never blocks |
//!
//! Failure modes (event-router.md): a failing step publishes
//! `deck/system/binding-error` with `{binding, step, error:{code,message}, topic, depth}`
//! and the **remaining steps still run**. `emit`-triggered recursion is capped at
//! [`MAX_EMIT_DEPTH`] (10): a dispatch at depth 9 still runs its steps, but its `emit`
//! step is refused with a clear `binding-error` event (depth 10 is never entered).
//!
//! Latency (actions.md §5, R11): `set`/`emit` step time is instrumented
//! ([`record_step`]/[`step_metrics`], ring buffer of the last 1024 samples per kind for
//! p99) and every step over the 10 ms budget logs a `warn` line. All state access goes
//! through the bounded channel to the state task, so no state lock is ever held on the
//! router thread and lock hold times stay short.

use crate::lifecycle;
use crate::router::bus::{Bus, Event, FnSink};
use crate::router::{topics, Pattern, TopicPattern};
use crate::state::StateHandle;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_state::{Op, Transaction};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::when;

/// Maximum `emit`-triggered dispatch depth. A dispatch at depth 9 still runs (its `set`
/// steps execute); its `emit` steps are refused — depth 10 is never entered
/// (precedent: flow depth cap 10, hubdeck engine.py:1088-1112).
pub const MAX_EMIT_DEPTH: u32 = 10;

/// The `set`/`emit` step time budget (actions.md §5: p99 ≤ 10 ms). Steps slower than this
/// log a `warn` line so the budget is observable under load.
pub const STEP_BUDGET: Duration = Duration::from_millis(10);

/// Upper bound of a `sleep` step (mirrors `schemas/binding.json` `ms` maximum).
pub const MAX_SLEEP_MS: u64 = 60_000;

/// How many recent per-step samples are kept for the p99 computation.
const METRIC_SAMPLES: usize = 1024;

/// The topic of the binding-error system event (actions.md §1).
pub const BINDING_ERROR_TOPIC: &str = "deck/system/binding-error";

/// Outcome of one step execution.
#[derive(Debug)]
enum StepOutcome {
    /// Step done; continue with the next step.
    Continue,
    /// `sleep`: the remaining steps continue on a timer thread after `ms`.
    Sleep(u64),
}

/// The binding engine: bus subscription + step executor. Cloneable handle via `Arc`.
pub struct Engine {
    bus: Bus,
    state: StateHandle,
    /// Markers for events published by `emit` steps: those are dispatched synchronously
    /// at emit time, so the engine's own bus subscription must skip the queued copy.
    /// Key: `(topic, ts_ms, payload)` of the published event.
    emitted: Mutex<HashMap<String, u32>>,
}

impl Engine {
    /// Create the engine and subscribe it to every topic event on `bus` (pattern `*`).
    /// `state` is the handle to the state task (single writer) used for `when` path reads
    /// and `set` transactions.
    pub fn new(bus: Bus, state: StateHandle) -> Arc<Self> {
        let engine = Arc::new(Self {
            bus,
            state,
            emitted: Mutex::new(HashMap::new()),
        });
        // Catch-all topic subscription: only Event::Topic matches a Topic pattern, so
        // state-change events never trigger bindings.
        let pat = Pattern::Topic(TopicPattern::parse("*").expect("'*' is a valid pattern"));
        let sink_engine = Arc::clone(&engine);
        let sink = Arc::new(FnSink(Arc::new(move |msg: &Value| {
            sink_engine.on_bus_msg(msg);
        })));
        engine.bus.subscribe(pat, None, "/*".to_string(), sink);
        engine
    }

    /// Bus sink callback: dispatch one wire event, skipping copies of events that an
    /// `emit` step already dispatched synchronously.
    fn on_bus_msg(self: &Arc<Self>, msg: &Value) {
        let Some(topic) = msg.get("topic").and_then(Value::as_str) else {
            return;
        };
        let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
        let ts_ms = msg.get("ts_ms").and_then(Value::as_u64).unwrap_or(0);
        let key = wire_key(topic, ts_ms, &payload);
        {
            let mut markers = self.emitted.lock().unwrap();
            if let Some(count) = markers.get_mut(&key) {
                if *count > 0 {
                    *count -= 1;
                    if *count == 0 {
                        markers.remove(&key);
                    }
                    return; // already dispatched by the emit step
                }
            }
        }
        self.dispatch(topic, &payload, 0);
    }

    /// Dispatch one event to the matching bindings: exact topic patterns first, then
    /// wildcard patterns, array order within each group (actions.md §2). `depth` is the
    /// emit-recursion depth (0 for externally published events).
    pub fn dispatch(self: &Arc<Self>, topic: &str, payload: &Value, depth: u32) {
        let reply = match self.state.get("/bindings", None) {
            Ok(r) => r,
            Err(_) => return,
        };
        let Some(arr) = reply.value.as_array() else {
            return;
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
        // Stable sort: exact (false) before wildcard (true), array order preserved inside
        // each group.
        matched.sort_by_key(|(_, wildcard)| *wildcard);
        for (binding, _) in matched {
            if let Some(when) = binding.get("when") {
                let state = self.state.clone();
                let fires = when::eval_when(
                    when,
                    move |path| state.get(path, None).ok().map(|r| r.value),
                    payload,
                );
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
            self.run_steps(binding, steps, 0, depth, topic);
        }
    }

    /// Run `steps[start..]` of one binding sequentially. A failing step publishes a
    /// `binding-error` event and the remaining steps still run; a `sleep` step moves the
    /// continuation to a timer thread so this (bus) thread never blocks.
    fn run_steps(
        self: &Arc<Self>,
        binding: &Value,
        steps: &[Value],
        start: usize,
        depth: u32,
        trigger: &str,
    ) {
        let binding_id = binding
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let mut i = start;
        while i < steps.len() {
            let step = &steps[i];
            let kind = step.get("do").and_then(Value::as_str).unwrap_or("?");
            let t0 = Instant::now();
            let result = self.exec_step(step, depth, trigger);
            if kind == "set" || kind == "emit" {
                let dur = t0.elapsed();
                record_step(kind, dur);
                if dur > STEP_BUDGET {
                    lifecycle::log(
                        "warn",
                        "ds-daemon::router::steps",
                        &format!(
                            "step '{kind}' took {:.3} ms (> {} ms budget): binding {binding_id} step {i} trigger {trigger:?}",
                            dur.as_secs_f64() * 1000.0,
                            STEP_BUDGET.as_millis()
                        ),
                    );
                }
            }
            match result {
                Ok(StepOutcome::Continue) => i += 1,
                Ok(StepOutcome::Sleep(ms)) => {
                    let engine = Arc::clone(self);
                    let binding = binding.clone();
                    let steps = steps.to_vec();
                    let trigger = trigger.to_string();
                    let spawned = std::thread::Builder::new()
                        .name("ds-router-sleep".into())
                        .spawn(move || {
                            std::thread::sleep(Duration::from_millis(ms));
                            engine.run_steps(&binding, &steps, i + 1, depth, &trigger);
                        });
                    match spawned {
                        Ok(_) => return, // continuation runs on the timer thread
                        Err(e) => {
                            // Never crash the router: continue inline without the delay.
                            lifecycle::log(
                                "error",
                                "ds-daemon::router::steps",
                                &format!(
                                    "cannot spawn the sleep timer thread: {e}; continuing inline"
                                ),
                            );
                            i += 1;
                        }
                    }
                }
                Err(err) => {
                    self.publish_binding_error(binding, i, &err, trigger, depth);
                    i += 1; // remaining steps still run
                }
            }
        }
    }

    /// Execute one step. Never panics; every failure is a typed [`IpcError`].
    fn exec_step(
        self: &Arc<Self>,
        step: &Value,
        depth: u32,
        trigger: &str,
    ) -> Result<StepOutcome, IpcError> {
        let do_ = step.get("do").and_then(Value::as_str).unwrap_or("");
        match do_ {
            "set" => {
                let path = step
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| schema_err("set step needs a string 'path'"))?;
                let value = step
                    .get("value")
                    .cloned()
                    .ok_or_else(|| schema_err("set step needs a 'value'"))?;
                // One transaction through the state task: schema-validated against
                // schemas/state.json on commit, exactly one rev bump. A schema-invalid
                // value fails the whole commit (E_SCHEMA) and state is untouched.
                self.state
                    .set(Transaction::new(vec![Op::set(path, value)]))
                    .map(|_| StepOutcome::Continue)
            }
            "emit" => {
                let topic = step
                    .get("topic")
                    .and_then(Value::as_str)
                    .ok_or_else(|| schema_err("emit step needs a string 'topic'"))?;
                if let Err(msg) = topics::validate_topic(topic) {
                    return Err(IpcError::new(
                        ErrorCode::ESchema,
                        format!("emit step topic {topic:?} is invalid: {msg}"),
                    ));
                }
                let payload = step.get("payload").cloned().unwrap_or(Value::Null);
                if depth + 1 >= MAX_EMIT_DEPTH {
                    return Err(IpcError::new(
                        ErrorCode::ELimit,
                        format!(
                            "emit recursion cap {MAX_EMIT_DEPTH} reached: dispatch depth {depth} \
                             (trigger {trigger:?}) may not re-enter the router for {topic:?}"
                        ),
                    ));
                }
                // Publish for subscribers, mark so our own subscription skips the queued
                // copy, then re-enter the router synchronously with depth + 1.
                let ev = Event::topic(topic, payload.clone());
                let key = event_key(&ev);
                {
                    let mut markers = self.emitted.lock().unwrap();
                    *markers.entry(key).or_insert(0) += 1;
                }
                self.bus.publish(ev);
                self.dispatch(topic, &payload, depth + 1);
                Ok(StepOutcome::Continue)
            }
            "plugin" => {
                // Stub until the plugin host lands (DS-050): typed error, never a crash,
                // never an invocation.
                let id = step.get("id").and_then(Value::as_str).unwrap_or("");
                let action = step.get("action").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() || action.is_empty() {
                    return Err(schema_err(
                        "plugin step needs non-empty 'id' and 'action' (invoke by id.action)",
                    ));
                }
                Err(IpcError::new(
                    ErrorCode::ECap,
                    format!(
                        "plugin step '{id}.{action}' cannot run yet: the plugin host lands with \
                         DS-050 (stubbed E_CAP; the router is unaffected)"
                    ),
                ))
            }
            "exec" => {
                // Fail closed until the DS-043 policy gate lands. The argv-only shape is
                // re-enforced here (bind-time schema enforces it too): a non-empty array
                // of strings with an absolute argv[0] — never a shell, never relative.
                let argv_ok = step.get("argv").and_then(Value::as_array).is_some_and(|a| {
                    !a.is_empty()
                        && a.len() <= 64
                        && a.iter().all(Value::is_string)
                        && a[0].as_str().is_some_and(|s| s.starts_with('/'))
                });
                if !argv_ok {
                    return Err(IpcError::new(
                        ErrorCode::ESchema,
                        "exec step needs a non-empty 'argv' array of strings with an absolute \
                         argv[0] (argv-only, never through a shell)"
                            .to_string(),
                    ));
                }
                Err(IpcError::new(
                    ErrorCode::ECap,
                    "exec step fails closed until the DS-043 policy gate is wired (E_CAP stands \
                     in for E_POLICY: /policy/exec allow/deny is not enforced yet); no process \
                     is spawned"
                        .to_string(),
                ))
            }
            "sleep" => {
                let ms = step
                    .get("ms")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| schema_err("sleep step needs an integer 'ms'"))?;
                if !(1..=MAX_SLEEP_MS).contains(&ms) {
                    return Err(IpcError::new(
                        ErrorCode::ESchema,
                        format!("sleep step 'ms' must be 1..={MAX_SLEEP_MS}, got {ms}"),
                    ));
                }
                Ok(StepOutcome::Sleep(ms))
            }
            other => Err(IpcError::new(
                ErrorCode::ESchema,
                format!(
                    "unknown step 'do' {other:?} — the vocabulary is closed \
                     (set, emit, plugin, exec, sleep)"
                ),
            )),
        }
    }

    /// Publish `deck/system/binding-error` for a failed step (event-router.md failure
    /// modes): binding id, step index, the typed error, the trigger topic and depth.
    fn publish_binding_error(
        &self,
        binding: &Value,
        index: usize,
        err: &IpcError,
        trigger: &str,
        depth: u32,
    ) {
        let id = binding.get("id").and_then(Value::as_str).unwrap_or("?");
        let payload = json!({
            "binding": id,
            "step": index,
            "error": {"code": err.code.as_str(), "message": err.message},
            "topic": trigger,
            "depth": depth,
        });
        lifecycle::log(
            "warn",
            "ds-daemon::router::steps",
            &format!(
                "binding {id} step {index} failed with {}: {} (trigger {trigger:?}, depth {depth}) — \
                 remaining steps still run",
                err.code.as_str(),
                err.message
            ),
        );
        self.bus.publish(Event::topic(BINDING_ERROR_TOPIC, payload));
    }
}

fn schema_err(msg: impl Into<String>) -> IpcError {
    IpcError::new(ErrorCode::ESchema, msg)
}

/// Marker key of a published topic event (must match the wire shape the sink sees).
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

// ---- step metrics (actions.md §5: set/emit p99 <= 10 ms, observable) --------------

struct Metrics {
    kinds: Mutex<HashMap<String, VecDeque<u64>>>,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| Metrics {
        kinds: Mutex::new(HashMap::new()),
    })
}

/// Record one step execution time (nanoseconds ring buffer, last [`METRIC_SAMPLES`]).
pub fn record_step(kind: &str, dur: Duration) {
    let ns = u64::try_from(dur.as_nanos()).unwrap_or(u64::MAX);
    let mut kinds = metrics().kinds.lock().unwrap();
    let q = kinds.entry(kind.to_string()).or_default();
    if q.len() >= METRIC_SAMPLES {
        q.pop_front();
    }
    q.push_back(ns);
}

/// Snapshot of the recorded step timings: per kind `{samples, p99_ms, max_ms, total_ms}`.
/// Combined with the per-step `warn` log over [`STEP_BUDGET`], this makes the
/// `set`/`emit` p99 ≤ 10 ms budget observable under load.
pub fn step_metrics() -> Value {
    let kinds = metrics().kinds.lock().unwrap();
    let mut out = serde_json::Map::new();
    for (kind, q) in kinds.iter() {
        let mut sorted: Vec<u64> = q.iter().copied().collect();
        sorted.sort_unstable();
        let n = sorted.len();
        let p99_ns = if n == 0 {
            0
        } else {
            let idx = ((n as f64 * 0.99).ceil() as usize).clamp(1, n) - 1;
            sorted[idx]
        };
        let max_ns = sorted.last().copied().unwrap_or(0);
        let total_ns: u64 = sorted.iter().sum();
        out.insert(
            kind.to_string(),
            json!({
                "samples": n,
                "p99_ms": p99_ns as f64 / 1e6,
                "max_ms": max_ns as f64 / 1e6,
                "total_ms": total_ns as f64 / 1e6,
            }),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::Bus;
    use crate::state::StateTask;
    use ds_state::Store;
    use serde_json::json;
    use std::sync::mpsc::Receiver;
    use std::thread::JoinHandle;

    fn test_engine() -> (Arc<Engine>, StateHandle, JoinHandle<()>, Bus) {
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

    /// Subscribe a collecting sink to a wire pattern; returns the receiver.
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

    #[test]
    fn set_step_is_one_schema_validated_transaction_with_one_rev_bump() {
        let (engine, state, t, _bus) = test_engine();
        bind(
            &state,
            "deck/control/keys.0/down",
            json!([{"do": "set", "path": "/controls/keys/0/image",
                    "value": {"kind": "color", "value": "#00ff00"}}]),
            None,
        );
        assert_eq!(state.rev().unwrap(), 1);
        engine.dispatch("deck/control/keys.0/down", &json!({"key": 0}), 0);
        assert_eq!(
            get(&state, "/controls/keys/0/image"),
            json!({"kind": "color", "value": "#00ff00"})
        );
        assert_eq!(
            state.rev().unwrap(),
            2,
            "one set step = exactly one rev bump"
        );
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn steps_run_sequentially_in_binding_order() {
        let (engine, state, t, bus) = test_engine();
        let events = collect(&bus, "/controls/*");
        bind(
            &state,
            "deck/control/keys.1/down",
            json!([
                {"do": "set", "path": "/controls/keys/1/label", "value": "first"},
                {"do": "set", "path": "/controls/keys/1/label", "value": "second"},
                {"do": "set", "path": "/controls/keys/1/label", "value": "third"},
            ]),
            None,
        );
        engine.dispatch("deck/control/keys.1/down", &json!({}), 0);
        assert_eq!(get(&state, "/controls/keys/1/label"), json!("third"));
        assert_eq!(state.rev().unwrap(), 4, "bind + 3 set steps");
        // the state events arrive in step order (one per committed change)
        let msgs = recv_all(&events, Duration::from_secs(2));
        let values: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["path"] == "/controls/keys/1/label")
            .map(|m| &m["value"])
            .collect();
        assert_eq!(
            values,
            vec![&json!("first"), &json!("second"), &json!("third")]
        );
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn emit_reenters_router_and_publishes_on_the_bus_exactly_once() {
        let (engine, state, t, bus) = test_engine();
        let app_events = collect(&bus, "/app/*");
        bind(
            &state,
            "app/demo/x",
            json!([{"do": "emit", "topic": "app/demo/y", "payload": {"n": 1}}]),
            None,
        );
        bind(
            &state,
            "app/demo/y",
            json!([{"do": "set", "path": "/controls/keys/1/label", "value": "hit"}]),
            None,
        );
        let rev_after_binds = state.rev().unwrap();
        engine.dispatch("app/demo/x", &json!({}), 0);
        // the emit step re-entered the router synchronously: the app/y binding ran
        assert_eq!(get(&state, "/controls/keys/1/label"), json!("hit"));
        assert_eq!(state.rev().unwrap(), rev_after_binds + 1);
        // subscribers saw the emitted topic event with its payload
        let msgs = recv_all(&app_events, Duration::from_secs(2));
        let emitted: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == "app/demo/y")
            .map(|m| &m["payload"])
            .collect();
        assert_eq!(emitted, vec![&json!({"n": 1})]);
        // the engine's own bus copy of app/y was marker-skipped: no second dispatch
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            state.rev().unwrap(),
            rev_after_binds + 1,
            "the queued copy of an emit-published event must not dispatch twice"
        );
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn emit_recursion_is_capped_at_depth_10_with_a_clear_error_event() {
        let (engine, state, t, bus) = test_engine();
        let errors = collect(&bus, "/deck/system/*");
        bind(
            &state,
            "app/demo/loop",
            json!([{"do": "emit", "topic": "app/demo/loop", "payload": {}}]),
            None,
        );
        engine.dispatch("app/demo/loop", &json!({}), 0);
        // the chain ran to depth 9 and the emit at depth 9 was refused: exactly one
        // binding-error event, on the cap, with binding id + step index
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let cap_errors: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(cap_errors.len(), 1, "exactly one cap error, got {msgs:?}");
        let payload = &cap_errors[0]["payload"];
        assert_eq!(payload["binding"], json!("b0001"));
        assert_eq!(payload["step"], json!(0));
        assert_eq!(payload["depth"], json!(9));
        assert_eq!(payload["error"]["code"], json!("E_LIMIT"));
        assert!(payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cap 10"));
        // the daemon stays healthy: the engine still dispatches afterwards
        bind(
            &state,
            "app/demo/after",
            json!([{"do": "set", "path": "/controls/keys/2/label", "value": "alive"}]),
            None,
        );
        engine.dispatch("app/demo/after", &json!({}), 0);
        assert_eq!(get(&state, "/controls/keys/2/label"), json!("alive"));
        // the error event is a valid wire message
        for m in &msgs {
            ds_ipc::message::validate_server_message(m).unwrap();
        }
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn depth_9_dispatch_still_runs_steps_but_its_emit_is_blocked() {
        let (engine, state, t, bus) = test_engine();
        let errors = collect(&bus, "/deck/system/*");
        // self-emitting binding: every dispatch level re-emits its own topic
        bind(
            &state,
            "app/demo/d9",
            json!([
                {"do": "set", "path": "/controls/keys/3/label", "value": "depth9"},
                {"do": "emit", "topic": "app/demo/d9", "payload": {}},
            ]),
            None,
        );
        // depth 9 is OK: the set step runs ...
        engine.dispatch("app/demo/d9", &json!({}), 9);
        assert_eq!(get(&state, "/controls/keys/3/label"), json!("depth9"));
        // ... but the emit would enter depth 10 and is blocked with a cap error
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let cap: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(cap.len(), 1, "{msgs:?}");
        assert_eq!(cap[0]["payload"]["depth"], json!(9));
        assert!(cap[0]["payload"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cap 10"));
        // at depth 8 the emit is allowed and re-enters at depth 9, where it is blocked:
        // still exactly one cap error for this dispatch
        engine.dispatch("app/demo/d9", &json!({}), 8);
        let msgs = recv_all(&errors, Duration::from_secs(2));
        assert_eq!(
            msgs.iter()
                .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
                .count(),
            1
        );
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn failing_step_emits_binding_error_and_remaining_steps_still_run() {
        let (engine, state, t, bus) = test_engine();
        let errors = collect(&bus, "/deck/system/*");
        let id = bind(
            &state,
            "deck/control/keys.4/down",
            json!([
                {"do": "set", "path": "/controls/keys/4/label", "value": "before"},
                {"do": "plugin", "id": "demo", "action": "bump", "args": {"by": 1}},
                {"do": "set", "path": "/controls/keys/4/image",
                 "value": {"kind": "color", "value": "#ff8800"}},
            ]),
            None,
        );
        let rev_before = state.rev().unwrap();
        engine.dispatch("deck/control/keys.4/down", &json!({}), 0);
        // both set steps ran, in order
        assert_eq!(get(&state, "/controls/keys/4/label"), json!("before"));
        assert_eq!(
            get(&state, "/controls/keys/4/image"),
            json!({"kind": "color", "value": "#ff8800"})
        );
        assert_eq!(state.rev().unwrap(), rev_before + 2);
        // one binding-error event for the plugin step (index 1), E_CAP stub
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let errs: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(errs.len(), 1, "{msgs:?}");
        assert_eq!(errs[0]["payload"]["binding"], json!(id));
        assert_eq!(errs[0]["payload"]["step"], json!(1));
        assert_eq!(errs[0]["payload"]["error"]["code"], json!("E_CAP"));
        assert!(errs[0]["payload"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DS-050"));
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn schema_invalid_set_value_is_rejected_without_touching_state() {
        let (engine, state, t, bus) = test_engine();
        let errors = collect(&bus, "/deck/system/*");
        bind(
            &state,
            "deck/control/keys.5/down",
            // brightness must be an integer 1..=100 per schemas/state.json
            json!([{"do": "set", "path": "/controls/keys/5/brightness", "value": "high"}]),
            None,
        );
        let rev_before = state.rev().unwrap();
        engine.dispatch("deck/control/keys.5/down", &json!({}), 0);
        assert_eq!(
            state.rev().unwrap(),
            rev_before,
            "a rejected set must not bump rev"
        );
        let err = state.get("/controls/keys/5/brightness", None).unwrap_err();
        assert_eq!(err.code, ErrorCode::EPath, "state was untouched");
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let errs: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0]["payload"]["error"]["code"], json!("E_SCHEMA"));
        assert_eq!(errs[0]["payload"]["step"], json!(0));
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn exec_fails_closed_and_enforces_the_argv_only_shape() {
        let (engine, state, t, bus) = test_engine();
        let errors = collect(&bus, "/deck/system/*");
        // the schema rejects a relative argv[0] at bind time, so only the absolute-argv
        // step is storable; the relative case is re-checked at runtime (below)
        bind(
            &state,
            "app/demo/exec",
            json!([{"do": "exec", "argv": ["/bin/sh", "-c", "echo hi"]}]),
            None,
        );
        engine.dispatch("app/demo/exec", &json!({}), 0);
        assert_eq!(state.rev().unwrap(), 1, "no exec step touches state");
        let msgs = recv_all(&errors, Duration::from_secs(2));
        let errs: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["topic"] == BINDING_ERROR_TOPIC)
            .collect();
        assert_eq!(errs.len(), 1, "{msgs:?}");
        // absolute argv[0] passes the shape check, then fails closed (DS-043 stub)
        assert_eq!(errs[0]["payload"]["step"], json!(0));
        assert_eq!(errs[0]["payload"]["error"]["code"], json!("E_CAP"));
        assert!(errs[0]["payload"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DS-043"));
        // runtime re-check (a hand-crafted document could carry it): relative argv[0] and
        // non-array argv violate the argv-only shape -> E_SCHEMA, never a spawn
        let err = engine
            .exec_step(
                &json!({"do": "exec", "argv": ["sh", "-c", "x"]}),
                0,
                "app/demo/exec",
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        let err = engine
            .exec_step(
                &json!({"do": "exec", "argv": "sh -c x"}),
                0,
                "app/demo/exec",
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        let err = engine
            .exec_step(&json!({"do": "exec", "argv": []}), 0, "app/demo/exec")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn sleep_step_never_blocks_the_router_thread() {
        let (engine, state, t, _bus) = test_engine();
        bind(
            &state,
            "app/demo/sleepy",
            json!([
                {"do": "sleep", "ms": 120},
                {"do": "set", "path": "/controls/keys/6/label", "value": "after-sleep"},
            ]),
            None,
        );
        let t0 = Instant::now();
        engine.dispatch("app/demo/sleepy", &json!({}), 0);
        let dispatch_dur = t0.elapsed();
        assert!(
            dispatch_dur < Duration::from_millis(100),
            "dispatch must return without waiting for the sleep (took {dispatch_dur:?})"
        );
        // the set step has not run yet (the timer thread is still sleeping)
        assert!(state.get("/controls/keys/6/label", None).is_err());
        // ... and runs after the bounded delay, on the timer thread
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(r) = state.get("/controls/keys/6/label", None) {
                assert_eq!(r.value, json!("after-sleep"));
                break;
            }
            assert!(Instant::now() < deadline, "the post-sleep step never ran");
            std::thread::sleep(Duration::from_millis(10));
        }
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn when_gates_dispatch_on_payload_and_on_state_path() {
        let (engine, state, t, _bus) = test_engine();
        bind(
            &state,
            "app/focus/changed",
            json!([{"do": "set", "path": "/controls/keys/7/label", "value": "fired"}]),
            Some(json!({"payload": "app", "op": "eq", "value": "code"})),
        );
        bind(
            &state,
            "app/demo/level",
            json!([{"do": "set", "path": "/controls/keys/8/label", "value": "level-fired"}]),
            Some(json!({"path": "/focus/app", "op": "eq", "value": "code"})),
        );
        // payload mismatch -> does not fire
        engine.dispatch("app/focus/changed", &json!({"app": "terminal"}), 0);
        assert!(state.get("/controls/keys/7/label", None).is_err());
        // payload match -> fires
        engine.dispatch("app/focus/changed", &json!({"app": "code"}), 0);
        assert_eq!(get(&state, "/controls/keys/7/label"), json!("fired"));
        // state-path `when` with a missing path -> exists:false -> does not fire
        engine.dispatch("app/demo/level", &json!({}), 0);
        assert!(state.get("/controls/keys/8/label", None).is_err());
        // once /focus/app is set to "code", the path-based `when` fires
        state
            .set(Transaction::new(vec![Op::set("/focus/app", json!("code"))]))
            .unwrap();
        engine.dispatch("app/demo/level", &json!({}), 0);
        assert_eq!(get(&state, "/controls/keys/8/label"), json!("level-fired"));
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn exact_bindings_run_before_wildcard_bindings() {
        let (engine, state, t, bus) = test_engine();
        let events = collect(&bus, "/controls/*");
        // wildcard binding first in the array: it must still run AFTER the exact one
        bind(
            &state,
            "deck/control/keys/*",
            json!([{"do": "set", "path": "/controls/keys/9/label", "value": "wildcard"}]),
            None,
        );
        bind(
            &state,
            "deck/control/keys.9/down",
            json!([{"do": "set", "path": "/controls/keys/9/label", "value": "exact"}]),
            None,
        );
        engine.dispatch("deck/control/keys.9/down", &json!({}), 0);
        assert_eq!(
            get(&state, "/controls/keys/9/label"),
            json!("wildcard"),
            "the wildcard binding ran last, so its value wins"
        );
        let msgs = recv_all(&events, Duration::from_secs(2));
        let values: Vec<&Value> = msgs
            .iter()
            .filter(|m| m["path"] == "/controls/keys/9/label")
            .map(|m| &m["value"])
            .collect();
        assert_eq!(values, vec![&json!("exact"), &json!("wildcard")]);
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn unknown_step_shapes_fail_with_typed_errors_never_panics() {
        let (engine, _state, t, _bus) = test_engine();
        // unknown `do` (unrepresentable via bind — the schema rejects it — but the
        // runtime re-checks): E_SCHEMA
        let err = engine
            .exec_step(&json!({"do": "eval", "code": "1"}), 0, "app/demo/x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // sleep out of bounds
        let err = engine
            .exec_step(&json!({"do": "sleep", "ms": 0}), 0, "app/demo/x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        let err = engine
            .exec_step(&json!({"do": "sleep", "ms": 999_999}), 0, "app/demo/x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // emit with an invalid topic grammar
        let err = engine
            .exec_step(
                &json!({"do": "emit", "topic": "app/onlysender"}),
                0,
                "app/demo/x",
            )
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // set without a value
        let err = engine
            .exec_step(&json!({"do": "set", "path": "/x"}), 0, "app/demo/x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // plugin without id/action
        let err = engine
            .exec_step(&json!({"do": "plugin"}), 0, "app/demo/x")
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        engine.state.stop();
        t.join().unwrap();
    }

    #[test]
    fn set_and_emit_step_times_are_instrumented() {
        let (engine, state, t, _bus) = test_engine();
        bind(
            &state,
            "app/demo/metric",
            json!([
                {"do": "set", "path": "/controls/keys/10/label", "value": "m"},
                {"do": "emit", "topic": "app/demo/metric2", "payload": {}},
            ]),
            None,
        );
        engine.dispatch("app/demo/metric", &json!({}), 0);
        let m = step_metrics();
        for kind in ["set", "emit"] {
            let k = &m[kind];
            assert!(k["samples"].as_u64().unwrap() >= 1, "{kind}: {m}");
            assert!(k["p99_ms"].as_f64().unwrap() >= 0.0);
            assert!(k["max_ms"].as_f64().unwrap() <= k["total_ms"].as_f64().unwrap() + 1e-9);
        }
        engine.state.stop();
        t.join().unwrap();
    }
}
