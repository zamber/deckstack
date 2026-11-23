//! The focus event model: payload shape, the exact `emit` request, the settle/dedupe
//! logic, and the `--inject` payload parsing. Pure and unit-tested; the backends only
//! produce [`Focus`] values.

use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

/// The topic ds-focus emits on (`docs/contracts/actions.md` §1, `app/<sender>/<topic>`).
pub const TOPIC: &str = "app/focus/changed";

/// One observed focused window. `pid` is included in the payload only when the backend
/// knows it (KWin's queryWindowInfo does; wlr-foreign-toplevel does not).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Focus {
    pub app: String,
    pub title: String,
    pub pid: Option<u64>,
}

impl Focus {
    /// The `app/focus/changed` payload: `{"app":…, "title":…, "pid":…?}` — the shape
    /// `schemas/state.json` `/focus` mirrors and `docs/contracts/actions.md` §6 chain B
    /// documents (`pid` optional).
    pub fn payload(&self) -> Value {
        let mut p = json!({"app": self.app, "title": self.title});
        if let Some(pid) = self.pid {
            p["pid"] = json!(pid);
        }
        p
    }
}

/// The exact IPC request ds-focus sends for one focus change (the client adds `id`).
/// Unit-tested against `schemas/ipc-message.json` via `ds_ipc::message::validate_request`.
pub fn emit_request(payload: &Value) -> Value {
    json!({"op": "emit", "topic": TOPIC, "payload": payload})
}

/// Coalesces focus churn: a candidate focus must stay unchanged for `settle` before it is
/// emitted; identical consecutive payloads are deduped (emit-storm guard).
///
/// Timing: a live backend is polled every `interval`; a change first observed at `t` is
/// emitted at `t + settle` (the loop wakes exactly at the settle deadline), so the emit
/// lands within the 500 ms latency budget measured from the settled change.
#[derive(Debug)]
pub struct Settler {
    settle: Duration,
    pending: Option<(Focus, Instant)>,
    last_emitted: Option<Focus>,
}

impl Settler {
    pub fn new(settle: Duration) -> Self {
        Self {
            settle,
            pending: None,
            last_emitted: None,
        }
    }

    /// Record one backend observation. A changed focus resets the stability timer.
    pub fn observe(&mut self, f: Focus, now: Instant) {
        match &self.pending {
            Some((p, _)) if *p == f => {} // still the same candidate: keep the timer
            _ => self.pending = Some((f, now)),
        }
    }

    /// The pending focus if it has now been stable for >= `settle` (and differs from the
    /// last emitted one). Takes the pending slot.
    pub fn settled(&mut self, now: Instant) -> Option<Focus> {
        let (_, since) = self.pending.as_ref()?;
        if now.saturating_duration_since(*since) < self.settle {
            return None;
        }
        let (f, _) = self.pending.take().expect("pending checked above");
        self.emit_if_new(f)
    }

    /// Immediate path for `--once` / `--inject` (already settled by definition):
    /// dedupe only, no timer.
    pub fn immediate(&mut self, f: Focus) -> Option<Focus> {
        self.emit_if_new(f)
    }

    fn emit_if_new(&mut self, f: Focus) -> Option<Focus> {
        if self.last_emitted.as_ref() == Some(&f) {
            return None;
        }
        self.last_emitted = Some(f.clone());
        Some(f)
    }

    /// When the main loop must wake next: the settle deadline if a candidate is pending,
    /// else the next poll tick.
    pub fn next_wake(&self, now: Instant, interval: Duration) -> Instant {
        match &self.pending {
            Some((_, since)) => {
                let deadline = *since + self.settle;
                let tick = now + interval;
                if deadline < tick {
                    deadline
                } else {
                    tick
                }
            }
            None => now + interval,
        }
    }
}

