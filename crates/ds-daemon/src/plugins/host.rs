//! The plugin host proper (ticket DS-050): [`Host`] manages the plugins installed
//! under a base dir (`~/.local/share/deckstack/plugins/`), and each [`Plugin`] runs on
//! its own supervisor thread driving one sandboxed child process
//! ([`super::spawn`]) through the wire protocol ([`super::wire`]).
//!
//! Lifecycle (contract §4): `install → start (spawn, rlimits, Landlock, process group)
//! → register (manifest must match, else E_SCHEMA + kill) → events/invokes →
//! crash/hang → watchdog kill + respawn ≤ 3 per 10 min → disabled`. Daemon exit kills
//! every plugin process group ([`super::shutdown_all`]).
//!
//! - **Handshake**: `hello` is written immediately after spawn; the `register` reply
//!   must arrive within [`HANDSHAKE_TIMEOUT`] and its manifest must equal the on-disk
//!   manifest (schema-validated), else the child is killed (`E_SCHEMA`).
//! - **Invoke**: `id.action` with a JSON payload; args are validated against the
//!   action's `params` JSON Schema before anything is sent; the reply (`effects`) is
//!   awaited at most [`INVOKE_TIMEOUT`] (2 s). Timeout → `degraded` (stops receiving
//!   events); a second consecutive timeout → restart. A crash mid-call fails the
//!   pending invoke with `E_DEVICE`. The wait is bounded, so the router thread never
//!   blocks unboundedly.
//! - **Effects/requests**: every item is capability-checked against the manifest
//!   (`state.read|state.write|device.write|exec|net:<glob>|fs:<root>|emit`) —
//!   `set` effects may only touch `/controls/…` or the plugin's own `/plugins/<id>/…`
//!   and are committed through the state task (schema-validated on commit); invalid or
//!   ungranted items are rejected and counted in `/plugins/<id>/stats.violations`.
//!   At most [`MAX_EFFECTS_PER_INVOKE`] effects per invoke.
//! - **Network (S4/T11)**: `request kind:"net"` is host-mediated — the daemon parses
//!   the URL, matches the host against the plugin's `net:<glob>` **and**
//!   `/policy/net/allow_hosts`, resolves DNS and connects itself (plain `http://`,
//!   bounded URL/body/timeout). The plugin process itself gets no TCP rights under
//!   Landlock ABI ≥ 4 (defense in depth).
//! - **Logs**: plugin stderr (and any malformed stdout line) goes to the daemon log
//!   and a per-plugin ring buffer of the last [`LOG_RING_CAP`] lines
//!   ([`Host::logs`]) — never to the control plane. A crash publishes
//!   `plugin/<id>/crash` on the bus and a `warn` log line.
//! - **State**: lifecycle is exposed at `/plugins/<id>` (`{manifest, status, state,
//!   stats}` per `schemas/state.json`) through the state task — readable with the
//!   ordinary `get /plugins` op, and reflected in `daemon status` via `rev`.

use super::manifest::{self, Manifest};
use super::spawn::{self, Sandbox};
use super::wire;
use crate::lifecycle;
use crate::router::bus::{Bus, Event, FnSink};
use crate::router::{Pattern, TopicPattern};
use crate::state::StateHandle;
use ds_ipc::error::{ErrorCode, IpcError};
use ds_state::{Op, Transaction};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Deadline for the `register` reply after `hello` (handshake).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Bounded invoke wait (contract §5: plugin invoke ≤ 2 s, `ASK_SECONDS` precedent).
pub const INVOKE_TIMEOUT: Duration = Duration::from_secs(2);
/// Watchdog window: at most [`MAX_RESPAWNS`] respawns per this window, then disabled.
pub const RESPAWN_WINDOW: Duration = Duration::from_secs(600);
/// Maximum respawns per [`RESPAWN_WINDOW`] before the plugin is disabled.
pub const MAX_RESPAWNS: usize = 3;
/// Effect storm cap: at most this many effects per invoke (extras are rejected +
/// counted as violations).
pub const MAX_EFFECTS_PER_INVOKE: usize = 100;
/// Per-plugin log ring buffer capacity (last N lines).
pub const LOG_RING_CAP: usize = 200;
/// Max URL length accepted for a host-mediated `net` request.
pub const NET_URL_MAX: usize = 2048;
/// Max response body bytes returned to a plugin.
pub const NET_BODY_MAX: usize = 1 << 20;
/// Timeout of one host-mediated HTTP request.
pub const NET_TIMEOUT: Duration = Duration::from_secs(5);
/// Supervisor poll slice (stop/restart checks).
const SUPERVISOR_POLL: Duration = Duration::from_millis(100);

/// Plugin lifecycle status (the `status` enum of `schemas/state.json` `/plugins`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Installing,
    Running,
    Degraded,
    Stopped,
    Crashed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Installing => "installing",
            Status::Running => "running",
            Status::Degraded => "degraded",
            Status::Stopped => "stopped",
            Status::Crashed => "crashed",
        }
    }
}

/// `/plugins/<id>/stats` counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub restarts: u64,
    pub violations: u64,
    pub invokes: u64,
}

/// Control messages to a plugin's supervisor thread.
#[derive(Debug)]
enum Ctrl {
    Stop,
    Restart,
}

/// One line from the plugin's stdout (or EOF).
enum Msg {
    Line(String),
    Eof,
}

struct ChildSlot {
    pid: u32,
    stdin: Arc<Mutex<ChildStdin>>,
}

struct Inner {
    status: Status,
    child: Option<Arc<ChildSlot>>,
    stats: Stats,
    pending: HashMap<u64, Sender<Result<Value, IpcError>>>,
    restarts: VecDeque<Instant>,
    ring: VecDeque<String>,
    timeouts: u32,
    request_seq: u64,
    disabled_reason: Option<String>,
    last_error: Option<String>,
    landlock: String,
}

/// One plugin under supervision.
pub struct Plugin {
    host: Weak<Host>,
    id: String,
    dir: PathBuf,
    manifest: Manifest,
    command: Vec<String>,
    token: String,
    inner: Mutex<Inner>,
    ctrl_tx: Sender<Ctrl>,
    stop_requested: AtomicBool,
    /// Set by `remove` before the stop request: no status refresh may write
    /// `/plugins/<id>` again once removal has begun.
    removed: AtomicBool,
    sup: Mutex<Option<JoinHandle<()>>>,
}

impl Plugin {
    fn host(&self) -> Option<Arc<Host>> {
        self.host.upgrade()
    }

    fn ring_push(&self, inner: &mut Inner, line: String) {
        if inner.ring.len() >= LOG_RING_CAP {
            inner.ring.pop_front();
        }
        inner.ring.push_back(line);
    }

