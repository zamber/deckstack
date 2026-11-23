//! In-crate tests for the plugin host (ticket DS-050). The test plugins are tiny
//! fixtures under `src/plugins/fixtures/` (copied into a temp base dir by each test);
//! the spawned children run through the real `ds-daemon` binary (the re-exec marker
//! path), so the rlimits + Landlock + argv-only execve are exercised for real.

use super::host::{Host, Status, INVOKE_TIMEOUT};
use super::{register, spawn, wire};
use crate::router::bus::{Bus, Event, FnSink};
use crate::router::steps;
use crate::router::Pattern;
use crate::state::StateTask;
use ds_ipc::error::ErrorCode;
use ds_state::{Op, Store, Transaction};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The real `ds-daemon` binary used as the plugin-child launcher in tests.
fn launcher() -> PathBuf {
    // 1) compile-time (cargo sets CARGO_BIN_EXE_<name> for the package's targets)
    if let Some(p) = option_env!("CARGO_BIN_EXE_ds-daemon") {
        return PathBuf::from(p);
    }
    // 2) runtime env
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_ds-daemon") {
        return PathBuf::from(p);
    }
    // 3) repo-root target dir (CARGO_MANIFEST_DIR = <root>/crates/ds-daemon)
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .unwrap()
        .to_path_buf();
    for cand in [
        root.join("target/debug/ds-daemon"),
        root.join("target/debug/deps/ds-daemon"),
    ] {
        if cand.is_file() {
            return cand;
        }
    }
    // 4) walk up from the test binary
    let exe = std::env::current_exe().unwrap();
    for anc in exe.ancestors() {
        let cand = anc.join("ds-daemon");
        if cand.is_file() {
            return cand;
        }
    }
    panic!(
        "ds-daemon bin not found (looked near {} and {})",
        exe.display(),
        root.display()
    );
}

fn copy_tree(src: &PathBuf, dest: &PathBuf) {
    std::fs::create_dir_all(dest).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dest.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
}

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/plugins/fixtures")
        .join(name)
}

struct Rig {
    host: Arc<Host>,
    state: crate::state::StateHandle,
    bus: Bus,
    t: std::sync::Mutex<Option<JoinHandle<()>>>,
    _tmp: tempfile::TempDir,
    base: PathBuf,
}

fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("plugins");
    std::fs::create_dir_all(&base).unwrap();
    let bus = Bus::new();
    let (state, t) = StateTask::spawn(Store::fresh(), None, bus.clone());
    let host = Host::with_launcher(state.clone(), bus.clone(), base.clone(), launcher());
    register(&host);
    Rig {
        host,
        state,
        bus,
        t: std::sync::Mutex::new(Some(t)),
        _tmp: tmp,
        base,
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.host.stop_all();
        self.state.stop();
        if let Some(t) = self.t.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

impl Rig {
    /// Install a fixture under a UNIQUE plugin id (tests run in parallel in one
    /// process and share the global host registry, so ids must not collide).
    fn install(&self, fixture: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let id = format!("t{n}");
        let src = self._tmp.path().join(format!("src{n}"));
        copy_tree(&fixture_dir(fixture), &src);
        let mp = src.join("manifest.json");
        let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&mp).unwrap()).unwrap();
        m["id"] = json!(id);
        std::fs::write(&mp, serde_json::to_string_pretty(&m).unwrap()).unwrap();
        self.host.install(&src).unwrap()
    }

    fn wait_running(&self, id: &str) -> Status {
        self.host
            .wait_ready(id, Duration::from_secs(15))
            .expect("wait_ready")
    }

    fn state_get(&self, path: &str) -> Value {
        self.state.get(path, None).unwrap().value
    }

    fn poll_state(&self, path: &str, timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(r) = self.state.get(path, None) {
                return r.value;
            }
            assert!(Instant::now() < deadline, "poll timeout for {path}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn poll_plugin_field<F: Fn(&Value) -> bool>(
        &self,
        id: &str,
        timeout: Duration,
        pred: F,
    ) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let v = self.host.plugin_state(id).unwrap();
            if pred(&v) {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "poll timeout for plugin {id}: {v}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn collect(bus: &Bus, wire_pat: &str) -> Receiver<Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    let sink = Arc::new(FnSink(Arc::new(move |msg: &Value| {
        let _ = tx.send(msg.clone());
    })));
    bus.subscribe(
        Pattern::parse_wire(wire_pat).unwrap(),
        None,
        wire_pat.to_string(),
        sink,
    );
    rx
}

