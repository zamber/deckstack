//! Exec policy — ticket DS-043 ("enforced, not asserted", S3; contract
//! `docs/contracts/actions.md` §3-4, threat T5 in `docs/20-security.md`).
//!
//! An `exec` step is **fire-and-forget** (S11) and can only ever run an explicitly
//! allowlisted absolute binary, argv-only, never through a shell:
//!
//! 1. **Shape** (schema level at bind time via `prefixItems: ^/`, re-enforced here):
//!    non-empty `argv` array of strings (≤ [`MAX_ARGV`]), `argv[0]` absolute — a
//!    relative `argv[0]` is `E_SCHEMA`. There is no shell string anywhere: the step
//!    shape has no code field, and nothing here ever spawns `/bin/sh`.
//! 2. **Deny wins** (S3): `argv[0]` must not be in `/policy/exec/deny` **or** in the
//!    built-in [`DEFAULT_DENY`] list of shells/interpreters/wrappers — so
//!    `["/bin/sh","-c","…"]` is refused even if an admin added `/bin/sh` to the allow
//!    list. Refusal: `E_CAP`.
//! 3. **Allow**: `argv[0]` must be in `/policy/exec/allow` (admin-managed absolute
//!    paths). Otherwise `E_CAP`. There is no `PATH` lookup — absolute paths only.
//! 4. **cwd confinement** (M2): the step's `cwd` (default `/tmp`) must canonicalize
//!    under one of `/policy/fs/roots` or `/tmp`. Otherwise refused (`E_CAP`;
//!    `E_PATH` when it does not exist, `E_SCHEMA` when relative).
//! 5. **Authorization**: the caller must have admin scope **and** the binding must
//!    carry `allow_exec: true` (`schemas/binding.json`). A non-admin trigger gets
//!    `E_SCOPE`; a missing `allow_exec` grant gets `E_CAP`.
//! 6. **Spawn** ([`spawn`]): argv-only `execve` of `argv[0]` with the exact `argv`, an
//!    explicit cwd, a scrubbed environment (`PATH`, `HOME`, `LANG` — never the token),
//!    rlimits (CPU 10 s → SIGKILL, NPROC 64, NOFILE 64, AS 1 GiB), its own process
//!    group, and a reaper thread so the child never becomes a zombie. The router never
//!    blocks on the child (fire-and-forget, S11).
//!
//! **Audit**: every attempt — allowed and refused — is logged and published as a
//! `deck/system/exec` event `{argv, cwd, decision, reason, pid | code, binding,
//! trigger, rev}` (actions.md §4), and kept in the in-memory ring buffer
//! [`audit_records`] (last [`AUDIT_RING_CAP`] records).
//!
//! The router seam is [`check_exec`], called from `router/steps.rs`; the exec-child
//! entry point [`spawn::exec_child_main`] is intercepted in `lib.rs`'s `daemon` facade.

use crate::lifecycle;
use crate::router::bus::{Bus, Event};
use crate::state::StateHandle;
use ds_ipc::error::{ErrorCode, IpcError};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub mod spawn;

/// Topic of the exec audit event (actions.md §1: `deck/system/<event>` includes `exec`).
pub const EXEC_AUDIT_TOPIC: &str = "deck/system/exec";

/// Maximum number of `argv` elements (mirrors `schemas/binding.json`).
pub const MAX_ARGV: usize = 64;

/// cwd used when the step carries none — always allowed.
pub const DEFAULT_CWD: &str = "/tmp";

/// cwd roots that are always allowed in addition to `/policy/fs/roots`.
pub const ALWAYS_CWD_ROOTS: &[&str] = &["/tmp"];

/// Rlimits applied to every exec child (actions.md §4): cpu 10 s (SIGKILL at the hard
/// limit), nproc 64, nofile 64, address space 1 GiB.
pub const RLIMIT_CPU_SECS: u64 = 10;
pub const RLIMIT_NPROC: u64 = 64;
pub const RLIMIT_NOFILE: u64 = 64;
pub const RLIMIT_AS_BYTES: u64 = 1 << 30;

