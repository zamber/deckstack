//! The phase-1 verbs of `ds` (`docs/contracts/cli.md` §1): get, set, watch, schema,
//! describe, daemon start|stop|status, status, and the binding verbs bind/unbind
//! (DS-041, in `bind.rs`/`unbind.rs`). Every daemon-backed verb is one-shot:
//! connect, hello, token auth, one op, print, exit (`docs/components/cli.md`).

use crate::cli::{Command, Global, OutputMode, SetValue};
use crate::connect::{client_name, connect, data_dir, socket_path};
use crate::output::{print_object, render_human, server_failure, Failure};
use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

/// `schemas/state.json`, embedded at compile time (`ds schema state`, `describe`).
pub const STATE_SCHEMA_JSON: &str = include_str!("../../../schemas/state.json");
/// `schemas/ipc-message.json`.
pub const IPC_SCHEMA_JSON: &str = include_str!("../../../schemas/ipc-message.json");
/// `schemas/stream-frame.json`.
pub const STREAM_FRAME_SCHEMA_JSON: &str = include_str!("../../../schemas/stream-frame.json");
/// `schemas/plugin-manifest.json`.
pub const PLUGIN_MANIFEST_SCHEMA_JSON: &str = include_str!("../../../schemas/plugin-manifest.json");
/// `schemas/binding.json`.
pub const BINDING_SCHEMA_JSON: &str = include_str!("../../../schemas/binding.json");

/// How long `daemon start` waits for the control socket + a successful status op.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long `daemon stop` waits for the socket to disappear afterwards (best effort).
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Run a parsed command; returns the process exit code.
pub fn run(cmd: Command, g: &Global) -> Result<i32, Failure> {
    match cmd {
        Command::Get { path, fields, rev } => verb_get(g, &path, fields.as_deref(), rev),
        Command::Set {
            path,
            value,
            if_rev,
        } => verb_set(g, &path, value, if_rev),
        Command::Watch {
            path,
            from_rev,
            max_events,
        } => verb_watch(g, &path, from_rev, max_events),
        Command::Schema { name } => verb_schema(g, &name),
        Command::Describe { path } => verb_describe(g, &path),
        Command::DaemonStart { device, foreground } => verb_daemon_start(g, device, foreground),
        Command::DaemonStop => verb_daemon_stop(g),
        Command::DaemonStatus => verb_daemon_status(g),
        Command::Status => verb_status(g),
        Command::Bind { topic, dos, when } => {
            crate::bind::verb_bind(g, &topic, &dos, when.as_deref())
        }
        Command::Unbind { id, topic } => {
            crate::unbind::verb_unbind(g, id.as_deref(), topic.as_deref())
        }
        Command::Help(_) | Command::Version => Ok(0), // handled in main
    }
}

/// `ds get <path> [--fields …] [--rev N]`.
fn verb_get(
    g: &Global,
    path: &str,
    fields: Option<&[String]>,
    rev: Option<u64>,
) -> Result<i32, Failure> {
    let mut c = connect(g)?;
    let resp = c.get(path, fields)?;
    let value = resp["result"]["value"].clone();
    let got_rev = resp.get("rev").and_then(Value::as_u64);
    if let Some(want) = rev {
        if got_rev != Some(want) {
            let mut e = IpcError::new(
                ErrorCode::EConflict,
                format!(
                    "rev mismatch: current rev is {}, requested {want}",
                    got_rev
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "none".to_string())
                ),
            );
            if let Some(cur) = got_rev {
                e = e.with_current_rev(cur);
            }
            return Err(server_failure(e));
        }
    }
    match g.mode {
        OutputMode::Json => {
            let mut out = json!({"path": path, "value": value});
            if let Some(r) = got_rev {
                out["rev"] = json!(r);
            }
            print_object(&out, g.compact);
        }
        _ => println!("{}", render_human(&value)),
    }
    Ok(0)
}