    /// Snapshot the `/plugins/<id>` state value (status/manifest/state/stats).
    fn state_value(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({
            "status": inner.status.as_str(),
            "manifest": self.manifest.to_json(),
            "state": {
                "pid": inner.child.as_ref().map(|c| c.pid),
                "landlock": inner.landlock,
                "disabled_reason": inner.disabled_reason,
                "last_error": inner.last_error,
                "timeouts": inner.timeouts,
            },
            "stats": {
                "restarts": inner.stats.restarts,
                "violations": inner.stats.violations,
                "invokes": inner.stats.invokes,
            },
        })
    }

    /// Push the current snapshot into the state document at `/plugins/<id>`. The
    /// plugin-owned `state` subtree (where effects write, e.g. `/plugins/<id>/state/…`)
    /// is preserved and merged with the host's own fields (pid, landlock, …).
    fn refresh_state(&self) {
        if self.removed.load(Ordering::SeqCst) {
            return;
        }
        let Some(host) = self.host() else { return };
        let path = format!("/plugins/{}", self.id);
        let preserved = host
            .state
            .get(&path, None)
            .ok()
            .and_then(|r| r.value.get("state").cloned());
        let mut value = self.state_value();
        let mut st = preserved.unwrap_or_else(|| json!({}));
        if let Some(obj) = st.as_object_mut() {
            if let Some(cur) = value["state"].as_object() {
                for (k, v) in cur {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        value["state"] = st;
        if let Err(e) = host.state.set(Transaction::new(vec![Op::set(path, value)])) {
            lifecycle::log(
                "warn",
                "ds-daemon::plugins",
                &format!("cannot write /plugins/{}: {e}", self.id),
            );
        }
    }

    fn set_status(&self, status: Status) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.status = status;
        }
        self.refresh_state();
    }

    fn fail_all_pending(&self, err: IpcError) {
        let pending: Vec<Sender<Result<Value, IpcError>>> = {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.drain().map(|(_, tx)| tx).collect()
        };
        for tx in pending {
            let _ = tx.send(Err(err.clone()));
        }
    }

    fn write_line(&self, msg: &Value) -> bool {
        let slot = {
            let inner = self.inner.lock().unwrap();
            inner.child.clone()
        };
        match slot {
            Some(slot) => {
                let mut stdin = slot.stdin.lock().unwrap();
                match stdin.write_all(wire::to_line(msg).as_bytes()) {
                    Ok(()) => stdin.flush().is_ok(),
                    Err(_) => false,
                }
            }
            None => false,
        }
    }

    /// Invoke one action with a bounded timeout. See module docs for the error mapping.
    fn invoke(&self, action: &str, args: &Value) -> Result<Value, IpcError> {
        // not-running wins over unknown action: E_DEVICE for a stopped/disabled/crashed
        // plugin, E_PATH only for a genuinely unknown action on a running plugin
        {
            let inner = self.inner.lock().unwrap();
            match inner.status {
                Status::Running | Status::Degraded => {}
                s => {
                    return Err(IpcError::new(
                        ErrorCode::EDevice,
                        format!(
                            "plugin '{}' is not running (status {}): invoke '{action}' refused",
                            self.id,
                            s.as_str()
                        ),
                    ))
                }
            }
        }
        let act = self.manifest.action(action).ok_or_else(|| {
            IpcError::new(
                ErrorCode::EPath,
                format!("plugin '{}' has no action '{action}'", self.id),
            )
        })?;
        validate_params(&act.params, args)?;
        let rid = {
            let mut inner = self.inner.lock().unwrap();
            inner.stats.invokes += 1;
            let rid = inner.request_seq;
            inner.request_seq += 1;
            let (tx, rx) = channel();
            inner.pending.insert(rid, tx);
            (rid, rx)
        };
        let (rid, rx) = rid;
        let msg = wire::invoke_message(rid, &self.id, action, args);
        if !self.write_line(&msg) {
            let mut inner = self.inner.lock().unwrap();
            inner.pending.remove(&rid);
            return Err(IpcError::new(
                ErrorCode::EDevice,
                format!("plugin '{}' stdin is gone (crashed?)", self.id),
            ));
        }
        match rx.recv_timeout(INVOKE_TIMEOUT) {
            Ok(res) => {
                let mut inner = self.inner.lock().unwrap();
                inner.timeouts = 0;
                drop(inner);
                self.refresh_state();
                res
            }
            Err(_) => {
                // Bounded timeout: degrade on the first, restart on the second
                // consecutive timeout (contract §5).
                let mut inner = self.inner.lock().unwrap();
                inner.pending.remove(&rid);
                inner.timeouts += 1;
                let n = inner.timeouts;
                if n >= 2 {
                    inner.status = Status::Installing;
                    inner.last_error = Some(format!(
                        "invoke '{action}' timed out after {} s (2nd consecutive): restarting",
                        INVOKE_TIMEOUT.as_secs()
                    ));
                    drop(inner);
                    let _ = self.ctrl_tx.send(Ctrl::Restart);
                    self.refresh_state();
                    Err(IpcError::new(
                        ErrorCode::ELimit,
                        format!(
                            "invoke '{}' on plugin '{}' timed out after {} s; second consecutive \
                             timeout -> plugin restart requested",
                            action,
                            self.id,
                            INVOKE_TIMEOUT.as_secs()
                        ),
                    ))
                } else {
                    inner.status = Status::Degraded;
                    inner.last_error = Some(format!(
                        "invoke '{action}' timed out after {} s: degraded (stops receiving events)",
                        INVOKE_TIMEOUT.as_secs()
                    ));
                    drop(inner);
                    self.refresh_state();
                    Err(IpcError::new(
                        ErrorCode::ELimit,
                        format!(
                            "invoke '{}' on plugin '{}' timed out after {} s: plugin marked degraded",
                            action,
                            self.id,
                            INVOKE_TIMEOUT.as_secs()
                        ),
                    ))
                }
            }
        }
    }

    /// Request a stop (kill the process group now; the supervisor thread exits).
    fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        let _ = self.ctrl_tx.send(Ctrl::Stop);
        let slot = {
            let mut inner = self.inner.lock().unwrap();
            inner.child.take()
        };
        if let Some(slot) = slot {
            spawn::kill_plugin_group(slot.pid);
        }
        self.fail_all_pending(IpcError::new(
            ErrorCode::EDevice,
            format!("plugin '{}' stopped", self.id),
        ));
        self.set_status(Status::Stopped);
    }
}