/// One tiny HTTP server connection handler: answers `200 OK` with a fixed body.
fn serve_one_http(listener: TcpListener) {
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => break,
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        // read the request head (read_line appends, so clear per line)
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let body = "hello-net";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        let _ = stream.flush();
    }
}

// ---- tests -------------------------------------------------------------------

#[test]
fn handshake_happy_path_and_lifecycle_state_exposure() {
    let rig = rig();
    let id = rig.install("good");
    let status = rig.wait_running(&id);
    assert_eq!(status, Status::Running, "{:?}", rig.host.plugin_state(&id));
    // /plugins/<id> in the state document: status + manifest + stats
    let plugins = rig.state_get("/plugins");
    assert_eq!(plugins[&id]["status"], json!("running"));
    assert_eq!(plugins[&id]["manifest"]["id"], json!(id));
    assert_eq!(
        plugins[&id]["manifest"]["capabilities"][0],
        json!("state.read")
    );
    assert_eq!(plugins[&id]["stats"]["invokes"], json!(0));
    // the host list() view matches
    assert_eq!(rig.host.list()[&id]["status"], json!("running"));
    // the child reported its Landlock status on stderr -> ring buffer + daemon log
    let logs = rig.host.logs(&id);
    assert!(
        logs.iter().any(|l| l.contains("landlock")),
        "expected the landlock status line in the ring buffer: {logs:?}"
    );
    // the plugin saw DS_PLUGIN_TOKEN
    assert!(logs.iter().any(|l| l.contains("token-present")), "{logs:?}");
}

#[test]
fn invoke_applies_effects_through_the_state_task() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    let r = rig
        .host
        .invoke(&id, "bump", &json!({"by": 2}))
        .expect("invoke bump");
    assert_eq!(r["applied"], json!(2));
    assert_eq!(r["rejected"], json!(0));
    assert_eq!(
        rig.state_get(&format!("/plugins/{id}/state/count")),
        json!(2)
    );
    assert_eq!(rig.state_get("/controls/keys/0/label"), json!("2"));
    let r = rig.host.invoke(&id, "bump", &json!({"by": 3})).unwrap();
    assert_eq!(r["applied"], json!(2));
    assert_eq!(
        rig.state_get(&format!("/plugins/{id}/state/count")),
        json!(5)
    );
    assert_eq!(rig.state_get("/controls/keys/0/label"), json!("5"));
    let st = rig.host.plugin_state(&id).unwrap();
    assert_eq!(st["stats"]["invokes"], json!(2));
}

#[test]
fn invoke_args_are_validated_against_the_params_schema() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    // "by" must be an integer >= 1
    let err = rig
        .host
        .invoke(&id, "bump", &json!({"by": "x"}))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ESchema);
    let err = rig.host.invoke(&id, "bump", &json!({"by": 0})).unwrap_err();
    assert_eq!(err.code, ErrorCode::ESchema);
    // nothing was sent: invokes counter unchanged, no state change
    assert_eq!(
        rig.host.plugin_state(&id).unwrap()["stats"]["invokes"],
        json!(0)
    );
    assert!(rig.state.get("/controls/keys/0/label", None).is_err());
}

#[test]
fn unknown_plugin_and_action_are_e_path() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    let err = rig.host.invoke("nope", "x", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::EPath);
    let err = rig.host.invoke(&id, "nope", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::EPath);
}

#[test]
fn bad_manifest_is_rejected_at_install_with_e_schema() {
    let rig = rig();
    let err = rig.host.install(&fixture_dir("badmanifest")).unwrap_err();
    assert_eq!(err.code, ErrorCode::ESchema);
    assert!(err.message.contains("plugin-manifest"));
    // nothing was installed
    assert!(rig.host.list().as_object().unwrap().is_empty());
    assert!(std::fs::read_dir(&rig.base).unwrap().count() == 0);
}