/// Parse the `--inject` argument into the payloads to emit, in order:
///
/// - a path to an existing file → one JSON object per non-empty line (ndjson);
/// - otherwise the argument itself must parse as JSON: one object, or an array of
///   objects.
///
/// Every payload must be a JSON object (the focus payload shape); it is emitted verbatim.
pub fn parse_inject(arg: &str) -> Result<Vec<Value>, String> {
    let path = Path::new(arg);
    if path.is_file() {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read inject file {}: {e}", path.display()))?;
        let mut out = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(line)
                .map_err(|e| format!("{}: line {}: not JSON: {e}", path.display(), n + 1))?;
            if !v.is_object() {
                return Err(format!(
                    "{}: line {}: payload must be a JSON object, got {}",
                    path.display(),
                    n + 1,
                    type_name(&v)
                ));
            }
            out.push(v);
        }
        if out.is_empty() {
            return Err(format!("{}: no payload lines", path.display()));
        }
        return Ok(out);
    }
    let v: Value = serde_json::from_str(arg)
        .map_err(|e| format!("--inject is neither an existing file nor JSON: {e}"))?;
    let items: Vec<Value> = match v {
        Value::Array(a) => a,
        other => vec![other],
    };
    for it in &items {
        if !it.is_object() {
            return Err(format!(
                "--inject payload must be a JSON object (or an array of objects), got {}",
                type_name(it)
            ));
        }
    }
    if items.is_empty() {
        return Err("--inject array is empty".to_string());
    }
    Ok(items)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn f(app: &str, title: &str) -> Focus {
        Focus {
            app: app.to_string(),
            title: title.to_string(),
            pid: None,
        }
    }

    #[test]
    fn payload_shape_matches_the_contract() {
        assert_eq!(
            f("code", "PLAN.md").payload(),
            json!({"app": "code", "title": "PLAN.md"})
        );
        let with_pid = Focus {
            app: "code".into(),
            title: "x".into(),
            pid: Some(4242),
        };
        assert_eq!(
            with_pid.payload(),
            json!({"app": "code", "title": "x", "pid": 4242})
        );
        // the emit request is schema-valid per schemas/ipc-message.json (ds-ipc embeds it)
        let req = emit_request(&f("code", "x").payload());
        ds_ipc::message::validate_request(&req).expect("emit request validates");
        let mut with_id = req.clone();
        with_id["id"] = json!(7);
        ds_ipc::message::validate_request(&with_id).expect("with correlation id validates");
        assert_eq!(req["op"], "emit");
        assert_eq!(req["topic"], "app/focus/changed");
        // a topic outside app/ or deck/ must NOT validate (schema pattern ^(deck|app)/)
        let bad = json!({"op": "emit", "topic": "other/x", "payload": {}});
        assert!(ds_ipc::message::validate_request(&bad).is_err());
    }

    #[test]
    fn settle_coalesces_churn_and_emits_once_stable() {
        let settle = Duration::from_millis(300);
        let mut s = Settler::new(settle);
        let t0 = Instant::now();
        // churn: A, B, A within the settle window
        s.observe(f("a", "1"), t0);
        s.observe(f("b", "2"), t0 + Duration::from_millis(50));
        s.observe(f("a", "1"), t0 + Duration::from_millis(100));
        assert!(s.settled(t0 + Duration::from_millis(399)).is_none());
        // stable since t0+100ms: settles at t0+400ms
        assert_eq!(
            s.settled(t0 + Duration::from_millis(400)),
            Some(f("a", "1"))
        );
        // already emitted: nothing more
        assert!(s.settled(t0 + Duration::from_millis(900)).is_none());
    }

    #[test]
    fn dedupe_identical_consecutive_payloads() {
        let mut s = Settler::new(Duration::from_millis(300));
        let t0 = Instant::now();
        s.observe(f("a", "1"), t0);
        assert_eq!(
            s.settled(t0 + Duration::from_millis(300)),
            Some(f("a", "1"))
        );
        // the same focus observed again later must not re-emit
        s.observe(f("a", "1"), t0 + Duration::from_millis(1000));
        assert!(s.settled(t0 + Duration::from_millis(1400)).is_none());
        // but a different one does (A -> B -> A re-emits A)
        s.observe(f("b", "2"), t0 + Duration::from_millis(1500));
        assert_eq!(
            s.settled(t0 + Duration::from_millis(1800)),
            Some(f("b", "2"))
        );
        s.observe(f("a", "1"), t0 + Duration::from_millis(1900));
        assert_eq!(
            s.settled(t0 + Duration::from_millis(2200)),
            Some(f("a", "1"))
        );
    }

    #[test]
    fn immediate_path_dedupes_without_timer() {
        let mut s = Settler::new(Duration::from_secs(60));
        assert_eq!(s.immediate(f("code", "x")), Some(f("code", "x")));
        assert_eq!(
            s.immediate(f("code", "x")),
            None,
            "identical payload deduped"
        );
        assert_eq!(s.immediate(f("term", "y")), Some(f("term", "y")));
        // pid participates in identity
        let p = Focus {
            app: "code".into(),
            title: "x".into(),
            pid: Some(1),
        };
        assert_eq!(
            s.immediate(p),
            Some(Focus {
                app: "code".into(),
                title: "x".into(),
                pid: Some(1)
            })
        );
    }

    #[test]
    fn next_wake_hits_the_settle_deadline_not_the_tick() {
        let interval = Duration::from_millis(250);
        let settle = Duration::from_millis(300);
        let mut s = Settler::new(settle);
        let t0 = Instant::now();
        assert_eq!(s.next_wake(t0, interval), t0 + interval, "no pending: tick");
        // candidate first observed at t0+10ms -> settle deadline t0+310ms
        s.observe(f("a", "1"), t0 + Duration::from_millis(10));
        let now = t0 + Duration::from_millis(10);
        // tick (t0+260) is still earlier than the deadline (t0+310): poll first
        assert_eq!(s.next_wake(now, interval), now + interval);
        // from the tick at t0+260 the deadline t0+310 wins over the next tick t0+510
        let at_tick = t0 + Duration::from_millis(260);
        assert_eq!(
            s.next_wake(at_tick, interval),
            t0 + Duration::from_millis(310)
        );
        // late in the window the deadline is what wakes the loop
        let later = t0 + Duration::from_millis(200);
        assert_eq!(
            s.next_wake(later, interval),
            t0 + Duration::from_millis(310)
        );
    }

    #[test]
    fn inject_inline_object_array_and_errors() {
        assert_eq!(
            parse_inject("{\"app\":\"code\",\"title\":\"x\"}").unwrap(),
            vec![json!({"app": "code", "title": "x"})]
        );
        assert_eq!(
            parse_inject("[{\"app\":\"a\"},{\"app\":\"b\",\"pid\":3}]").unwrap(),
            vec![json!({"app": "a"}), json!({"app": "b", "pid": 3})]
        );
        // verbatim pass-through: extra keys survive
        assert_eq!(
            parse_inject("{\"app\":\"code\",\"title\":\"x\",\"pid\":9,\"extra\":true}").unwrap(),
            vec![json!({"app": "code", "title": "x", "pid": 9, "extra": true})]
        );
        assert!(parse_inject("42").is_err(), "non-object rejected");
        assert!(parse_inject("[1,2]").is_err());
        assert!(parse_inject("[]").is_err());
        assert!(parse_inject("not json at all").is_err());
        assert!(parse_inject("{\"app\":\"a\",").is_err());
    }

    #[test]
    fn inject_ndjson_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("events.ndjson");
        std::fs::write(
            &p,
            "{\"app\":\"code\",\"title\":\"x\"}\n\n{\"app\":\"term\",\"title\":\"y\"}\n",
        )
        .unwrap();
        let got = parse_inject(p.to_str().unwrap()).unwrap();
        assert_eq!(
            got,
            vec![
                json!({"app": "code", "title": "x"}),
                json!({"app": "term", "title": "y"})
            ]
        );
        // bad line
        std::fs::write(&p, "{\"app\":\"a\"}\nnope\n").unwrap();
        let e = parse_inject(p.to_str().unwrap()).unwrap_err();
        assert!(e.contains("line 2"), "{e}");
        // empty file
        std::fs::write(&p, "\n\n").unwrap();
        assert!(parse_inject(p.to_str().unwrap())
            .unwrap_err()
            .contains("no payload"));
        // missing file falls through to the JSON parse error
        let e = parse_inject("/definitely/not/here.ndjson").unwrap_err();
        assert!(e.contains("neither an existing file nor JSON"), "{e}");
    }
}