/// Validate invoke `args` against the action's `params` JSON Schema (`E_SCHEMA`).
fn validate_params(params: &Value, args: &Value) -> Result<(), IpcError> {
    let validator = jsonschema::validator_for(params).map_err(|e| {
        IpcError::new(
            ErrorCode::ESchema,
            format!("action params schema does not compile: {e}"),
        )
    })?;
    if let Err(err) = validator.validate(args) {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!("invoke args violate the action's params schema: {err}"),
        ));
    }
    Ok(())
}

/// The host: owns the plugins under a base dir.
pub struct Host {
    state: StateHandle,
    bus: Bus,
    dir: PathBuf,
    launcher: PathBuf,
    plugins: Mutex<HashMap<String, Arc<Plugin>>>,
}

impl Host {
    /// A host with the production launcher (`current_exe`) over `dir`.
    pub fn new(state: StateHandle, bus: Bus, dir: PathBuf) -> Arc<Host> {
        let launcher = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe"));
        Self::with_launcher(state, bus, dir, launcher)
    }

    /// A host with an explicit launcher binary (tests: the built `ds-daemon` bin).
    pub fn with_launcher(
        state: StateHandle,
        bus: Bus,
        dir: PathBuf,
        launcher: PathBuf,
    ) -> Arc<Host> {
        let host = Arc::new(Host {
            state,
            bus,
            dir,
            launcher,
            plugins: Mutex::new(HashMap::new()),
        });
        // Event fan-out: subscribe to every topic event and forward the mapped plugin
        // events to the running plugins that subscribed to them.
        let weak = Arc::downgrade(&host);
        let sink = Arc::new(FnSink(Arc::new(move |msg: &Value| {
            if let Some(host) = weak.upgrade() {
                host.on_bus_msg(msg);
            }
        })));
        let pat = Pattern::Topic(TopicPattern::parse("*").expect("'*' parses"));
        host.bus
            .subscribe(pat, None, "/*".to_string(), sink.clone());
        // state events (committed changes) feed the plugin event "state": a root
        // state-path pattern matches every Event::State on the bus
        let root = Pattern::State(ds_state::Path::parse("/").expect("'/' parses"));
        host.bus.subscribe(root, None, "/".to_string(), sink);
        host
    }

    /// The base dir plugins are installed under.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Does this host manage plugin `id` (any status)?
    pub fn has_plugin(&self, id: &str) -> bool {
        self.plugins.lock().unwrap().contains_key(id)
    }