#[test]
fn register_manifest_mismatch_is_e_schema_and_the_child_is_killed() {
    let rig = rig();
    let id = rig.install("mismatch");
    // the supervisor rejects the tampered register, kills the child and records why
    let v = rig.poll_plugin_field(&id, Duration::from_secs(15), |v| {
        v["state"]["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("manifest"))
    });
    assert!(v["state"]["last_error"]
        .as_str()
        .unwrap()
        .contains("manifest"));
    assert_ne!(v["status"], json!("running"));
    rig.host.stop(&id);
}

#[test]
fn capability_violations_are_rejected_counted_and_state_untouched() {
    let rig = rig();
    let events = collect(&rig.bus, "/app/*");
    let id = rig.install("good");
    rig.wait_running(&id);
    let r = rig.host.invoke(&id, "violate", &json!({})).unwrap();
    assert_eq!(r["applied"], json!(0));
    assert_eq!(r["rejected"], json!(2), "both effects rejected: {r}");
    // counted in /plugins/<id>/stats.violations
    let st = rig.host.plugin_state(&id).unwrap();
    assert_eq!(st["stats"]["violations"], json!(2));
    // state untouched: /policy/exec/allow was never created
    assert!(rig.state.get("/policy/exec/allow", None).is_err());
    // no app/* event was published by the emit effect
    assert!(events.recv_timeout(Duration::from_millis(400)).is_err());
    // the plugin can still be invoked afterwards (daemon unaffected)
    let r = rig.host.invoke(&id, "echo", &json!({"ok": true})).unwrap();
    assert_eq!(r["applied"], json!(1));
}

#[test]
fn crash_mid_call_returns_e_device_and_the_watchdog_respawns() {
    let rig = rig();
    let crashes = collect(&rig.bus, "/plugin/*");
    let id = rig.install("good");
    rig.wait_running(&id);
    let err = rig.host.invoke(&id, "crash", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::EDevice, "{err}");
    assert!(err.message.contains("crashed"));
    // crash log event on the bus (plugin/<id>/crash)
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut crash_ev = None;
    while Instant::now() < deadline {
        if let Ok(m) = crashes.recv_timeout(Duration::from_millis(200)) {
            if m["topic"] == json!(format!("plugin/{id}/crash")) {
                crash_ev = Some(m);
                break;
            }
        }
    }
    let ev = crash_ev.expect("plugin crash event");
    assert_eq!(ev["payload"]["plugin"], json!(id));
    // watchdog respawned: running again, restarts counted
    let v = rig.poll_plugin_field(&id, Duration::from_secs(15), |v| {
        v["status"] == json!("running") && v["stats"]["restarts"] == json!(1)
    });
    assert_eq!(v["stats"]["restarts"], json!(1));
    // the respawned process works
    let r = rig
        .host
        .invoke(&id, "echo", &json!({"after": "crash"}))
        .unwrap();
    assert_eq!(r["applied"], json!(1));
}

#[test]
fn watchdog_disables_the_plugin_after_3_respawns_in_10_min() {
    let rig = rig();
    let disables = collect(&rig.bus, "/plugin/*");
    let id = rig.install("crashloop");
    // crash-loop: registers, exits; after 3 respawns the plugin is disabled
    let v = rig.poll_plugin_field(&id, Duration::from_secs(30), |v| {
        v["status"] == json!("stopped")
            && v["state"]["disabled_reason"]
                .as_str()
                .is_some_and(|r| r.contains("watchdog"))
    });
    assert_eq!(
        v["stats"]["restarts"],
        json!(3),
        "respawn <= 3 per 10 min: {v}"
    );
    assert!(
        v["state"]["pid"].is_null(),
        "no live child when disabled: {v}"
    );
    // the disabled event went out on the bus
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = false;
    while Instant::now() < deadline {
        if let Ok(m) = disables.recv_timeout(Duration::from_millis(200)) {
            if m["topic"] == json!(format!("plugin/{id}/disabled")) {
                got = true;
                break;
            }
        }
    }
    assert!(got, "plugin disabled event");
    // invokes on a disabled plugin fail with E_DEVICE, never block
    let err = rig.host.invoke(&id, "bump", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::EDevice);
}

#[test]
fn invoke_timeout_degrades_then_restarts_on_second_consecutive() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    // first timeout: E_LIMIT + degraded
    let t0 = Instant::now();
    let err = rig.host.invoke(&id, "sleep", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::ELimit);
    assert!(
        t0.elapsed() >= INVOKE_TIMEOUT && t0.elapsed() < INVOKE_TIMEOUT + Duration::from_secs(2)
    );
    let v = rig.host.plugin_state(&id).unwrap();
    assert_eq!(v["status"], json!("degraded"), "{v}");
    assert!(v["state"]["timeouts"].as_u64().unwrap() >= 1);
    // second consecutive timeout: restart requested -> respawned, running again
    let err = rig.host.invoke(&id, "sleep", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::ELimit);
    let v = rig.poll_plugin_field(&id, Duration::from_secs(15), |v| {
        v["status"] == json!("running") && v["stats"]["restarts"].as_u64().unwrap() >= 1
    });
    assert!(v["stats"]["restarts"].as_u64().unwrap() >= 1);
    // the restarted plugin answers again
    let r = rig
        .host
        .invoke(&id, "echo", &json!({"back": true}))
        .unwrap();
    assert_eq!(r["applied"], json!(1));
    // a successful invoke resets the consecutive-timeout counter
    let v = rig.host.plugin_state(&id).unwrap();
    assert_eq!(v["state"]["timeouts"], json!(0));
}