/// Shells, interpreters and wrappers are **always** denied (S3: "no shell" is a deny
/// list, not an assertion), independent of what `/policy/exec/deny` contains — the
/// allowlist is the primary control, this list is defense in depth.
pub const DEFAULT_DENY: &[&str] = &[
    "/bin/sh",
    "/bin/bash",
    "/bin/dash",
    "/bin/zsh",
    "/bin/ksh",
    "/bin/ash",
    "/bin/busybox",
    "/usr/bin/env",
    "/usr/bin/python",
    "/usr/bin/python3",
    "/usr/bin/perl",
    "/usr/bin/ruby",
    "/usr/bin/php",
    "/usr/bin/node",
    "/usr/bin/awk",
    "/usr/bin/gawk",
    "/usr/bin/sed",
    "/usr/bin/xargs",
    "/usr/bin/find",
    "/usr/bin/make",
    "/usr/bin/nc",
    "/usr/bin/curl",
    "/usr/bin/wget",
];

/// Capacity of the in-memory audit ring buffer.
pub const AUDIT_RING_CAP: usize = 256;

/// Everything [`check_exec`] needs besides the step itself.
pub struct ExecContext<'a> {
    /// State task handle (reads `/policy/exec/*`, `/policy/fs/roots`, rev).
    pub state: &'a StateHandle,
    /// Bus the audit event is published on.
    pub bus: &'a Bus,
    /// The binding the step belongs to (id + `allow_exec` grant).
    pub binding: &'a Value,
    /// Trigger topic of the dispatch (used for the admin-scope seam, see
    /// [`caller_is_admin`]).
    pub trigger: &'a str,
    /// Whether the triggering caller has admin scope. The bus does not propagate the
    /// emitting client's token scope (that seam is `ipc.rs`/`server.rs`), so the router
    /// derives it from the trigger namespace: daemon-internal events (`deck/*`,
    /// `daemon/*`) are admin; client-origin `app/*`/`plugin/*` events fail closed as
    /// non-admin.
    pub caller_admin: bool,
}

/// Is a trigger topic daemon-internal (admin) or client-origin (non-admin)?
/// `deck/*` and `daemon/*` originate inside the daemon (device input, lifecycle);
/// `app/*` and `plugin/*` come from IPC clients and are treated as non-admin.
pub fn caller_is_admin(trigger: &str) -> bool {
    !trigger.starts_with("app/") && !trigger.starts_with("plugin/")
}

/// Check one `exec` step against the policy and, if allowed, spawn it
/// fire-and-forget. Returns the child pid on success; every refusal is a typed
/// [`IpcError`] (`E_SCHEMA` shape, `E_CAP` policy/allowlist/cwd/grant, `E_SCOPE`
/// non-admin caller) **and** an audit record + `deck/system/exec` event either way.
pub fn check_exec(ctx: &ExecContext<'_>, step: &Value) -> Result<u32, IpcError> {
    // Best-effort raw view for the audit record, even of a malformed step.
    let raw_argv: Vec<String> = step
        .get("argv")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let raw_cwd = step.get("cwd").and_then(Value::as_str).map(str::to_string);

    let argv = match parse_argv(step) {
        Ok(a) => a,
        Err(e) => return refuse(ctx, &raw_argv, raw_cwd.as_deref(), e.code, e.message),
    };
    let cwd = match confine_cwd(ctx.state, step.get("cwd").and_then(Value::as_str)) {
        Ok(c) => c,
        Err(e) => return refuse(ctx, &argv, raw_cwd.as_deref(), e.code, e.message),
    };
    let cwd_display = cwd.display().to_string();
    let argv0 = argv[0].clone();

    let allow = string_list(ctx.state, "/policy/exec/allow");
    let deny = string_list(ctx.state, "/policy/exec/deny");

    // Deny wins over allow — shells/interpreters/wrappers are refused even if an
    // admin mistakenly allowlisted them (S3).
    if deny.iter().any(|d| d == &argv0) || DEFAULT_DENY.contains(&argv0.as_str()) {
        return refuse(
            ctx,
            &argv,
            Some(cwd_display.as_str()),
            ErrorCode::ECap,
            format!(
                "exec refused: argv[0] {argv0:?} is on the deny list (shells, interpreters and \
                 wrappers are always denied; /policy/exec/deny + built-in list) — no shell is \
                 ever spawned"
            ),
        );
    }
    if !allow.iter().any(|a| a == &argv0) {
        return refuse(
            ctx,
            &argv,
            Some(cwd_display.as_str()),
            ErrorCode::ECap,
            format!(
                "exec refused: argv[0] {argv0:?} is not in /policy/exec/allow \
                 (allowlist: {allow:?})"
            ),
        );
    }

    // Authorization: admin scope of the caller AND allow_exec:true on the binding
    // (schemas/binding.json). A non-admin caller gets E_SCOPE; a missing grant E_CAP.
    if !ctx.caller_admin {
        let trigger = ctx.trigger;
        return refuse(
            ctx,
            &argv,
            Some(cwd_display.as_str()),
            ErrorCode::EScope,
            format!(
                "exec refused: the trigger {trigger:?} is client-origin and the bus does not \
                 carry an admin scope (non-admin caller needs admin scope for exec steps)"
            ),
        );
    }
    let allow_exec = ctx
        .binding
        .get("allow_exec")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !allow_exec {
        return refuse(
            ctx,
            &argv,
            Some(cwd_display.as_str()),
            ErrorCode::ECap,
            "exec refused: the binding has no allow_exec:true grant (schemas/binding.json)"
                .to_string(),
        );
    }

    // Allowed: spawn fire-and-forget (the router never blocks on the child, S11).
    match spawn::spawn(&argv, &cwd) {
        Ok(pid) => {
            audit(
                ctx,
                &argv,
                Some(cwd_display.as_str()),
                "allow",
                "argv[0] is allowlisted, not denied; admin scope + allow_exec:true; spawned \
                 fire-and-forget with rlimits (cpu 10s, nproc 64, nofile 64, as 1GiB)",
                Some(pid),
                None,
            );
            Ok(pid)
        }
        Err(e) => refuse(
            ctx,
            &argv,
            Some(cwd_display.as_str()),
            ErrorCode::EInternal,
            format!("exec allowed by policy but the spawn failed: {e}"),
        ),
    }
}