    /// Start every plugin found under the base dir (each `<id>/manifest.json`).
    pub fn load_all(self: &Arc<Self>) -> Vec<(String, Result<(), IpcError>)> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() || !dir.join("manifest.json").is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().to_string();
            out.push((id.clone(), self.start(&id)));
        }
        out
    }

    /// Install a plugin from a source dir: validate its manifest, copy the tree to
    /// `<base>/<id>/`, then start it. Returns the plugin id.
    pub fn install(self: &Arc<Self>, src: &Path) -> Result<String, IpcError> {
        let manifest = manifest::read_manifest(src)?;
        let id = manifest.id.clone();
        let dest = self.dir.join(&id);
        if dest.exists() {
            return Err(IpcError::new(
                ErrorCode::EConflict,
                format!("plugin '{id}' is already installed at {}", dest.display()),
            ));
        }
        copy_dir(src, &dest).map_err(|e| {
            IpcError::new(
                ErrorCode::EInternal,
                format!("cannot copy {} -> {}: {e}", src.display(), dest.display()),
            )
        })?;
        self.start(&id)?;
        Ok(id)
    }

    /// Start (spawn + supervise) one installed plugin. The manifest is loaded and
    /// schema-validated synchronously (`E_PATH`/`E_SCHEMA`); the handshake completes
    /// asynchronously on the supervisor thread.
    pub fn start(self: &Arc<Self>, id: &str) -> Result<(), IpcError> {
        {
            let plugins = self.plugins.lock().unwrap();
            if plugins.contains_key(id) {
                return Err(IpcError::new(
                    ErrorCode::EConflict,
                    format!("plugin '{id}' is already managed by this host"),
                ));
            }
        }
        let dir = self.dir.join(id);
        let manifest = manifest::load_manifest(&dir)?;
        // resolve command[0] up front (no ambient PATH lookup at spawn time)
        let mut command = manifest.command.clone();
        command[0] = manifest::resolve_command0(&manifest.command)?
            .display()
            .to_string();
        let token = ds_ipc::auth::generate_token().map_err(|e| {
            IpcError::new(
                ErrorCode::EInternal,
                format!("cannot generate the plugin token: {e}"),
            )
        })?;
        let (ctrl_tx, ctrl_rx) = channel();
        let plugin = Arc::new(Plugin {
            host: Arc::downgrade(self),
            id: id.to_string(),
            dir,
            manifest,
            command,
            token,
            inner: Mutex::new(Inner {
                status: Status::Installing,
                child: None,
                stats: Stats::default(),
                pending: HashMap::new(),
                restarts: VecDeque::new(),
                ring: VecDeque::new(),
                timeouts: 0,
                request_seq: 1,
                disabled_reason: None,
                last_error: None,
                landlock: String::new(),
            }),
            ctrl_tx,
            stop_requested: AtomicBool::new(false),
            removed: AtomicBool::new(false),
            sup: Mutex::new(None),
        });
        self.plugins
            .lock()
            .unwrap()
            .insert(id.to_string(), plugin.clone());
        let host_weak = Arc::downgrade(self);
        let sup_plugin = plugin.clone();
        let handle = std::thread::Builder::new()
            .name(format!("ds-plugin-sup-{id}"))
            .spawn(move || run_supervisor(host_weak, sup_plugin, ctrl_rx))
            .map_err(|e| {
                IpcError::new(
                    ErrorCode::EInternal,
                    format!("cannot spawn the supervisor thread for '{id}': {e}"),
                )
            })?;
        *plugin.sup.lock().unwrap() = Some(handle);
        plugin.refresh_state();
        Ok(())
    }

    /// Invoke `id.action` (router seam entry).
    pub fn invoke(&self, id: &str, action: &str, args: &Value) -> Result<Value, IpcError> {
        let plugin = {
            let plugins = self.plugins.lock().unwrap();
            plugins.get(id).cloned()
        };
        match plugin {
            Some(p) => p.invoke(action, args),
            None => Err(IpcError::new(
                ErrorCode::EPath,
                format!(
                    "unknown plugin '{id}' (not installed under {})",
                    self.dir.display()
                ),
            )),
        }
    }

    /// Stop one plugin (kill its process group; status `stopped`).
    pub fn stop(&self, id: &str) {
        let plugin = self.plugins.lock().unwrap().get(id).cloned();
        if let Some(p) = plugin {
            p.request_stop();
        }
    }

    /// Remove one plugin: stop it, drop it from the host, delete its dir and its
    /// `/plugins/<id>` state entry.
    pub fn remove(&self, id: &str) -> Result<(), IpcError> {
        let plugin = self.plugins.lock().unwrap().remove(id);
        if let Some(p) = plugin {
            // Authoritative removal: mark first, then stop and drain the supervisor so
            // no late status update can re-create the state entry we delete below.
            p.removed.store(true, Ordering::SeqCst);
            p.request_stop();
            let handle = p.sup.lock().unwrap().take();
            if let Some(handle) = handle {
                let _ = handle.join();
            }
        }
        let dir = self.dir.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                IpcError::new(
                    ErrorCode::EInternal,
                    format!("cannot remove {}: {e}", dir.display()),
                )
            })?;
        }
        if let Err(e) = self
            .state
            .set(Transaction::new(vec![Op::delete(format!("/plugins/{id}"))]))
        {
            lifecycle::log(
                "warn",
                "ds-daemon::plugins",
                &format!("cannot delete /plugins/{id}: {e}"),
            );
        }
        Ok(())
    }

    /// `/plugins` view (in-memory, always fresh): `{id: {status, manifest, state, stats}}`.
    pub fn list(&self) -> Value {
        let plugins = self.plugins.lock().unwrap();
        let mut map = serde_json::Map::new();
        for (id, p) in plugins.iter() {
            map.insert(id.clone(), p.state_value());
        }
        Value::Object(map)
    }

    /// One plugin's state value (`/plugins/<id>`), or `E_PATH`.
    pub fn plugin_state(&self, id: &str) -> Result<Value, IpcError> {
        let plugins = self.plugins.lock().unwrap();
        plugins
            .get(id)
            .map(|p| p.state_value())
            .ok_or_else(|| IpcError::new(ErrorCode::EPath, format!("unknown plugin '{id}'")))
    }

    /// The per-plugin log ring buffer (last [`LOG_RING_CAP`] lines: stderr + malformed
    /// stdout). Never written to the control plane.
    pub fn logs(&self, id: &str) -> Vec<String> {
        let plugins = self.plugins.lock().unwrap();
        plugins
            .get(id)
            .map(|p| p.inner.lock().unwrap().ring.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Wait until the plugin reaches `Running` or a terminal status (tests/CLI).
    pub fn wait_ready(&self, id: &str, timeout: Duration) -> Result<Status, IpcError> {
        let deadline = Instant::now() + timeout;
        loop {
            let status = {
                let plugins = self.plugins.lock().unwrap();
                plugins.get(id).map(|p| p.inner.lock().unwrap().status)
            };
            match status {
                Some(Status::Running) => return Ok(Status::Running),
                Some(Status::Stopped) => return Ok(Status::Stopped),
                None => {
                    return Err(IpcError::new(
                        ErrorCode::EPath,
                        format!("unknown plugin '{id}'"),
                    ))
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                return Ok(status.unwrap_or(Status::Stopped));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Stop every plugin (daemon exit).
    pub fn stop_all(&self) {
        let plugins: Vec<Arc<Plugin>> = self.plugins.lock().unwrap().values().cloned().collect();
        for p in plugins {
            p.request_stop();
        }
    }

    /// Bus fan-out: map the event to a plugin event name and forward it to the running
    /// plugins that subscribed to it (degraded plugins stop receiving events).
    fn on_bus_msg(&self, msg: &Value) {
        let (event_name, line) = if let Some(topic) = msg.get("topic").and_then(Value::as_str) {
            match wire::topic_to_event(topic) {
                Some(name) => (
                    name,
                    wire::event_message(name, topic, msg.get("payload").unwrap_or(&Value::Null)),
                ),
                None => return,
            }
        } else if let Some(path) = msg.get("path").and_then(Value::as_str) {
            // state event -> plugin event "state"
            let mut l = json!({"op": "event", "event": "state", "path": path});
            if let Some(v) = msg.get("value") {
                l["value"] = v.clone();
            }
            ("state", l)
        } else {
            return;
        };
        let plugins: Vec<Arc<Plugin>> = self.plugins.lock().unwrap().values().cloned().collect();
        for p in plugins {
            let (subscribed, running) = {
                let inner = p.inner.lock().unwrap();
                (
                    p.manifest.events.iter().any(|e| e == event_name),
                    inner.status == Status::Running,
                )
            };
            if subscribed && running {
                let _ = p.write_line(&line);
            }
        }
    }
}

/// Recursively copy a small plugin dir.
fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dest.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

// ---- supervisor -------------------------------------------------------------

fn run_supervisor(host: Weak<Host>, plugin: Arc<Plugin>, ctrl_rx: Receiver<Ctrl>) {
    'respawn: loop {
        if plugin.stop_requested.load(Ordering::SeqCst) {
            return;
        }
        let Some(host) = host.upgrade() else { return };
        // Resolve the command + sandbox.
        let sandbox = Sandbox {
            limits: plugin.manifest.resources(),
            fs_roots: plugin.manifest.fs_roots.clone(),
            plugin_dir: plugin.dir.display().to_string(),
        };
        let spawned = spawn::spawn_plugin(
            &host.launcher,
            &plugin.command,
            &plugin.dir,
            &plugin.token,
            &sandbox,
        );
        let spawned = match spawned {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("spawn failed: {e}");
                lifecycle::log(
                    "error",
                    "ds-daemon::plugins",
                    &format!("plugin {}: {msg}", plugin.id),
                );
                {
                    let mut inner = plugin.inner.lock().unwrap();
                    inner.status = Status::Stopped;
                    inner.last_error = Some(msg);
                }
                plugin.refresh_state();
                plugin.fail_all_pending(IpcError::new(
                    ErrorCode::EDevice,
                    format!("plugin '{}' could not be spawned", plugin.id),
                ));
                return;
            }
        };
        let pid = spawned.pid;
        let stdin = Arc::new(Mutex::new(spawned.stdin));
        {
            let mut inner = plugin.inner.lock().unwrap();
            inner.child = Some(Arc::new(ChildSlot {
                pid,
                stdin: stdin.clone(),
            }));
            inner.status = Status::Installing;
        }
        plugin.refresh_state();
        lifecycle::log(
            "info",
            "ds-daemon::plugins",
            &format!("plugin '{}' spawned (pid {pid})", plugin.id),
        );
        // hello
        let hello = wire::hello_message(
            &plugin.token,
            &plugin.manifest.capabilities,
            &plugin.manifest.fs_roots,
            &plugin.manifest.resources(),
        );
        let hello_ok = {
            let mut guard = stdin.lock().unwrap();
            guard
                .write_all(wire::to_line(&hello).as_bytes())
                .and_then(|_| guard.flush())
                .is_ok()
        };
        if !hello_ok {
            handle_crash(&host, &plugin, "cannot write hello (child died at spawn)");
            if !should_respawn(&plugin) {
                return;
            }
            continue 'respawn;
        }
        // reader + stderr threads
        let (line_tx, line_rx) = channel::<Msg>();
        let reader_plugin = plugin.clone();
        let reader = std::thread::Builder::new()
            .name(format!("ds-plugin-read-{}", plugin.id))
            .spawn(move || {
                let mut reader = BufReader::new(spawned.stdout);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if line_tx.send(Msg::Line(line)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = line_tx.send(Msg::Eof);
                let _ = reader_plugin;
            });
        let stderr_plugin = plugin.clone();
        let stderr_thread = std::thread::Builder::new()
            .name(format!("ds-plugin-stderr-{}", plugin.id))
            .spawn(move || {
                let mut reader = BufReader::new(spawned.stderr);
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            let line = line.trim_end().to_string();
                            if line.is_empty() {
                                continue;
                            }
                            let mut inner = stderr_plugin.inner.lock().unwrap();
                            if line.contains("landlock") && inner.landlock.is_empty() {
                                inner.landlock = line.clone();
                            }
                            stderr_plugin.ring_push(&mut inner, line.clone());
                            drop(inner);
                            lifecycle::log(
                                "info",
                                &format!("ds-daemon::plugins::{} ", stderr_plugin.id),
                                &line,
                            );
                        }
                        Err(_) => break,
                    }
                }
            });
        let _ = (reader, stderr_thread);

        // Handshake: expect register within the deadline (other messages, e.g. early
        // `log` lines, are processed normally while waiting).
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        let handshake = loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break None;
            }
            match line_rx.recv_timeout(remaining) {
                Ok(Msg::Line(l)) => {
                    if let Some(m) = wire::parse_line(l.trim_end()) {
                        if m.get("op").and_then(Value::as_str) == Some("register") {
                            break Some(l);
                        }
                        handle_message(&host, &plugin, &m);
                    }
                }
                Ok(Msg::Eof) => break None,
                Err(_) => break None, // timeout
            }
        };
        match handshake {
            Some(line) => match check_register(&plugin, &line) {
                Ok(()) => {
                    let mut inner = plugin.inner.lock().unwrap();
                    inner.status = Status::Running;
                    drop(inner);
                    plugin.refresh_state();
                    lifecycle::log(
                        "info",
                        "ds-daemon::plugins",
                        &format!("plugin '{}' registered (pid {pid})", plugin.id),
                    );
                }
                Err(e) => {
                    lifecycle::log(
                        "warn",
                        "ds-daemon::plugins",
                        &format!(
                            "plugin '{}' register rejected: {e} (killing pid {pid})",
                            plugin.id
                        ),
                    );
                    spawn::kill_plugin_group(pid);
                    {
                        let mut inner = plugin.inner.lock().unwrap();
                        inner.child = None;
                        inner.last_error = Some(format!("register: {e}"));
                    }
                    plugin.refresh_state();
                    plugin.fail_all_pending(IpcError::new(
                        ErrorCode::EDevice,
                        format!("plugin '{}' died during handshake: {e}", plugin.id),
                    ));
                    handle_crash(&host, &plugin, &format!("register rejected: {e}"));
                    if !should_respawn(&plugin) {
                        return;
                    }
                    continue 'respawn;
                }
            },
            None => {
                spawn::kill_plugin_group(pid);
                {
                    let mut inner = plugin.inner.lock().unwrap();
                    inner.child = None;
                    inner.last_error = Some("handshake timeout/EOF before register".to_string());
                }
                plugin.refresh_state();
                plugin.fail_all_pending(IpcError::new(
                    ErrorCode::EDevice,
                    format!(
                        "plugin '{}' did not register within {} s",
                        plugin.id,
                        HANDSHAKE_TIMEOUT.as_secs()
                    ),
                ));
                handle_crash(&host, &plugin, "no register within handshake deadline");
                if !should_respawn(&plugin) {
                    return;
                }
                continue 'respawn;
            }
        }

        // Main loop.
        loop {
            // control messages first
            match ctrl_rx.try_recv() {
                Ok(Ctrl::Stop) | Err(_) => {
                    if plugin.stop_requested.load(Ordering::SeqCst) {
                        return;
                    }
                }
                Ok(Ctrl::Restart) => {
                    lifecycle::log(
                        "warn",
                        "ds-daemon::plugins",
                        &format!("plugin '{}' restart requested (invoke timeout)", plugin.id),
                    );
                    break;
                }
            }
            match line_rx.recv_timeout(SUPERVISOR_POLL) {
                Ok(Msg::Line(line)) => {
                    if let Some(msg) = wire::parse_line(line.trim_end()) {
                        handle_message(&host, &plugin, &msg);
                    } else {
                        let mut inner = plugin.inner.lock().unwrap();
                        inner.stats.violations += 1;
                        let v = inner.stats.violations;
                        plugin.ring_push(
                            &mut inner,
                            format!("malformed stdout line: {}", line.trim_end()),
                        );
                        drop(inner);
                        lifecycle::log(
                            "warn",
                            "ds-daemon::plugins",
                            &format!(
                                "plugin '{}': malformed wire line (violation #{v})",
                                plugin.id
                            ),
                        );
                        plugin.refresh_state();
                    }
                }
                Ok(Msg::Eof) => {
                    handle_crash(&host, &plugin, "stdout EOF (process exited)");
                    if !should_respawn(&plugin) {
                        return;
                    }
                    continue 'respawn;
                }
                Err(_) => {
                    if plugin.stop_requested.load(Ordering::SeqCst) {
                        return;
                    }
                }
            }
        }
        // Restart requested: count a respawn and respawn.
        {
            let mut inner = plugin.inner.lock().unwrap();
            inner.restarts.push_back(Instant::now());
            inner.stats.restarts += 1;
            inner.status = Status::Installing;
            inner.child = None;
        }
        spawn::kill_plugin_group(pid);
        plugin.refresh_state();
    }
}