#[test]
fn plugin_step_goes_through_the_router_engine() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    let errors = collect(&rig.bus, "/deck/system/*");
    // bind a plugin step on a topic, then dispatch through the DS-042 engine
    rig.state
        .bind(json!({
            "id": "b0000",
            "topic": "app/demo/go",
            "steps": [{"do": "plugin", "id": id, "action": "bump", "args": {"by": 1}}]
        }))
        .unwrap();
    let engine = steps::Engine::new(rig.bus.clone(), rig.state.clone());
    engine.dispatch("app/demo/go", &json!({}), 0);
    // the plugin step invoked the host (not the E_CAP stub): the effect landed in state
    assert_eq!(
        rig.poll_state("/controls/keys/0/label", Duration::from_secs(5)),
        json!("1")
    );
    assert_eq!(
        rig.state_get(&format!("/plugins/{id}/state/count")),
        json!(1)
    );
    assert_eq!(
        rig.host.plugin_state(&id).unwrap()["stats"]["invokes"],
        json!(1)
    );
    // no binding-error was published for the plugin step
    assert!(errors.recv_timeout(Duration::from_millis(400)).is_err());
}

#[test]
fn invoke_with_returns_the_typed_stub_when_no_host_serves_the_plugin() {
    // fresh bus/state, no registered host for "demo", no default plugin dir in the
    // test env -> the DS-042 stub error (E_CAP mentioning DS-050)
    let bus = Bus::new();
    let (state, t) = StateTask::spawn(Store::fresh(), None, bus.clone());
    let err = super::invoke_with(&state, &bus, "demo", "bump", &json!({})).unwrap_err();
    assert_eq!(err.code, ErrorCode::ECap);
    assert!(err.message.contains("DS-050"), "{err}");
    state.stop();
    t.join().unwrap();
}