/// `ds set <path> <value|--file f> [--if-rev N]`.
fn verb_set(g: &Global, path: &str, value: SetValue, if_rev: Option<u64>) -> Result<i32, Failure> {
    let mut c = connect(g)?;
    let value = match value {
        SetValue::Inline(v) => v,
        SetValue::File(p) => file_visual(&mut c, &p)?,
    };
    let ops = json!([{"op": "set", "path": path, "value": value}]);
    let resp = c.set(ops, if_rev)?;
    let rev = resp.get("rev").and_then(Value::as_u64).unwrap_or(0);
    match g.mode {
        OutputMode::Json => print_object(&json!({"ok": true, "rev": rev}), g.compact),
        _ => println!("ok rev={rev}"),
    }
    Ok(0)
}

/// `--file <path>`: canonicalize and confine to `/policy/fs/roots` (+ `/tmp`,
/// `docs/contracts/actions.md` §4), then build the `{"kind":"file",…}` visual.
fn file_visual(c: &mut ds_client::Client, p: &Path) -> Result<Value, Failure> {
    let canon = std::fs::canonicalize(p)
        .map_err(|e| Failure::Usage(format!("cannot read file {}: {e}", p.display())))?;
    let mut allowed: Vec<PathBuf> = Vec::new();
    match c.get("/policy/fs/roots", None) {
        Ok(resp) => {
            if let Some(arr) = resp["result"]["value"].as_array() {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        allowed.push(canon_root(s));
                    }
                }
            }
        }
        // no roots configured yet: only the /tmp allowance applies
        Err(ClientError::Server(e)) if e.code == ErrorCode::EPath => {}
        Err(e) => return Err(Failure::Client(e)),
    }
    allowed.push(canon_root("/tmp"));
    if !allowed.iter().any(|root| canon.starts_with(root)) {
        return Err(server_failure(IpcError::new(
            ErrorCode::ECap,
            format!(
                "file {} is not under /policy/fs/roots (add a root with: ds set /policy/fs/roots --json '[\"/abs/dir\"]')",
                canon.display()
            ),
        )));
    }
    Ok(json!({"kind": "file", "path": canon.to_string_lossy()}))
}

fn canon_root(s: &str) -> PathBuf {
    std::fs::canonicalize(s).unwrap_or_else(|_| PathBuf::from(s))
}

/// `ds watch <path> [--ndjson] [--from-rev N] [--max-events N]` — the streaming verb.
fn verb_watch(
    g: &Global,
    path: &str,
    from_rev: Option<u64>,
    max_events: Option<usize>,
) -> Result<i32, Failure> {
    let mut c = connect(g)?;
    let mode = g.mode; // Json is rejected at parse time (contract §2)
    let stdout = std::io::stdout();
    let delivered = c.watch(path, from_rev, max_events, &mut |ev| {
        let mut out = stdout.lock();
        match mode {
            OutputMode::Ndjson => {
                if let Ok(line) = serde_json::to_string(ev) {
                    let _ = writeln!(out, "{line}");
                }
            }
            _ => {
                let path = ev.get("path").and_then(Value::as_str).unwrap_or("?");
                let rev = ev
                    .get("rev")
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let _ = writeln!(out, "event {path} rev={rev}");
            }
        }
        let _ = out.flush();
    })?;
    if mode != OutputMode::Ndjson {
        println!("ok events={delivered}");
    }
    Ok(0)
}

/// `ds schema <name>` — print an embedded schema (no daemon needed).
fn verb_schema(g: &Global, name: &str) -> Result<i32, Failure> {
    let raw = match name {
        "state" => STATE_SCHEMA_JSON,
        "ipc" => IPC_SCHEMA_JSON,
        "stream-frame" => STREAM_FRAME_SCHEMA_JSON,
        "plugin-manifest" => PLUGIN_MANIFEST_SCHEMA_JSON,
        "binding" => BINDING_SCHEMA_JSON,
        other => {
            return Err(Failure::Usage(format!(
                "unknown schema name '{other}' (valid: {})",
                crate::cli::SCHEMA_NAMES.join(", ")
            )))
        }
    };
    let v: Value = serde_json::from_str(raw)
        .map_err(|e| Failure::Usage(format!("embedded schema {name} does not parse: {e}")))?;
    print_object(&v, g.compact);
    Ok(0)
}