/// Validate the `register` message: id must match, manifest must be schema-valid and
/// equal to the on-disk manifest (E_SCHEMA otherwise).
fn check_register(plugin: &Arc<Plugin>, line: &str) -> Result<(), IpcError> {
    let msg = wire::parse_line(line)
        .ok_or_else(|| IpcError::new(ErrorCode::ESchema, "register is not a wire message"))?;
    if msg.get("op").and_then(Value::as_str) != Some("register") {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!("expected op 'register', got {:?}", msg.get("op")),
        ));
    }
    let id = msg
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| IpcError::new(ErrorCode::ESchema, "register needs a string 'id'"))?;
    if id != plugin.id {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            format!(
                "register id {id:?} does not match the plugin dir id {:?}",
                plugin.id
            ),
        ));
    }
    let raw = msg
        .get("manifest")
        .cloned()
        .ok_or_else(|| IpcError::new(ErrorCode::ESchema, "register needs a 'manifest' object"))?;
    // schema-validate the registered manifest, then require semantic equality with the
    // on-disk manifest (key order / omitted optional fields do not matter)
    let registered = manifest::validate_manifest(&raw)?;
    if registered != plugin.manifest {
        return Err(IpcError::new(
            ErrorCode::ESchema,
            "register manifest does not match the on-disk manifest (schemas/plugin-manifest.json)",
        ));
    }
    Ok(())
}