#[test]
fn net_requests_are_host_mediated_glob_and_policy_checked() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    // admin policy allows 127.0.0.1
    rig.state
        .set(Transaction::new(vec![Op::set(
            "/policy/net/allow_hosts",
            json!(["127.0.0.1"]),
        )]))
        .unwrap();
    // local HTTP server
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || serve_one_http(listener));

    // allowed: net:127.0.0.1 (glob) AND /policy/net/allow_hosts -> daemon fetches itself
    let r = rig
        .host
        .invoke(
            &id,
            "net",
            &json!({"url": format!("http://127.0.0.1:{port}/x") }),
        )
        .unwrap();
    assert_eq!(r["applied"], json!(2), "{r}");
    let reply = rig.poll_state(
        &format!("/plugins/{id}/state/last_reply"),
        Duration::from_secs(5),
    );
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert_eq!(reply["status"], json!(200));
    assert_eq!(reply["body"], json!("hello-net"));

    // denied by the plugin's glob (evil.com vs net:127.0.0.1) -> E_CAP reply + violation
    let r = rig
        .host
        .invoke(&id, "net", &json!({"url": "http://evil.com/x"}))
        .unwrap();
    assert_eq!(r["applied"], json!(2));
    let reply = rig.poll_state(
        &format!("/plugins/{id}/state/last_reply"),
        Duration::from_secs(5),
    );
    assert_eq!(reply["ok"], json!(false), "{reply}");
    assert_eq!(reply["code"], json!("E_CAP"));
    let st = rig.host.plugin_state(&id).unwrap();
    assert!(st["stats"]["violations"].as_u64().unwrap() >= 1);

    // denied by /policy/net/allow_hosts even with a matching glob: use a host that
    // matches no glob first is covered above; here 127.0.0.1 without the policy entry
    rig.state
        .set(Transaction::new(vec![Op::set(
            "/policy/net/allow_hosts",
            json!([]),
        )]))
        .unwrap();
    let _ = rig
        .host
        .invoke(
            &id,
            "net",
            &json!({"url": format!("http://127.0.0.1:{port}/y")}),
        )
        .unwrap();
    let reply = rig.poll_state(
        &format!("/plugins/{id}/state/last_reply"),
        Duration::from_secs(5),
    );
    assert_eq!(reply["ok"], json!(false));
    assert_eq!(reply["code"], json!("E_CAP"));
    assert!(reply["message"].as_str().unwrap().contains("allow_hosts"));
}

#[test]
fn get_requests_need_state_read_and_return_state() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    rig.state
        .set(Transaction::new(vec![Op::set(
            "/controls/keys/1/label",
            json!("visible"),
        )]))
        .unwrap();
    let _ = rig
        .host
        .invoke(&id, "getstate", &json!({"path": "/controls/keys/1/label"}))
        .unwrap();
    let reply = rig.poll_state(
        &format!("/plugins/{id}/state/last_reply"),
        Duration::from_secs(5),
    );
    assert_eq!(reply["ok"], json!(true), "{reply}");
    assert_eq!(reply["value"], json!("visible"));
}

#[test]
fn landlock_denies_fs_outside_roots_and_raw_tcp_for_the_plugin() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    // a secret file OUTSIDE the plugin dir and fs_roots
    let secret = rig._tmp.path().join("secret.txt");
    std::fs::write(&secret, "topsecret").unwrap();
    // a listening TCP socket the plugin could otherwise reach directly
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in listener.incoming() {
            drop(s);
        }
    });
    let _ = rig
        .host
        .invoke(
            &id,
            "probe",
            &json!({"secret": secret.display().to_string(), "port": port}),
        )
        .unwrap();
    let probe = rig.poll_state(
        &format!("/plugins/{id}/state/probe"),
        Duration::from_secs(5),
    );
    let landlock_line = rig
        .host
        .logs(&id)
        .iter()
        .find(|l| l.contains("landlock"))
        .cloned()
        .unwrap_or_default();
    eprintln!("landlock line: {landlock_line}; probe: {probe}");
    if landlock_line.contains("Enforced") {
        // Landlock active: the plugin could NOT read the secret file nor open a TCP
        // socket (host-mediated net only)
        assert_eq!(probe["secret"], json!("PermissionError"), "{probe}");
        assert_eq!(probe["tcp"], json!("PermissionError"), "{probe}");
    } else {
        // documented gap (no Landlock on this kernel): host-enforced checks only
        assert_eq!(probe["secret"], json!("read-ok"));
    }
    // control: the plugin CAN read its own dir (it read manifest.json to register)
    assert_eq!(
        rig.host.plugin_state(&id).unwrap()["status"],
        json!("running")
    );
}