/// `ds describe <path>` — schema + current value + allowed verbs.
fn verb_describe(g: &Global, path: &str) -> Result<i32, Failure> {
    let mut c = connect(g)?;
    let resp = c.get(path, None)?;
    let value = resp["result"]["value"].clone();
    let rev = resp.get("rev").and_then(Value::as_u64);
    let verbs: Vec<&str> = if path.starts_with("/telemetry") {
        vec!["get", "watch", "describe"]
    } else {
        vec!["get", "set", "watch", "describe"]
    };
    let schema = schema_ref_for(path);
    match g.mode {
        OutputMode::Json => {
            let mut out = json!({
                "path": path,
                "value": value,
                "verbs": verbs,
                "schema": schema,
            });
            if let Some(r) = rev {
                out["rev"] = json!(r);
            }
            print_object(&out, g.compact);
        }
        _ => {
            println!("path: {path}");
            if let Some(r) = rev {
                println!("rev: {r}");
            }
            println!("value:");
            for line in render_human(&value).lines() {
                println!("  {line}");
            }
            println!("verbs: {}", verbs.join(", "));
            println!(
                "schema: {}",
                serde_json::to_string(&schema).unwrap_or_default()
            );
        }
    }
    Ok(0)
}

/// Map a state path to the subschema that governs it (a `$ref` into
/// `schemas/state.json` when there is one, else the section schema).
pub fn schema_ref_for(path: &str) -> Value {
    let root: Value = serde_json::from_str(STATE_SCHEMA_JSON).expect("state schema parses");
    let id = root["$id"].as_str().unwrap_or("").to_string();
    let ref_of = |name: &str| json!({"$ref": format!("{id}#/$defs/{name}")});
    let segs: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    match segs.as_slice() {
        [] => json!({"$ref": id}),
        ["controls"] => root["properties"]["controls"].clone(),
        ["controls", "dial"] => ref_of("screenControl"),
        ["controls", kind] => root["properties"]["controls"]["properties"][kind].clone(),
        ["controls", "dial", leaf] => leaf_schema(&root, leaf, &ref_of),
        ["controls", kind, idx] if is_control_index(kind, idx) => match *kind {
            "keys" => ref_of("screenControl"),
            _ => ref_of("ledControl"),
        },
        ["controls", _, _, leaf] => leaf_schema(&root, leaf, &ref_of),
        [top] => root["properties"].get(*top).cloned().unwrap_or(Value::Null),
        [top, second] => root["properties"]
            .get(*top)
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.get(*second))
            .cloned()
            .unwrap_or(Value::Null),
        _ => root["properties"]
            .get(segs[0])
            .cloned()
            .unwrap_or(Value::Null),
    }
}

/// The schema of a leaf attribute under a control (`image`, `label`, `brightness`,
/// `led`, `value`).
fn leaf_schema(root: &Value, leaf: &str, ref_of: &dyn Fn(&str) -> Value) -> Value {
    match leaf {
        "image" => ref_of("visual"),
        "label" => json!({"type": "string", "maxLength": 32}),
        "brightness" => json!({"type": "integer", "minimum": 1, "maximum": 100}),
        "led" => root["$defs"]["ledControl"]["properties"]["led"].clone(),
        "value" => json!({"type": "integer", "minimum": 0, "maximum": 100}),
        _ => Value::Null,
    }
}

fn is_control_index(kind: &str, idx: &str) -> bool {
    let range = match kind {
        "keys" => (0, 14),
        "knobs" => (0, 2),
        "faders" => (0, 4),
        "rollers" => (0, 1),
        _ => return false,
    };
    idx.parse::<u32>()
        .is_ok_and(|n| n >= range.0 && n <= range.1)
}