/// Parse + shape-check the step's `argv` (non-empty, ≤ [`MAX_ARGV`], all strings,
/// absolute `argv[0]`, no interior NUL — execve cannot carry those).
fn parse_argv(step: &Value) -> Result<Vec<String>, IpcError> {
    let schema_err = |msg: String| IpcError::new(ErrorCode::ESchema, msg);
    let Some(arr) = step.get("argv").and_then(Value::as_array) else {
        return Err(schema_err(
            "exec step needs an 'argv' array of strings".to_string(),
        ));
    };
    if arr.is_empty() || arr.len() > MAX_ARGV {
        return Err(schema_err(format!(
            "exec step 'argv' must have 1..={MAX_ARGV} elements, got {}",
            arr.len()
        )));
    }
    let mut argv = Vec::with_capacity(arr.len());
    for a in arr {
        let Some(s) = a.as_str() else {
            return Err(schema_err(
                "exec step 'argv' elements must all be strings".to_string(),
            ));
        };
        if s.contains('\0') {
            return Err(schema_err(
                "exec step 'argv' elements must not contain NUL bytes".to_string(),
            ));
        }
        argv.push(s.to_string());
    }
    if !argv[0].starts_with('/') {
        return Err(schema_err(format!(
            "exec step argv[0] must be an absolute path (^/), got {:?} — relative paths are \
             refused, there is no PATH lookup and never a shell",
            argv[0]
        )));
    }
    Ok(argv)
}

/// Confine the step's `cwd` (default [`DEFAULT_CWD`]) to `/policy/fs/roots` + `/tmp`:
/// absolute, no `..` segment, must exist, must canonicalize under an allowed root.
fn confine_cwd(state: &StateHandle, cwd: Option<&str>) -> Result<PathBuf, IpcError> {
    let raw = cwd.unwrap_or(DEFAULT_CWD);
    if !raw.starts_with('/') {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!("exec step 'cwd' must be an absolute path, got {raw:?}"),
        ));
    }
    if Path::new(raw)
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!("exec step 'cwd' must not contain a '..' segment, got {raw:?}"),
        ));
    }
    let canon = std::fs::canonicalize(raw).map_err(|e| {
        IpcError::new(
            ErrorCode::EPath,
            format!("exec step 'cwd' {raw:?} does not exist / cannot be resolved: {e}"),
        )
    })?;
    let mut roots: Vec<PathBuf> = string_list(state, "/policy/fs/roots")
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    for extra in ALWAYS_CWD_ROOTS {
        if let Ok(c) = std::fs::canonicalize(extra) {
            roots.push(c);
        }
    }
    if roots.iter().any(|r| canon.starts_with(r)) {
        Ok(canon)
    } else {
        Err(IpcError::new(
            ErrorCode::ECap,
            format!(
                "exec refused: cwd {raw:?} (canonical {canon:?}) is outside /policy/fs/roots \
                 + {ALWAYS_CWD_ROOTS:?} (roots: {roots:?})"
            ),
        ))
    }
}