/// Crash handling: status, pending invokes, bus event, log line, watchdog decision.
fn handle_crash(host: &Arc<Host>, plugin: &Arc<Plugin>, reason: &str) {
    // an intentional stop wins over the crash bookkeeping (the kill EOF races us)
    if plugin.stop_requested.load(Ordering::SeqCst) {
        plugin.set_status(Status::Stopped);
        plugin.fail_all_pending(IpcError::new(
            ErrorCode::EDevice,
            format!("plugin '{}' stopped", plugin.id),
        ));
        return;
    }
    {
        let mut inner = plugin.inner.lock().unwrap();
        inner.status = Status::Crashed;
        inner.child = None;
        inner.last_error = Some(reason.to_string());
    }
    plugin.refresh_state();
    plugin.fail_all_pending(IpcError::new(
        ErrorCode::EDevice,
        format!("plugin '{}' crashed mid-call: {reason}", plugin.id),
    ));
    // log event on crash
    lifecycle::log(
        "warn",
        "ds-daemon::plugins",
        &format!("plugin '{}' crashed: {reason}", plugin.id),
    );
    host.bus.publish(Event::topic(
        format!("plugin/{}/crash", plugin.id),
        json!({"plugin": plugin.id, "reason": reason}),
    ));
    // watchdog: prune the window, count the respawn
    let mut inner = plugin.inner.lock().unwrap();
    let cutoff = Instant::now() - RESPAWN_WINDOW;
    while inner.restarts.front().is_some_and(|t| *t < cutoff) {
        inner.restarts.pop_front();
    }
    if inner.restarts.len() >= MAX_RESPAWNS {
        inner.status = Status::Stopped;
        inner.disabled_reason = Some(format!(
            "watchdog: {MAX_RESPAWNS} respawns within {} s; plugin disabled",
            RESPAWN_WINDOW.as_secs()
        ));
        let reason_disabled = inner.disabled_reason.clone().unwrap();
        drop(inner);
        plugin.refresh_state();
        lifecycle::log(
            "error",
            "ds-daemon::plugins",
            &format!("plugin '{}' disabled: {reason_disabled}", plugin.id),
        );
        host.bus.publish(Event::topic(
            format!("plugin/{}/disabled", plugin.id),
            json!({"plugin": plugin.id, "reason": reason_disabled}),
        ));
    } else {
        inner.restarts.push_back(Instant::now());
        inner.stats.restarts += 1;
        drop(inner);
        plugin.refresh_state();
    }
}

/// After a crash: should the supervisor respawn (vs. stay disabled/stopped)?
fn should_respawn(plugin: &Arc<Plugin>) -> bool {
    !plugin.stop_requested.load(Ordering::SeqCst)
        && plugin.inner.lock().unwrap().status != Status::Stopped
}

/// Handle one parsed wire message from a running plugin.
fn handle_message(host: &Arc<Host>, plugin: &Arc<Plugin>, msg: &Value) {
    match msg.get("op").and_then(Value::as_str) {
        Some("effects") => handle_effects(host, plugin, msg),
        Some("request") => handle_request(host, plugin, msg),
        Some("log") => {
            if let Some(m) = msg.get("message").and_then(Value::as_str) {
                let mut inner = plugin.inner.lock().unwrap();
                plugin.ring_push(&mut inner, m.to_string());
                drop(inner);
                lifecycle::log("info", &format!("ds-daemon::plugins::{}", plugin.id), m);
            }
        }
        Some("reply") => { /* replies to host-initiated requests are not used */ }
        _ => {
            let mut inner = plugin.inner.lock().unwrap();
            inner.stats.violations += 1;
            drop(inner);
            lifecycle::log(
                "warn",
                "ds-daemon::plugins",
                &format!(
                    "plugin '{}': unknown op {:?} (protocol violation)",
                    plugin.id,
                    msg.get("op")
                ),
            );
            plugin.refresh_state();
        }
    }
}

/// Apply the `effects` of one invoke: capability-check + validate every item, commit
/// the `set` effects through the state task, publish `emit` effects, then reply to
/// the plugin and complete the pending invoke.
fn handle_effects(host: &Arc<Host>, plugin: &Arc<Plugin>, msg: &Value) {
    let rid = msg.get("request_id").and_then(Value::as_u64).unwrap_or(0);
    let empty = Vec::new();
    let effects = msg
        .get("effects")
        .and_then(Value::as_array)
        .unwrap_or(&empty)
        .clone();
    let (mut applied, mut rejected) = (0u64, 0u64);
    let mut violations = 0u64;
    let total = effects.len();
    for eff in effects.into_iter().take(MAX_EFFECTS_PER_INVOKE) {
        match apply_effect(host, plugin, &eff) {
            Ok(()) => applied += 1,
            Err(e) => {
                rejected += 1;
                violations += 1;
                lifecycle::log(
                    "warn",
                    "ds-daemon::plugins",
                    &format!(
                        "plugin '{}': effect rejected ({}): {:?}",
                        plugin.id,
                        e.code.as_str(),
                        eff
                    ),
                );
            }
        }
    }
    if total > MAX_EFFECTS_PER_INVOKE {
        violations += (total - MAX_EFFECTS_PER_INVOKE) as u64;
        rejected += (total - MAX_EFFECTS_PER_INVOKE) as u64;
    }
    if violations > 0 {
        let mut inner = plugin.inner.lock().unwrap();
        inner.stats.violations += violations;
        drop(inner);
        plugin.refresh_state();
    }
    // reply to the plugin + complete the pending invoke
    let reply = wire::reply_message(
        rid,
        rejected == 0,
        json!({"applied": applied, "rejected": rejected}),
    );
    let _ = plugin.write_line(&reply);
    let waiter = {
        let mut inner = plugin.inner.lock().unwrap();
        inner.pending.remove(&rid)
    };
    if let Some(tx) = waiter {
        let _ = tx.send(Ok(json!({
            "plugin": plugin.id,
            "applied": applied,
            "rejected": rejected,
        })));
    }
}