/// `ds daemon start` — spawn `ds-daemon run`, wait for the socket + status, print it.
fn verb_daemon_start(g: &Global, device: Option<String>, foreground: bool) -> Result<i32, Failure> {
    let dir = data_dir(g);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Failure::Usage(format!("cannot create data dir {}: {e}", dir.display())))?;
    let bin = daemon_bin();
    let mut args: Vec<String> = vec![
        "run".to_string(),
        "--data-dir".to_string(),
        dir.display().to_string(),
    ];
    if let Some(d) = &device {
        // phase 1: only virtual:/hid: URIs exist; auto = none until DS-021 hardware
        args.push("--device".to_string());
        args.push(if d == "auto" {
            "none".to_string()
        } else {
            d.clone()
        });
    }
    if foreground {
        let status = std::process::Command::new(&bin)
            .args(&args)
            .status()
            .map_err(|e| Failure::Usage(format!("cannot spawn {}: {e}", bin.display())))?;
        return Ok(status.code().unwrap_or(8));
    }
    let log_path = dir.join("daemon.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| Failure::Usage(format!("cannot open {}: {e}", log_path.display())))?;
    let mut child = std::process::Command::new(&bin)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| {
            Failure::Usage(format!(
                "cannot spawn {}: {e} (set DS_DAEMON_BIN or put ds-daemon next to the ds binary)",
                bin.display()
            ))
        })?;
    let socket = dir.join("daemon.sock");
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| Failure::Usage(format!("cannot poll the daemon child: {e}")))?
        {
            return Err(Failure::Usage(format!(
                "daemon exited during startup with {status} — see {}",
                log_path.display()
            )));
        }
        if socket.exists() {
            let mut gg = g.clone();
            gg.socket = Some(socket.clone());
            if let Ok(mut c) = connect(&gg) {
                if let Ok(resp) = c.request(json!({"op": "daemon", "action": "status"})) {
                    return print_status(g, resp["result"].clone(), &socket, &dir);
                }
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(Failure::Client(ClientError::Unreachable {
                path: socket.clone(),
                hint: format!(
                    "daemon did not come up within 5 s — see {}",
                    log_path.display()
                ),
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, "startup timeout"),
            }));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The ds-daemon binary: $DS_DAEMON_BIN, else the sibling of the running `ds`
/// binary, else `ds-daemon` on PATH.
fn daemon_bin() -> PathBuf {
    if let Ok(p) = std::env::var("DS_DAEMON_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("ds-daemon");
            if sibling.exists() {
                return sibling;
            }
        }
    }
    PathBuf::from("ds-daemon")
}

/// `ds daemon stop` — the `daemon/stop` control op (admin scope), then wait for the
/// socket to disappear (best effort).
fn verb_daemon_stop(g: &Global) -> Result<i32, Failure> {
    let mut c = connect(g)?;
    let resp = c.request(json!({"op": "daemon", "action": "stop"}))?;
    match g.mode {
        OutputMode::Json => print_object(
            &json!({"ok": true, "result": resp["result"].clone()}),
            g.compact,
        ),
        _ => println!("ok stopping"),
    }
    let socket = socket_path(g);
    let deadline = Instant::now() + STOP_TIMEOUT;
    while socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(0)
}

/// `ds daemon status` — socket/pid state plus the `daemon/status` control op.
fn verb_daemon_status(g: &Global) -> Result<i32, Failure> {
    let dir = data_dir(g);
    let socket = socket_path(g);
    if !socket.exists() {
        return Err(unreachable(&socket));
    }
    let mut c = connect(g)?;
    let resp = c.request(json!({"op": "daemon", "action": "status"}))?;
    print_status(g, resp["result"].clone(), &socket, &dir)
}

/// `ds status` — daemon status plus the client name.
fn verb_status(g: &Global) -> Result<i32, Failure> {
    let dir = data_dir(g);
    let socket = socket_path(g);
    if !socket.exists() {
        return Err(unreachable(&socket));
    }
    let mut c = connect(g)?;
    let resp = c.request(json!({"op": "daemon", "action": "status"}))?;
    let mut result = resp["result"].clone();
    result["client"] = json!(client_name());
    print_status(g, result, &socket, &dir)
}

fn unreachable(socket: &Path) -> Failure {
    Failure::Client(ClientError::Unreachable {
        path: socket.to_path_buf(),
        hint: "start it with: ds daemon start".to_string(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "socket missing"),
    })
}

