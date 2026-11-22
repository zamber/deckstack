//! Integration test for the `ds stream` verbs: an in-process stack exactly like
//! `tests/e2e/run.py` boots it — a ds-vdev virtual device, a control socket (ds-ipc
//! `Server` + a test handler serving the telemetry plane and the stream ops) and the
//! DS-013 stream socket at `<data>/stream.sock` (a test `FrameSink` that re-encodes
//! each accepted DSF1 frame to the device wire format and writes the chunked reports to
//! the vdev, like the DS-031 writer does) — driven ONLY through the compiled `ds`
//! binary, black box.
//!
//! Covers: one-frame push to `key:0` → the reassembled vdev frame sha equals the
//! golden encoded-frame sha (`tests/golden/example-key0.sha256`); `stream stats
//! key:0 --json` (object, no rev, incl. the `get /telemetry/streams/…` fallback);
//! `stream stop`; a 10-frame `--dir` push at 30 fps with the latest frame winning on the
//! device; bad target → exit 1; `--ndjson` on a stream verb → exit 1; missing
//! stream.sock / missing daemon socket → exit 6 `E_UNREACHABLE`.

use ds_daemon::frames::chunk::chunk_reports;
use ds_daemon::frames::encode;
use ds_daemon::frames::FrameTarget;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_ipc::frames::codec::Frame;
use ds_ipc::frames::server::{FrameSink, StreamServer};
use ds_ipc::frames::StreamConfig;
use ds_ipc::limits::ServerConfig;
use ds_ipc::message::{Request, Response};
use ds_ipc::server::{Connection, Handler, Server};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// helpers

fn ds_bin() -> PathBuf {
    if let Ok(p) = std::env::var("DS_BIN") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    // the usual place first (workspace target dir next to bins/ds)
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for cand in [
        manifest.join("../../target/debug/ds"),
        manifest.join("../../target/release/ds"),
    ] {
        if cand.is_file() {
            return cand;
        }
    }
    // else walk up from the test binary to a directory holding the built `ds`
    let exe = std::env::current_exe().unwrap();
    for anc in exe.ancestors() {
        let cand = anc.join("ds");
        if cand.is_file() && cand != exe && !anc.to_string_lossy().contains("/build/") {
            return cand;
        }
    }
    panic!("ds binary not found — set DS_BIN=/path/to/ds");
}

fn run_ds(bin: &Path, sock: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .arg("--socket")
        .arg(sock)
        .args(args)
        .output()
        .expect("spawn ds")
}