/// One effect: capability check + shape check + apply. `E_CAP` = ungranted/outside the
/// writable subset, `E_SCHEMA` = shape/state-schema violation.
fn apply_effect(host: &Arc<Host>, plugin: &Arc<Plugin>, eff: &Value) -> Result<(), IpcError> {
    let kind = eff
        .get("effect")
        .and_then(Value::as_str)
        .ok_or_else(|| IpcError::new(ErrorCode::ESchema, "effect needs a string 'effect'"))?;
    match kind {
        "set" => {
            if !plugin.manifest.has_cap("state.write") {
                return Err(IpcError::new(
                    ErrorCode::ECap,
                    "effect 'set' needs the state.write capability",
                ));
            }
            let path = eff
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new(ErrorCode::ESchema, "set effect needs a 'path'"))?;
            if !writable_path(&plugin.id, path) {
                return Err(IpcError::new(
                    ErrorCode::ECap,
                    format!(
                        "set effect path {path:?} is outside the plugin-writable subset \
                         (/controls/…, /plugins/{}/…)",
                        plugin.id
                    ),
                ));
            }
            let value = eff.get("value").cloned().unwrap_or(Value::Null);
            // The state task validates against schemas/state.json on commit.
            host.state
                .set(Transaction::new(vec![Op::set(path.to_string(), value)]))
                .map_err(|e| {
                    IpcError::new(
                        e.code,
                        format!("set effect on {path:?} rejected by the state schema: {e}"),
                    )
                })?;
            Ok(())
        }
        "emit" => {
            if !plugin.manifest.has_cap("emit") {
                return Err(IpcError::new(
                    ErrorCode::ECap,
                    "effect 'emit' needs the emit capability",
                ));
            }
            let topic = eff
                .get("topic")
                .and_then(Value::as_str)
                .ok_or_else(|| IpcError::new(ErrorCode::ESchema, "emit effect needs a 'topic'"))?;
            if !topic.starts_with(&format!("plugin/{}/", plugin.id)) {
                return Err(IpcError::new(
                    ErrorCode::ECap,
                    format!(
                        "emit effect topic {topic:?} must live under plugin/{}/",
                        plugin.id
                    ),
                ));
            }
            let payload = eff.get("payload").cloned().unwrap_or(Value::Null);
            host.bus.publish(Event::topic(topic.to_string(), payload));
            Ok(())
        }
        "log" => {
            if let Some(m) = eff.get("message").and_then(Value::as_str) {
                let mut inner = plugin.inner.lock().unwrap();
                plugin.ring_push(&mut inner, m.to_string());
                drop(inner);
                lifecycle::log("info", &format!("ds-daemon::plugins::{}", plugin.id), m);
            }
            Ok(())
        }
        other => Err(IpcError::new(
            ErrorCode::ESchema,
            format!("unknown effect kind {other:?} (set, emit, log)"),
        )),
    }
}

/// The state paths a plugin may write via effects/requests: `/controls/…` and its own
/// `/plugins/<id>/…` (validated against `schemas/state.json` on commit).
pub(crate) fn writable_path(plugin_id: &str, path: &str) -> bool {
    path.starts_with("/controls/") || path.starts_with(&format!("/plugins/{plugin_id}/"))
}

/// Handle a plugin→host `request` (capability-gated).
fn handle_request(host: &Arc<Host>, plugin: &Arc<Plugin>, msg: &Value) {
    let rid = msg.get("request_id").and_then(Value::as_u64).unwrap_or(0);
    let kind = msg.get("kind").and_then(Value::as_str).unwrap_or("");
    let reply = match kind {
        "get" => {
            if !plugin.manifest.has_cap("state.read") {
                violation(plugin, "request kind 'get' needs state.read");
                wire::reply_message(
                    rid,
                    false,
                    json!({"code": "E_CAP", "message": "get needs state.read"}),
                )
            } else {
                let path = msg.get("path").and_then(Value::as_str).unwrap_or("");
                match host.state.get(path, None) {
                    Ok(r) => {
                        wire::reply_message(rid, true, json!({"value": r.value, "rev": r.rev}))
                    }
                    Err(e) => wire::reply_message(
                        rid,
                        false,
                        json!({"code": e.code.as_str(), "message": e.message}),
                    ),
                }
            }
        }
        "set" => {
            if !plugin.manifest.has_cap("state.write") {
                violation(plugin, "request kind 'set' needs state.write");
                wire::reply_message(rid, false, json!({"code": "E_CAP"}))
            } else {
                let path = msg.get("path").and_then(Value::as_str).unwrap_or("");
                if !writable_path(&plugin.id, path) {
                    violation(
                        plugin,
                        &format!("request set {path:?} outside the writable subset"),
                    );
                    wire::reply_message(
                        rid,
                        false,
                        json!({"code": "E_CAP", "message": "path outside /controls/… or /plugins/<id>/…"}),
                    )
                } else {
                    let value = msg.get("value").cloned().unwrap_or(Value::Null);
                    match host
                        .state
                        .set(Transaction::new(vec![Op::set(path.to_string(), value)]))
                    {
                        Ok(rev) => wire::reply_message(rid, true, json!({"rev": rev})),
                        Err(e) => {
                            violation(plugin, &format!("request set {path:?} rejected: {e}"));
                            wire::reply_message(
                                rid,
                                false,
                                json!({"code": e.code.as_str(), "message": e.message}),
                            )
                        }
                    }
                }
            }
        }
        "emit" => {
            if !plugin.manifest.has_cap("emit") {
                violation(plugin, "request kind 'emit' needs emit");
                wire::reply_message(rid, false, json!({"code": "E_CAP"}))
            } else {
                let topic = msg.get("topic").and_then(Value::as_str).unwrap_or("");
                if !topic.starts_with(&format!("plugin/{}/", plugin.id)) {
                    violation(
                        plugin,
                        &format!("request emit {topic:?} outside plugin/{}/…", plugin.id),
                    );
                    wire::reply_message(rid, false, json!({"code": "E_CAP"}))
                } else {
                    let payload = msg.get("payload").cloned().unwrap_or(Value::Null);
                    host.bus.publish(Event::topic(topic.to_string(), payload));
                    wire::reply_message(rid, true, json!({}))
                }
            }
        }
        "log" => {
            if let Some(m) = msg.get("message").and_then(Value::as_str) {
                let mut inner = plugin.inner.lock().unwrap();
                plugin.ring_push(&mut inner, m.to_string());
                drop(inner);
                lifecycle::log("info", &format!("ds-daemon::plugins::{}", plugin.id), m);
            }
            wire::reply_message(rid, true, json!({}))
        }
        "fs" => {
            let path = msg.get("path").and_then(Value::as_str).unwrap_or("");
            if plugin.manifest.fs_cap_covers(path) {
                wire::reply_message(rid, true, json!({}))
            } else {
                violation(
                    plugin,
                    &format!("request fs {path:?} outside fs:<root> caps"),
                );
                wire::reply_message(
                    rid,
                    false,
                    json!({"code": "E_CAP", "message": "no fs:<root> capability covers the path"}),
                )
            }
        }
        "net" => return handle_net_request(host, plugin, msg, rid),
        other => {
            violation(plugin, &format!("unknown request kind {other:?}"));
            wire::reply_message(
                rid,
                false,
                json!({"code": "E_SCHEMA", "message": format!("unknown request kind {other:?}")}),
            )
        }
    };
    let _ = plugin.write_line(&reply);
}