/// Print a daemon-status result merged with the local socket/pid state.
fn print_status(g: &Global, mut result: Value, socket: &Path, dir: &Path) -> Result<i32, Failure> {
    let pid_path = dir.join("daemon.pid");
    result["socket"] = json!(socket.display().to_string());
    result["pid_file"] = json!(pid_path.display().to_string());
    if result.get("pid").is_none() {
        if let Ok(content) = std::fs::read_to_string(&pid_path) {
            if let Ok(pid) = content.trim().parse::<u64>() {
                result["pid"] = json!(pid);
            }
        }
    }
    match g.mode {
        OutputMode::Json => print_object(&result, g.compact),
        _ => {
            let client = result
                .get("client")
                .and_then(Value::as_str)
                .map(|c| format!(" client={c}"))
                .unwrap_or_default();
            println!(
                "ok running pid={} rev={} device={} socket={}{client}",
                result
                    .get("pid")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                result
                    .get("rev")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "-".into()),
                result.get("device").and_then(Value::as_str).unwrap_or("?"),
                socket.display(),
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_schemas_parse() {
        for raw in [
            STATE_SCHEMA_JSON,
            IPC_SCHEMA_JSON,
            STREAM_FRAME_SCHEMA_JSON,
            PLUGIN_MANIFEST_SCHEMA_JSON,
            BINDING_SCHEMA_JSON,
        ] {
            assert!(serde_json::from_str::<Value>(raw).is_ok());
        }
        assert!(STATE_SCHEMA_JSON.contains("\"$defs\""));
        assert!(STATE_SCHEMA_JSON.contains("\"visual\""));
    }

    #[test]
    fn schema_ref_for_maps_control_paths_to_defs() {
        assert_eq!(
            schema_ref_for("/controls/keys/0"),
            json!({"$ref": "https://deckstack.dev/schemas/state.json/v1#/$defs/screenControl"})
        );
        assert_eq!(
            schema_ref_for("/controls/dial"),
            json!({"$ref": "https://deckstack.dev/schemas/state.json/v1#/$defs/screenControl"})
        );
        assert_eq!(
            schema_ref_for("/controls/knobs/2"),
            json!({"$ref": "https://deckstack.dev/schemas/state.json/v1#/$defs/ledControl"})
        );
        assert_eq!(
            schema_ref_for("/controls/keys/0/image"),
            json!({"$ref": "https://deckstack.dev/schemas/state.json/v1#/$defs/visual"})
        );
        assert_eq!(
            schema_ref_for("/controls/keys/14/brightness"),
            json!({"type": "integer", "minimum": 1, "maximum": 100})
        );
        // out-of-range key index is not a control: falls through to the section schema
        assert!(schema_ref_for("/controls/keys/99").is_object());
        // top-level sections
        assert_eq!(
            schema_ref_for("/meta")["required"],
            json!(["version", "rev", "protocol"])
        );
        assert_eq!(
            schema_ref_for("/"),
            json!({"$ref": "https://deckstack.dev/schemas/state.json/v1"})
        );
    }

    #[test]
    fn is_control_index_ranges() {
        assert!(is_control_index("keys", "0"));
        assert!(is_control_index("keys", "14"));
        assert!(!is_control_index("keys", "15"));
        assert!(!is_control_index("keys", "99"));
        assert!(is_control_index("knobs", "2"));
        assert!(!is_control_index("knobs", "3"));
        assert!(!is_control_index("dial", "0"));
    }

    #[test]
    fn daemon_bin_prefers_env_then_sibling_then_path() {
        // In tests the sibling of the test binary is target/debug — ds-daemon is built
        // there by the workspace, so the sibling branch wins unless DS_DAEMON_BIN is set.
        std::env::set_var("DS_DAEMON_BIN", "/tmp/custom-ds-daemon");
        assert_eq!(daemon_bin(), PathBuf::from("/tmp/custom-ds-daemon"));
        std::env::remove_var("DS_DAEMON_BIN");
        let bin = daemon_bin();
        assert!(bin.ends_with("ds-daemon"), "{}", bin.display());
    }
}