#[test]
fn events_are_fanned_out_to_subscribed_running_plugins() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    rig.bus
        .publish(Event::topic("deck/control/keys.0/down", json!({"key": 0})));
    // state change -> "state" event
    rig.state
        .set(Transaction::new(vec![Op::set(
            "/controls/keys/2/label",
            json!("x"),
        )]))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let logs = rig.host.logs(&id);
        let has_key = logs.iter().any(|l| l.contains("event:keyDown"));
        let has_state = logs.iter().any(|l| l.contains("event:state"));
        if has_key && has_state {
            break;
        }
        assert!(Instant::now() < deadline, "logs: {logs:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn stop_all_kills_every_plugin_process_group() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    let pid = rig.host.plugin_state(&id).unwrap()["state"]["pid"]
        .as_u64()
        .unwrap() as u32;
    assert!(pid > 0);
    rig.host.stop_all();
    // the process is gone (reaped or dead)
    let deadline = Instant::now() + Duration::from_secs(5);
    while spawn::exit_status(pid).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        spawn::exit_status(pid).is_some(),
        "plugin pid {pid} was reaped"
    );
    assert_eq!(
        rig.host.plugin_state(&id).unwrap()["status"],
        json!("stopped")
    );
    assert!(rig.host.plugin_state(&id).unwrap()["state"]["pid"].is_null());
}

#[test]
fn remove_deletes_the_plugin_dir_and_its_state_entry() {
    let rig = rig();
    let id = rig.install("good");
    rig.wait_running(&id);
    rig.host.remove(&id).unwrap();
    assert!(!rig.base.join(&id).exists());
    assert!(!rig.host.has_plugin(&id));
    assert!(rig.host.plugin_state(&id).is_err());
    assert_eq!(rig.state_get("/plugins"), json!({}));
}

#[test]
fn http_fetch_and_url_parsing_helpers() {
    // parse_http_url
    let u = super::host::parse_http_url("http://lights.lan/api/x?y=1").unwrap();
    assert_eq!(
        (u.host.as_str(), u.port, u.path.as_str()),
        ("lights.lan", 80, "/api/x?y=1")
    );
    let u = super::host::parse_http_url("http://127.0.0.1:8080").unwrap();
    assert_eq!(
        (u.host.as_str(), u.port, u.path.as_str()),
        ("127.0.0.1", 8080, "/")
    );
    assert!(super::host::parse_http_url("https://x/").is_none());
    assert!(super::host::parse_http_url("http://").is_none());
    // http_fetch against a local server
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || serve_one_http(listener));
    let (code, body) =
        super::host::http_fetch("GET", &format!("http://127.0.0.1:{port}/p")).unwrap();
    assert_eq!(code, 200);
    assert_eq!(body, "hello-net");
    // unreachable port -> Err, never a panic
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = dead.local_addr().unwrap().port();
    drop(dead);
    assert!(super::host::http_fetch("GET", &format!("http://127.0.0.1:{dead_port}/")).is_err());
}

#[test]
fn writable_path_subset_is_enforced() {
    assert!(super::host::writable_path("tp1", "/controls/keys/0/label"));
    assert!(super::host::writable_path("tp1", "/plugins/tp1/state/x"));
    assert!(!super::host::writable_path("tp1", "/plugins/tp2/state/x")); // different plugin subtree
    assert!(!super::host::writable_path("tp1", "/policy/exec/allow"));
    assert!(!super::host::writable_path("tp1", "/bindings"));
    assert!(!super::host::writable_path("tp1", "/controls"));
}

#[test]
fn wire_and_glob_helpers_under_test() {
    assert!(wire::glob_matches_host("*.lan", "lights.lan"));
    assert!(!wire::glob_matches_host("*.lan", "a.b.lan"));
    assert_eq!(
        wire::topic_to_event("deck/control/keys.3/down"),
        Some("keyDown")
    );
    let m = wire::hello_message(
        "t",
        &["state.read".to_string()],
        &[],
        &super::manifest::Resources::default(),
    );
    assert_eq!(m["op"], json!("hello"));
    assert_eq!(m["protocol"], json!(1));
}