/// Read a string-array policy path; a missing/unreadable path is an empty list.
fn string_list(state: &StateHandle, path: &str) -> Vec<String> {
    state
        .get(path, None)
        .ok()
        .and_then(|r| r.value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Refuse an exec attempt: audit (log + `deck/system/exec` + ring buffer) and return
/// the typed error.
fn refuse(
    ctx: &ExecContext<'_>,
    argv: &[String],
    cwd: Option<&str>,
    code: ErrorCode,
    reason: String,
) -> Result<u32, IpcError> {
    audit(ctx, argv, cwd, "refused", &reason, None, Some(code));
    Err(IpcError::new(code, reason))
}

/// Emit one audit record: structured log line + `deck/system/exec` bus event +
/// in-memory ring buffer. `decision` is `"allow"` (with `pid`) or `"refused"` (with
/// the error `code` and `reason`).
fn audit(
    ctx: &ExecContext<'_>,
    argv: &[String],
    cwd: Option<&str>,
    decision: &str,
    reason: &str,
    pid: Option<u32>,
    code: Option<ErrorCode>,
) {
    let rev = ctx.state.rev().ok();
    let binding_id = ctx.binding.get("id").and_then(Value::as_str).unwrap_or("?");
    let mut payload = json!({
        "argv": argv,
        "cwd": cwd,
        "decision": decision,
        "reason": reason,
        "binding": binding_id,
        "trigger": ctx.trigger,
        "rev": rev,
    });
    if let Some(pid) = pid {
        payload["pid"] = pid.into();
    }
    if let Some(code) = code {
        payload["code"] = code.as_str().into();
    }
    lifecycle::log(
        if decision == "allow" { "info" } else { "warn" },
        "ds-daemon::policy",
        &format!(
            "exec {decision} (binding {binding_id}, trigger {:?}): argv={argv:?} cwd={} pid={} \
             code={} rev={rev:?} — {reason}",
            ctx.trigger,
            cwd.unwrap_or(""),
            pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            code.map(|c| c.as_str().to_string())
                .unwrap_or_else(|| "-".into()),
        ),
    );
    ctx.bus
        .publish(Event::topic(EXEC_AUDIT_TOPIC, payload.clone()));
    let ring = audit_ring();
    let mut q = ring.lock().unwrap();
    if q.len() >= AUDIT_RING_CAP {
        q.pop_front();
    }
    q.push_back(payload);
}

fn audit_ring() -> &'static Mutex<VecDeque<Value>> {
    static RING: OnceLock<Mutex<VecDeque<Value>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Snapshot of the in-memory audit ring buffer (last [`AUDIT_RING_CAP`] exec attempts,
/// allowed and refused).
pub fn audit_records() -> Vec<Value> {
    audit_ring().lock().unwrap().iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::bus::FnSink;
    use crate::router::{Pattern, TopicPattern};
    use crate::state::StateTask;
    use ds_state::{Op, Store, Transaction};
    use serde_json::json;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    fn test_ctx<'a>(
        state: &'a StateHandle,
        bus: &'a Bus,
        binding: &'a Value,
        trigger: &'a str,
        admin: bool,
    ) -> ExecContext<'a> {
        ExecContext {
            state,
            bus,
            binding,
            trigger,
            caller_admin: admin,
        }
    }

    struct Rig {
        bus: Bus,
        state: StateHandle,
        t: std::thread::JoinHandle<()>,
        rx: std::sync::mpsc::Receiver<Value>,
    }

    fn rig() -> Rig {
        let bus = Bus::new();
        let (state, t) = StateTask::spawn(Store::fresh(), None, bus.clone());
        let (tx, rx) = channel();
        let sink = std::sync::Arc::new(FnSink(std::sync::Arc::new(move |msg: &Value| {
            let _ = tx.send(msg.clone());
        })));
        bus.subscribe(
            Pattern::Topic(TopicPattern::parse("deck/system/*").unwrap()),
            None,
            "/deck/system/*".to_string(),
            sink,
        );
        Rig { bus, state, t, rx }
    }

    fn set_policy(rig: &Rig, path: &str, value: Value) {
        rig.state
            .set(Transaction::new(vec![Op::set(path, value)]))
            .unwrap();
    }

    fn binding(allow_exec: bool) -> Value {
        json!({"id": "b0001", "topic": "deck/control/keys.0/down", "allow_exec": allow_exec,
               "steps": [{"do": "exec", "argv": ["/bin/true"]}]})
    }

    fn exec_events(rx: &std::sync::mpsc::Receiver<Value>) -> Vec<Value> {
        // the bus delivers on a drainer thread: wait briefly for the audit event
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => {
                    if m.get("topic").and_then(Value::as_str) == Some(EXEC_AUDIT_TOPIC) {
                        out.push(m["payload"].clone());
                    }
                }
                Err(_) => break,
            }
        }
        out
    }

    #[test]
    fn relative_argv0_is_schema_error_and_audited() {
        let rig = rig();
        let b = binding(true);
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
        let err = check_exec(&ctx, &json!({"do": "exec", "argv": ["sh", "-c", "x"]})).unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        let evs = exec_events(&rig.rx);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["decision"], json!("refused"));
        assert_eq!(evs[0]["code"], json!("E_SCHEMA"));
        assert_eq!(evs[0]["argv"], json!(["sh", "-c", "x"]));
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn shells_and_wrappers_are_denied_even_when_allowlisted() {
        let rig = rig();
        set_policy(
            &rig,
            "/policy/exec/allow",
            json!(["/bin/sh", "/usr/bin/env"]),
        );
        let b = binding(true);
        for argv0 in ["/bin/sh", "/usr/bin/env", "/bin/bash", "/usr/bin/python3"] {
            let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
            let err = check_exec(
                &ctx,
                &json!({"do": "exec", "argv": [argv0, "-c", "touch /tmp/pwn"]}),
            )
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::ECap, "{argv0}: {err}");
            assert!(err.message.contains("deny list"), "{argv0}: {err}");
        }
        let evs = exec_events(&rig.rx);
        assert_eq!(evs.len(), 4);
        assert!(evs
            .iter()
            .all(|e| e["decision"] == json!("refused") && e["code"] == json!("E_CAP")));
        assert!(evs.iter().all(|e| e.get("pid").is_none()));
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn binary_not_in_allow_list_is_refused() {
        let rig = rig();
        set_policy(&rig, "/policy/exec/allow", json!(["/bin/true"]));
        let b = binding(true);
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
        let err = check_exec(&ctx, &json!({"do": "exec", "argv": ["/bin/false"]})).unwrap_err();
        assert_eq!(err.code, ErrorCode::ECap);
        assert!(err.message.contains("not in /policy/exec/allow"), "{err}");
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn non_admin_caller_gets_e_scope() {
        let rig = rig();
        set_policy(&rig, "/policy/exec/allow", json!(["/bin/true"]));
        let b = binding(true);
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "app/demo/exec", false);
        let err = check_exec(&ctx, &json!({"do": "exec", "argv": ["/bin/true"]})).unwrap_err();
        assert_eq!(err.code, ErrorCode::EScope);
        let evs = exec_events(&rig.rx);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0]["code"], json!("E_SCOPE"));
        assert_eq!(evs[0]["decision"], json!("refused"));
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn binding_without_allow_exec_is_refused_with_e_cap() {
        let rig = rig();
        set_policy(&rig, "/policy/exec/allow", json!(["/bin/true"]));
        let b = binding(false);
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
        let err = check_exec(&ctx, &json!({"do": "exec", "argv": ["/bin/true"]})).unwrap_err();
        assert_eq!(err.code, ErrorCode::ECap);
        assert!(err.message.contains("allow_exec"), "{err}");
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn cwd_outside_fs_roots_is_refused() {
        let rig = rig();
        set_policy(&rig, "/policy/exec/allow", json!(["/bin/true"]));
        set_policy(&rig, "/policy/fs/roots", json!(["/usr"]));
        let b = binding(true);
        // outside roots
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
        let err = check_exec(
            &ctx,
            &json!({"do": "exec", "argv": ["/bin/true"], "cwd": "/etc"}),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::ECap);
        assert!(err.message.contains("outside"), "{err}");
        // relative cwd -> E_SCHEMA
        let err = check_exec(
            &ctx,
            &json!({"do": "exec", "argv": ["/bin/true"], "cwd": "relative/dir"}),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // '..' segment -> E_SCHEMA
        let err = check_exec(
            &ctx,
            &json!({"do": "exec", "argv": ["/bin/true"], "cwd": "/tmp/../etc"}),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::ESchema);
        // nonexistent -> E_PATH
        let err = check_exec(
            &ctx,
            &json!({"do": "exec", "argv": ["/bin/true"], "cwd": "/does/not/exist"}),
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::EPath);
        // /tmp is always allowed
        set_policy(&rig, "/policy/fs/roots", json!([]));
        let pid = check_exec(
            &ctx,
            &json!({"do": "exec", "argv": ["/bin/true"], "cwd": "/tmp"}),
        )
        .unwrap();
        assert!(pid > 0);
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn allowed_absolute_binary_spawns_and_is_audited_with_pid() {
        let rig = rig();
        set_policy(&rig, "/policy/exec/allow", json!(["/bin/true"]));
        let b = binding(true);
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
        let pid = check_exec(&ctx, &json!({"do": "exec", "argv": ["/bin/true"]})).unwrap();
        assert!(pid > 0);
        // the audit event carries argv, cwd, decision=allow, pid, rev
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut evs = Vec::new();
        while std::time::Instant::now() < deadline {
            evs.extend(exec_events(&rig.rx));
            if !evs.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(evs.len(), 1, "exactly one audit event: {evs:?}");
        let e = &evs[0];
        assert_eq!(e["decision"], json!("allow"));
        assert_eq!(e["argv"], json!(["/bin/true"]));
        assert_eq!(
            e["cwd"],
            json!(PathBuf::from("/tmp")
                .canonicalize()
                .unwrap()
                .display()
                .to_string())
        );
        assert_eq!(e["pid"], json!(pid));
        assert!(e["rev"].is_number());
        assert_eq!(e["binding"], json!("b0001"));
        // the ring buffer holds the same record
        let ring = audit_records();
        assert!(ring
            .iter()
            .any(|r| r["decision"] == json!("allow") && r["pid"] == json!(pid)));
        // the child exits cleanly (reaped, no zombie)
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if spawn::exit_status(pid).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            matches!(
                spawn::exit_status(pid),
                Some(nix::sys::wait::WaitStatus::Exited(_, 0))
            ),
            "child /bin/true must exit 0, got {:?}",
            spawn::exit_status(pid)
        );
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn metacharacter_arguments_pass_the_policy_unchanged_and_are_audited_literally() {
        // The real "argument arrives literally, no shell interpolation" proof runs in
        // tests/conformance/exec against the ds-daemon binary (the unit-test binary's
        // main is libtest and never reaches the exec-child marker). Here: a
        // metacharacter-laden argument is accepted by the policy (it is just a string),
        // spawned, and recorded verbatim in the audit event.
        let rig = rig();
        let dir = tempfile::tempdir().unwrap();
        let weird = format!("{}/meta$(touch pwned)`id`; rm -rf", dir.path().display());
        set_policy(&rig, "/policy/exec/allow", json!(["/usr/bin/touch"]));
        set_policy(
            &rig,
            "/policy/fs/roots",
            json!([dir.path().display().to_string()]),
        );
        let b = binding(true);
        let ctx = test_ctx(&rig.state, &rig.bus, &b, "deck/control/keys.0/down", true);
        let argv = vec!["/usr/bin/touch".to_string(), weird.clone()];
        let pid = check_exec(
            &ctx,
            &json!({"do": "exec", "argv": argv, "cwd": dir.path().display().to_string()}),
        )
        .unwrap();
        assert!(pid > 0);
        let evs = exec_events(&rig.rx);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0]["decision"], json!("allow"));
        assert_eq!(evs[0]["argv"], json!(["/usr/bin/touch", weird]));
        assert_eq!(evs[0]["pid"], json!(pid));
        rig.state.stop();
        rig.t.join().unwrap();
    }

    #[test]
    fn caller_is_admin_by_trigger_namespace() {
        assert!(caller_is_admin("deck/control/keys.0/down"));
        assert!(caller_is_admin("deck/system/exec"));
        assert!(caller_is_admin("daemon/start"));
        assert!(!caller_is_admin("app/demo/exec"));
        assert!(!caller_is_admin("plugin/demo/event"));
    }
}