fn code(out: &Output) -> Option<i32> {
    out.status.code()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration, what: &str) {
    let deadline = Instant::now() + timeout;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// repo root (bins/ds/../..)
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

// ---------------------------------------------------------------------------
// a minimal host-side link to the ds-vdev device socket
// (u32 LE body_len | u8 kind | u16 LE pid | payload; write kind 0x01, ack 0x82)

struct VdevLink {
    w: UnixStream,
    r: BufReader<UnixStream>,
}

impl VdevLink {
    fn connect(path: &Path) -> std::io::Result<Self> {
        let w = UnixStream::connect(path)?;
        let r = BufReader::new(w.try_clone()?);
        Ok(Self { w, r })
    }

    fn send_report(&mut self, pid: u16, report: &[u8]) -> Result<(), String> {
        let mut body = Vec::with_capacity(3 + report.len());
        body.push(0x01);
        body.extend_from_slice(&pid.to_le_bytes());
        body.extend_from_slice(report);
        let mut frame = (body.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&body);
        self.w.write_all(&frame).map_err(|e| e.to_string())?;
        self.w.flush().map_err(|e| e.to_string())?;
        loop {
            let mut hdr = [0u8; 4];
            self.r.read_exact(&mut hdr).map_err(|e| e.to_string())?;
            let len = u32::from_le_bytes(hdr) as usize;
            let mut body = vec![0u8; len];
            self.r.read_exact(&mut body).map_err(|e| e.to_string())?;
            let kind = body[0];
            match kind {
                0x82 => return Ok(()), // ack
                0x84 => return Err(String::from_utf8_lossy(&body[3..]).into_owned()),
                _ => continue, // input events etc. may interleave
            }
        }
    }
}

/// "<name> pid=0x…. out_len=…" lines from `ds-vdevctl reports`.
fn module_pids(vdev_sock: &Path) -> HashMap<String, (u16, usize)> {
    let lines = ds_vdev::ctl::request(vdev_sock, "reports").expect("vdev reports");
    let mut map = HashMap::new();
    for line in lines {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        let name = toks[0].to_string();
        let pid = toks
            .iter()
            .find_map(|t| t.strip_prefix("pid="))
            .and_then(|h| u16::from_str_radix(h.trim_start_matches("0x"), 16).ok());
        let out_len = toks
            .iter()
            .find_map(|t| t.strip_prefix("out_len="))
            .and_then(|v| v.parse::<usize>().ok());
        if let (Some(p), Some(o)) = (pid, out_len) {
            map.insert(name, (p, o));
        }
    }
    assert!(
        map.contains_key("keys") && map.contains_key("dial"),
        "pids: {map:?}"
    );
    map
}

// ---------------------------------------------------------------------------
// the test stream sink: accepted DSF1 frames → device JPEG → chunked vdev reports

struct TestSink {
    link: Mutex<VdevLink>,
    pids: HashMap<String, (u16, usize)>,
    stats: Arc<Mutex<HashMap<String, Value>>>,
}

impl FrameSink for TestSink {
    fn on_frame(&self, target: &str, frame: &Frame) -> Result<(), IpcError> {
        let ft = FrameTarget::parse(target).ok_or_else(|| {
            IpcError::new(ErrorCode::ESchema, format!("bad stream target {target:?}"))
        })?;
        let jpeg = encode::encode_frame(ft, &frame.payload)
            .map_err(|e| IpcError::new(ErrorCode::EDevice, format!("encode: {e}")))?;
        let module = match ft {
            FrameTarget::Panel | FrameTarget::Key(_) => "keys",
            FrameTarget::Dial => "dial",
        };
        let (pid, out_len) = self.pids[module];
        let reports = chunk_reports(ft.cmd(), ft.index(), &jpeg, out_len);
        let mut link = self.link.lock().unwrap();
        for r in &reports {
            link.send_report(pid, r)
                .map_err(|e| IpcError::new(ErrorCode::EDevice, e))?;
        }
        drop(link);
        let mut st = self.stats.lock().unwrap();
        let e = st.entry(target.to_string()).or_insert_with(|| {
            json!({
                "sent": 0, "dropped": 0, "fps": 0.0, "bytes": 0, "queue": 0,
                "in_flight": false, "last_sha256": null, "last_seq": null,
                "writer_ms_p99": 0.0, "sampled_at": null,
            })
        });
        e["sent"] = json!(e["sent"].as_u64().unwrap_or(0) + 1);
        e["bytes"] = json!(e["bytes"].as_u64().unwrap_or(0) + jpeg.len() as u64);
        e["last_seq"] = json!(frame.meta.seq);
        e["last_sha256"] = json!(ds_vdev::sha256_hex(&jpeg));
        e["in_flight"] = json!(false);
        e["sampled_at"] = json!(super::source::now_ms().to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// the test control handler: telemetry reads (no rev, S1) + the stream ops

struct TestHandler {
    stats: Arc<Mutex<HashMap<String, Value>>>,
    /// false = behave like the current phase-1 daemon: stream-stats answers E_INTERNAL
    /// "not served", so the CLI must fall back to `get /telemetry/streams/<target>`.
    serve_stream_ops: bool,
}

impl Handler for TestHandler {
    fn handle(&self, _conn: &Connection, req: &Request) -> Response {
        let id = req.id();
        match req.op() {
            "ping" => Response::ok(id, json!({"pong": true})),
            "get" => {
                let path = req.get("path").and_then(Value::as_str).unwrap_or("");
                let st = self.stats.lock().unwrap();
                if path == "/telemetry/streams" {
                    // the whole telemetry plane (no rev, S1)
                    let all =
                        Value::Object(st.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
                    return Response::ok(id, json!({"path": path, "value": all}));
                }
                if let Some(t) = path.strip_prefix("/telemetry/streams/") {
                    return match st.get(t) {
                        Some(v) => Response::ok(id, json!({"path": path, "value": v})), // no rev (S1)
                        None => Response::error_code(
                            id,
                            ErrorCode::EPath,
                            format!("path not found: {path}"),
                        ),
                    };
                }
                Response::error_code(id, ErrorCode::EPath, format!("path not found: {path}"))
            }
            "stream-stats" if self.serve_stream_ops => {
                let t = req.get("target").and_then(Value::as_str).unwrap_or("");
                let st = self.stats.lock().unwrap();
                match st.get(t) {
                    Some(v) => Response::ok(id, json!({"target": t, "value": v})),
                    None => Response::error_code(
                        id,
                        ErrorCode::EPath,
                        format!("no stream stats for target '{t}' yet"),
                    ),
                }
            }
            "stream-open" if self.serve_stream_ops => {
                let t = req.get("target").and_then(Value::as_str).unwrap_or("");
                let c = req.get("credits").and_then(Value::as_u64).unwrap_or(0);
                Response::ok(id, json!({"target": t, "credits": c}))
            }
            "stream-stop" if self.serve_stream_ops => {
                let t = req.get("target").and_then(Value::as_str).unwrap_or("");
                Response::ok(id, json!({"target": t, "stopped": true}))
            }
            "stream-stats" | "stream-open" | "stream-stop" => Response::error_code(
                id,
                ErrorCode::EInternal,
                format!(
                    "op '{}' is not served by this phase-1 daemon (streams land in later tickets)",
                    req.op()
                ),
            ),
            other => Response::error_code(
                id,
                ErrorCode::ESchema,
                format!("op '{other}' is not served by the test handler"),
            ),
        }
    }
}

type BootStack = (
    PathBuf,
    Arc<Mutex<HashMap<String, Value>>>,
    Option<ds_vdev::VdevServer>,
);

fn boot_stack(tmp: &Path, serve_stream_ops: bool, with_stream_sock: bool) -> BootStack {
    let vdev_sock = tmp.join("vdev.sock");
    let vdev = ds_vdev::VdevServer::start(&vdev_sock, ds_vdev::HubConfig::default()).unwrap();
    let data = tmp.join("data");
    std::fs::create_dir_all(&data).unwrap();
    ds_daemon::lifecycle::ensure_token(&data).unwrap();
    let stats: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
    let handler = Arc::new(TestHandler {
        stats: stats.clone(),
        serve_stream_ops,
    });
    let server = Server::bind(&data.join("daemon.sock"), &data, ServerConfig::default()).unwrap();
    let _accept = server.start(handler).unwrap();
    if with_stream_sock {
        let pids = module_pids(&vdev_sock);
        let sink = Arc::new(TestSink {
            link: Mutex::new(VdevLink::connect(&vdev_sock).unwrap()),
            pids,
            stats: stats.clone(),
        });
        let ss = StreamServer::bind(
            &data.join("stream.sock"),
            StreamConfig {
                idle_timeout: None,
                ..StreamConfig::default()
            },
        )
        .unwrap();
        let _bus = ss.start(sink).unwrap();
    }
    (data.join("daemon.sock"), stats, Some(vdev))
}

// ---------------------------------------------------------------------------
// the test

#[test]
fn stream_verbs_end_to_end_against_vdev_and_stream_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let (sock, _stats, vdev) = boot_stack(tmp.path(), true, true);
    let vdev_sock = tmp.path().join("vdev.sock");
    let bin = ds_bin();
    let root = repo_root();
    let key_png = root.join("examples/key.png");
    assert!(
        key_png.is_file(),
        "golden example missing: {}",
        key_png.display()
    );
    let golden = std::fs::read_to_string(root.join("tests/golden/example-key0.sha256"))
        .unwrap()
        .trim()
        .to_string();
    let key_png_arg = key_png.to_str().unwrap().to_string();

    // -- 1. one-frame push to key:0 ----------------------------------------
    let out = run_ds(
        &bin,
        &sock,
        &["stream", "push", "key:0", "--file", &key_png_arg],
    );
    assert_eq!(
        code(&out),
        Some(0),
        "push rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("ok target=key:0 sent=1"),
        "{}",
        stdout(&out)
    );
    // the device received the golden re-encoded frame (sha via the vdev ctl, as
    // ds-vdevctl frames key:0 prints it)
    wait_until(
        || {
            ds_vdev::ctl::request(&vdev_sock, "frames key:0")
                .map(|ls| ls.join("\n").contains(&golden))
                .unwrap_or(false)
        },
        Duration::from_secs(10),
        "the golden encoded frame to be reassembled on the vdev",
    );

    // -- 2. stats --json: one object, no rev; human: one line per fact ------
    let out = run_ds(&bin, &sock, &["stream", "stats", "key:0", "--json"]);
    assert_eq!(
        code(&out),
        Some(0),
        "stats rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    let v: Value = serde_json::from_str(stdout(&out).trim()).expect("stats --json is one object");
    assert!(
        v.get("rev").is_none(),
        "telemetry stats never carry a rev (S1): {v}"
    );
    assert!(v["sent"].as_u64().unwrap() >= 1, "{v}");
    assert_eq!(v["last_sha256"].as_str(), Some(golden.as_str()), "{v}");
    let out = run_ds(&bin, &sock, &["stream", "stats", "key:0"]);
    assert_eq!(code(&out), Some(0));
    assert!(stdout(&out).contains("sent: 1"), "{}", stdout(&out));
    assert!(stdout(&out).contains("last_sha256: "), "{}", stdout(&out));

    // -- 3. stop -------------------------------------------------------------
    let out = run_ds(&bin, &sock, &["stream", "stop", "key:0"]);
    assert_eq!(
        code(&out),
        Some(0),
        "stop rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    assert!(stdout(&out).contains("stopped=true"), "{}", stdout(&out));
    // stop is idempotent: exit 0 again
    let out = run_ds(&bin, &sock, &["stream", "stop", "key:0", "--json"]);
    assert_eq!(code(&out), Some(0), "stderr={}", stderr(&out));

    // -- 4. bad target → exit 1 (S6: no panel/screen synonyms) --------------
    for bad in ["panel", "screen", "key:15"] {
        let out = run_ds(
            &bin,
            &sock,
            &["stream", "push", bad, "--file", &key_png_arg],
        );
        assert_eq!(code(&out), Some(1), "{bad}: rc={:?}", code(&out));
        assert!(
            stderr(&out).contains("invalid stream target"),
            "{bad}: {}",
            stderr(&out)
        );
    }
    // --ndjson on a stream verb → exit 1 (only watch takes --ndjson)
    let out = run_ds(&bin, &sock, &["stream", "stats", "key:0", "--ndjson"]);
    assert_eq!(code(&out), Some(1));
    assert!(
        stderr(&out).contains("--ndjson is only valid"),
        "{}",
        stderr(&out)
    );

    // -- 5. 10-frame --dir push at 30 fps: the latest frame wins -------------
    let dir = tmp.path().join("anim");
    std::fs::create_dir_all(&dir).unwrap();
    let mut payloads = Vec::new();
    for i in 0..10u8 {
        let jpg = encode::encode_solid(FrameTarget::Key(0), [10 + i * 20, 0x40, 0x80]);
        std::fs::write(dir.join(format!("{i:04}.jpg")), &jpg).unwrap();
        payloads.push(jpg);
    }
    let dir_arg = dir.to_str().unwrap().to_string();
    let t0 = Instant::now();
    let out = run_ds(
        &bin,
        &sock,
        &["stream", "push", "key:0", "--dir", &dir_arg, "--fps", "30"],
    );
    assert_eq!(
        code(&out),
        Some(0),
        "dir push rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    assert!(stdout(&out).contains("sent=10"), "{}", stdout(&out));
    assert!(stdout(&out).contains("fps=30"), "{}", stdout(&out));
    // 30 fps pacing: 10 frames take at least ~9 * 33 ms
    assert!(
        t0.elapsed() >= Duration::from_millis(280),
        "30 fps pacing: {:?}",
        t0.elapsed()
    );
    // the last pushed frame is the device's final frame (latest-frame-wins)
    let want = encode::encode_frame(FrameTarget::Key(0), &payloads[9]).unwrap();
    let want_sha = ds_vdev::sha256_hex(&want);
    wait_until(
        || {
            ds_vdev::ctl::request(&vdev_sock, "frames key:0")
                .map(|ls| ls.join("\n").contains(&want_sha))
                .unwrap_or(false)
        },
        Duration::from_secs(10),
        "the last pushed frame to be the final device frame",
    );
    // stats count every push: 1 (step 1) + 10 (step 5)
    let out = run_ds(&bin, &sock, &["stream", "stats", "key:0", "--json"]);
    assert_eq!(code(&out), Some(0), "stderr={}", stderr(&out));
    let v: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(v["sent"], json!(11), "{v}");
    assert_eq!(v["last_seq"], json!(9), "{v}");
    assert_eq!(v["last_sha256"].as_str(), Some(want_sha.as_str()), "{v}");

    // -- 6. a daemon without control stream ops: stats falls back to get ---
    let tmp2 = tempfile::tempdir().unwrap();
    let (sock2, stats2, vdev2) = boot_stack(tmp2.path(), false, true);
    // push one frame through the DSctl-only path (control stream-open answers
    // E_INTERNAL "not served", the session opens on stream.sock)
    let out = run_ds(
        &bin,
        &sock2,
        &["stream", "push", "key:0", "--file", &key_png_arg],
    );
    assert_eq!(
        code(&out),
        Some(0),
        "fallback push rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    {
        let st = stats2.lock().unwrap();
        assert_eq!(st["key:0"]["sent"], json!(1));
    }
    let out = run_ds(&bin, &sock2, &["stream", "stats", "key:0", "--json"]);
    assert_eq!(
        code(&out),
        Some(0),
        "stats fallback rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    let v: Value = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert_eq!(v["sent"], json!(1), "{v}");
    assert!(v.get("rev").is_none(), "{v}");
    // stop via the DSctl stream-stop fallback on stream.sock
    let out = run_ds(&bin, &sock2, &["stream", "stop", "key:0"]);
    assert_eq!(
        code(&out),
        Some(0),
        "stop fallback rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    if let Some(v) = vdev2 {
        v.shutdown();
    }

    // -- 7. missing stream.sock → exit 6 E_UNREACHABLE ----------------------
    let tmp3 = tempfile::tempdir().unwrap();
    let (sock3, _s, vdev3) = boot_stack(tmp3.path(), true, false); // control only
    let out = run_ds(
        &bin,
        &sock3,
        &["stream", "push", "keys", "--file", &key_png_arg],
    );
    assert_eq!(
        code(&out),
        Some(6),
        "rc={:?} stderr={}",
        code(&out),
        stderr(&out)
    );
    assert!(stderr(&out).contains("stream.sock"), "{}", stderr(&out));
    assert!(stderr(&out).contains("ds daemon start"), "{}", stderr(&out));
    // stats still works over the control socket alone (no stream needed)
    let out = run_ds(&bin, &sock3, &["stream", "stats", "keys"]);
    assert_eq!(
        code(&out),
        Some(2),
        "no stats yet → E_PATH exit 2: {}",
        stderr(&out)
    );
    // a completely missing daemon socket → exit 6 as well
    let missing = tmp3.path().join("nope/daemon.sock");
    let out = run_ds(&bin, &missing, &["stream", "stats", "keys"]);
    assert_eq!(code(&out), Some(6));
    assert!(stderr(&out).contains("ds daemon start"), "{}", stderr(&out));
    if let Some(v) = vdev3 {
        v.shutdown();
    }

    // -- 8. help at every stream level ---------------------------------------
    for args in [
        vec!["stream", "--help"],
        vec!["stream", "push", "--help"],
        vec!["stream", "stats", "--help"],
        vec!["stream", "stop", "--help"],
    ] {
        let out = run_ds(&bin, &sock, &args);
        assert_eq!(code(&out), Some(0), "{:?}", args);
        assert!(
            stdout(&out).contains("Usage:"),
            "{:?}: {}",
            args,
            stdout(&out)
        );
    }

    if let Some(v) = vdev {
        v.shutdown();
    }
}