fn violation(plugin: &Arc<Plugin>, msg: &str) {
    let mut inner = plugin.inner.lock().unwrap();
    inner.stats.violations += 1;
    plugin.ring_push(&mut inner, format!("violation: {msg}"));
    let v = inner.stats.violations;
    drop(inner);
    lifecycle::log(
        "warn",
        "ds-daemon::plugins",
        &format!("plugin '{}': {msg} (violation #{v})", plugin.id),
    );
    plugin.refresh_state();
}

/// Host-mediated network (S4/T11): the plugin never opens a socket — the daemon
/// parses the URL, matches the host against the plugin's `net:<glob>` AND
/// `/policy/net/allow_hosts`, resolves DNS and performs the (plain http) request
/// itself on a helper thread, then replies.
fn handle_net_request(host: &Arc<Host>, plugin: &Arc<Plugin>, msg: &Value, rid: u64) {
    let url = msg.get("url").and_then(Value::as_str).unwrap_or("");
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("GET")
        .to_ascii_uppercase();
    if url.len() > NET_URL_MAX {
        violation(plugin, "net request URL over NET_URL_MAX");
        let _ = plugin.write_line(&wire::reply_message(
            rid,
            false,
            json!({"code": "E_LIMIT", "message": format!("URL longer than {NET_URL_MAX} bytes")}),
        ));
        return;
    }
    let parsed = match parse_http_url(url) {
        Some(p) => p,
        None => {
            violation(
                plugin,
                &format!("net request URL {url:?} is not a plain http:// URL"),
            );
            let _ = plugin.write_line(&wire::reply_message(
                rid,
                false,
                json!({"code": "E_SCHEMA", "message": "only plain http:// URLs are supported"}),
            ));
            return;
        }
    };
    let host_str = parsed.host.to_ascii_lowercase();
    // 1) the plugin's own net:<glob> grant
    if !plugin.manifest.net_cap_matches(&host_str) {
        violation(
            plugin,
            &format!("net request to {host_str:?} matches no net:<glob> capability"),
        );
        let _ = plugin.write_line(&wire::reply_message(
            rid,
            false,
            json!({"code": "E_CAP", "message": format!("host {host_str:?} is not granted by any net:<glob>")}),
        ));
        return;
    }
    // 2) the admin policy /policy/net/allow_hosts
    let allowed = host
        .state
        .get("/policy/net/allow_hosts", None)
        .ok()
        .and_then(|r| r.value.as_array().cloned())
        .unwrap_or_default();
    let host_allowed = allowed
        .iter()
        .filter_map(Value::as_str)
        .any(|h| h.to_ascii_lowercase() == host_str);
    if !host_allowed {
        violation(
            plugin,
            &format!("net request to {host_str:?} is not in /policy/net/allow_hosts"),
        );
        let _ = plugin.write_line(&wire::reply_message(
            rid,
            false,
            json!({"code": "E_CAP", "message": format!("host {host_str:?} is not in /policy/net/allow_hosts")}),
        ));
        return;
    }
    // 3) the daemon performs the request itself (DNS resolved here, after the host
    // string was re-checked against the glob).
    let plugin_id = plugin.id.clone();
    let stdin_slot = {
        let inner = plugin.inner.lock().unwrap();
        inner.child.clone()
    };
    let url_owned = url.to_string();
    std::thread::Builder::new()
        .name(format!("ds-plugin-net-{plugin_id}"))
        .spawn(move || {
            let result = http_fetch(&method, &url_owned);
            let reply = match result {
                Ok((status, body)) => {
                    wire::reply_message(rid, true, json!({"status": status, "body": body}))
                }
                Err(e) => {
                    wire::reply_message(rid, false, json!({"code": "E_INTERNAL", "message": e}))
                }
            };
            if let Some(slot) = stdin_slot {
                let mut guard = slot.stdin.lock().unwrap();
                let _ = guard.write_all(wire::to_line(&reply).as_bytes());
                let _ = guard.flush();
            }
        })
        .ok();
}

/// Parsed plain `http://host[:port][/path]` URL.
pub(crate) struct HttpUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub(crate) fn parse_http_url(url: &str) -> Option<HttpUrl> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        // [v6]:port
        let end = h.find(']')?;
        let host = &h[..end];
        let port = match h[end + 1..].strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None => 80,
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), p.parse().ok()?)
            }
            _ => (authority.to_string(), 80),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some(HttpUrl {
        host,
        port,
        path: path.to_string(),
    })
}

/// Minimal host-side HTTP/1.1 fetch (plain http only): resolve, connect with a
/// timeout, send the request, read the response (body capped at [`NET_BODY_MAX`]).
pub(crate) fn http_fetch(method: &str, url: &str) -> Result<(u16, String), String> {
    let parsed = parse_http_url(url).ok_or("bad URL")?;
    if method != "GET" && method != "HEAD" {
        return Err(format!("unsupported method {method:?} (GET/HEAD only)"));
    }
    let addrs: Vec<std::net::SocketAddr> = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolve {}: {e}", parsed.host))?
        .collect();
    let addr = addrs
        .first()
        .copied()
        .ok_or_else(|| format!("DNS resolved to no address: {}", parsed.host))?;
    let mut stream = TcpStream::connect_timeout(&addr, NET_TIMEOUT)
        .map_err(|e| format!("connect {addr}: {e}"))?;
    stream
        .set_read_timeout(Some(NET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(NET_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let request = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: */*\r\n\r\n",
        parsed.path, parsed.host
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > NET_BODY_MAX + 65536 {
                    return Err(format!("response exceeds {} bytes", NET_BODY_MAX + 65536));
                }
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    let text = String::from_utf8_lossy(&raw).to_string();
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let status_line = head.lines().next().unwrap_or("");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed status line {status_line:?}"))?;
    let body = if method == "HEAD" {
        String::new()
    } else {
        body.chars().take(NET_BODY_MAX).collect()
    };
    Ok((code, body))
}
