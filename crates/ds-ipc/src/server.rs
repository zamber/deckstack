//! The control-plane server: bind a Unix socket (dir 0700, socket 0600), accept up to
//! [`MAX_CONNECTIONS`](crate::limits::MAX_CONNECTIONS) connections, and per connection
//! run the NDJSON request/response loop with the `hello` → `auth` → ops state machine,
//! scope enforcement, and the documented limits (`docs/contracts/ipc.md` §2–§5).
//!
//! The crate stays transport-only: after the built-in `hello`/`auth` handshake, every
//! other op is dispatched to the user's [`Handler`]. The daemon (DS-015) supplies the
//! handler that owns state; tests use [`crate::testing::MockHandler`].

use crate::auth::{Scope, TokenSet};
use crate::error::{ErrorCode, IpcError};
use crate::framing::{self, FrameError};
use crate::limits::ServerConfig;
use crate::message::{Request, Response};
use crate::paths;
use serde_json::{json, Value};
use std::fs;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// The scope an op requires (the table in `docs/contracts/ipc.md` §3). `hello` and
/// `auth` return `None` — they are the handshake itself and need no prior scope.
pub fn required_scope(req: &Request) -> Option<Scope> {
    let action = req.get("action").and_then(Value::as_str);
    match req.op() {
        "hello" | "auth" => None,
        "ping" | "get" | "subscribe" | "unsubscribe" | "stream-stats" | "schema" => {
            Some(Scope::Read)
        }
        "set" | "emit" | "bind" | "unbind" | "stream-open" | "stream-stop" => Some(Scope::Mutate),
        "plugin" => match action {
            Some("list") => Some(Scope::Read),
            _ => Some(Scope::Mutate),
        },
        "daemon" => match action {
            Some("status") => Some(Scope::Read),
            _ => Some(Scope::Admin),
        },
        "token" => match action {
            Some("list") => Some(Scope::Read),
            _ => Some(Scope::Admin),
        },
        // Unknown ops are rejected by schema validation before dispatch; fail closed.
        _ => Some(Scope::Admin),
    }
}

/// A live connection handle. Cloneable and `Send + Sync`: a [`Handler`] may keep a clone
/// to push async server messages (events) with [`Connection::send_event`].
#[derive(Clone)]
pub struct Connection {
    inner: Arc<ConnInner>,
}

struct ConnInner {
    id: u64,
    writer: Mutex<std::io::BufWriter<UnixStream>>,
    timeout_stream: UnixStream,
    generation: u64,
    scope: Mutex<Option<Scope>>,
    token: Mutex<String>,
    max_response_bytes: usize,
    idle_timeout: Option<Duration>,
}

impl Connection {
    /// Connection id (monotonic per server).
    pub fn id(&self) -> u64 {
        self.inner.id
    }

    /// The token-set generation this connection authenticated under. The generation is
    /// bumped on every rotation; the ≤ 5 s in-flight grace (S12/M4) is measured against
    /// it (see [`Server::generation`]).
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// The scope granted by `auth` (`None` before auth).
    pub fn scope(&self) -> Option<Scope> {
        *self.inner.scope.lock().unwrap()
    }

    /// Mark the connection exempt from the idle timeout (subscriptions, §5).
    pub fn set_idle_exempt(&self, exempt: bool) {
        let t = if exempt {
            None
        } else {
            self.inner.idle_timeout
        };
        let _ = self.inner.timeout_stream.set_read_timeout(t);
    }

    /// Send one async server message (an `event`/`dropped`/`stream-*` object — no `ok`,
    /// no `id`) on this connection. Schema-validated and size-capped like any response.
    pub fn send_event(&self, value: &Value) -> Result<(), IpcError> {
        crate::message::validate_server_message(value)?;
        send_raw(self, value)
    }
}

fn send_raw(conn: &Connection, value: &Value) -> Result<(), IpcError> {
    let mut w = conn.inner.writer.lock().unwrap();
    framing::write_line(&mut *w, value, conn.inner.max_response_bytes)
        .map_err(|e| e.to_ipc_error())?;
    Ok(())
}

/// The daemon-side op handler. `hello` and `auth` are answered by the server itself;
/// everything else arrives here, already authenticated and scope-checked.
pub trait Handler: Send + Sync + 'static {
    /// Current revision, reported in the `welcome` result (`rev` field).
    fn rev(&self) -> u64 {
        0
    }

    /// Answer one post-auth request. The server overwrites the response's `id` with the
    /// request's id before sending.
    fn handle(&self, conn: &Connection, req: &Request) -> Response;
}

/// Cached token set with a rotation generation counter. The token files are re-read
/// whenever their (mtime, len) signature changes — and at least once per new connection —
/// so revocation is instant for new connections (S12/M4, `docs/contracts/ipc.md` §3).
struct TokenCache {
    set: Option<TokenSet>,
    error: Option<String>,
    sig: Option<Sig>,
    generation: u64,
    rotated_at: Option<Instant>,
}

fn file_sig(path: &Path) -> Option<(SystemTime, u64)> {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok().map(|t| (t, m.len())))
}

type Sig = (Option<(SystemTime, u64)>, Option<(SystemTime, u64)>);

impl TokenCache {
    fn new() -> Self {
        // `sig: None` forces a load on the first refresh (the files may already exist).
        Self {
            set: None,
            error: None,
            sig: None,
            generation: 0,
            rotated_at: None,
        }
    }

    /// Re-read the token files if their signature changed; bumps the generation and
    /// records the rotation instant. Fails closed: an unreadable/insecure token file
    /// leaves `set = None`, so nobody authenticates.
    fn refresh(&mut self, dir: &Path) {
        let sig: Sig = (
            file_sig(&dir.join("token")),
            file_sig(&dir.join("tokens.json")),
        );
        if self.sig.as_ref() == Some(&sig) {
            return;
        }
        self.sig = Some(sig);
        self.generation += 1;
        self.rotated_at = Some(Instant::now());
        match TokenSet::load(dir) {
            Ok(set) => {
                self.set = Some(set);
                self.error = None;
            }
            Err(e) => {
                self.set = None;
                self.error = Some(e.to_string());
            }
        }
    }
}

/// A bound control-plane server. Call [`Server::start`] (background accept loop) or
/// [`Server::serve`] (blocking).
pub struct Server {
    listener: UnixListener,
    socket_path: PathBuf,
    config: ServerConfig,
    token_dir: PathBuf,
    cache: Arc<Mutex<TokenCache>>,
    live: Arc<AtomicUsize>,
    next_conn_id: Arc<AtomicU64>,
}

impl Server {
    /// Bind `socket_path` (parent dir created 0700, socket 0600). `token_dir` holds
    /// `token` + `tokens.json`. A stale socket file (no listener behind it) is removed;
    /// a live one is an error.
    pub fn bind(
        socket_path: &Path,
        token_dir: &Path,
        config: ServerConfig,
    ) -> std::io::Result<Self> {
        if let Some(parent) = socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
        if socket_path.exists() {
            match UnixStream::connect(socket_path) {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("a daemon is already listening on {}", socket_path.display()),
                    ))
                }
                Err(_) => fs::remove_file(socket_path)?,
            }
        }
        let listener = UnixListener::bind(socket_path)?;
        fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            config,
            token_dir: token_dir.to_path_buf(),
            cache: Arc::new(Mutex::new(TokenCache::new())),
            live: Arc::new(AtomicUsize::new(0)),
            next_conn_id: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Bind the default socket (`$XDG_RUNTIME_DIR/deckstack/daemon.sock`, fallback
    /// `~/.local/state/deckstack/daemon.sock`) with the default config and the default
    /// token dir.
    pub fn bind_default() -> std::io::Result<Self> {
        let dir = paths::default_dir();
        Self::bind(&dir.join("daemon.sock"), &dir, ServerConfig::default())
    }

    /// The bound socket path.
    pub fn local_addr(&self) -> &Path {
        &self.socket_path
    }

    /// The token-set generation (rotation counter).
    pub fn generation(&self) -> u64 {
        self.cache.lock().unwrap().generation
    }

    /// Spawn the accept loop on a background thread and return its handle.
    pub fn start<H: Handler>(self, handler: Arc<H>) -> std::io::Result<thread::JoinHandle<()>> {
        let listener = self.listener.try_clone()?;
        let config = self.config.clone();
        let token_dir = self.token_dir.clone();
        let cache = Arc::clone(&self.cache);
        let live = Arc::clone(&self.live);
        let next_conn_id = Arc::clone(&self.next_conn_id);
        let handle = thread::Builder::new()
            .name("ds-ipc-accept".into())
            .spawn(move || {
                accept_loop(
                    listener,
                    config,
                    token_dir,
                    cache,
                    live,
                    next_conn_id,
                    handler,
                );
            })?;
        Ok(handle)
    }

    /// Run the accept loop on this thread (blocking until the listener errors out).
    pub fn serve<H: Handler>(self, handler: Arc<H>) {
        accept_loop(
            self.listener,
            self.config,
            self.token_dir,
            self.cache,
            self.live,
            self.next_conn_id,
            handler,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn accept_loop<H: Handler>(
    listener: UnixListener,
    config: ServerConfig,
    token_dir: PathBuf,
    cache: Arc<Mutex<TokenCache>>,
    live: Arc<AtomicUsize>,
    next_conn_id: Arc<AtomicU64>,
    handler: Arc<H>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if live.fetch_add(1, Ordering::SeqCst) >= config.max_connections {
                    // At the connection cap (§5): refuse; the client sees EOF.
                    live.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                let config = config.clone();
                let token_dir = token_dir.clone();
                let cache = Arc::clone(&cache);
                let live = Arc::clone(&live);
                let id = next_conn_id.fetch_add(1, Ordering::SeqCst);
                let handler = Arc::clone(&handler);
                let _ = thread::Builder::new()
                    .name(format!("ds-ipc-conn-{id}"))
                    .spawn(move || {
                        handle_connection(stream, id, config, token_dir, cache, handler);
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
            }
            Err(e) => {
                eprintln!("ds-ipc: accept error: {e}");
            }
        }
    }
}

fn handle_connection<H: Handler>(
    stream: UnixStream,
    id: u64,
    config: ServerConfig,
    token_dir: PathBuf,
    cache: Arc<Mutex<TokenCache>>,
    handler: Arc<H>,
) {
    // Instant revocation for new connections: refresh the token set before anything else.
    cache.lock().unwrap().refresh(&token_dir);
    let generation = cache.lock().unwrap().generation;

    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let timeout_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    if let Some(t) = config.idle_timeout {
        let _ = stream.set_read_timeout(Some(t));
    }
    let mut reader = BufReader::new(reader_stream);

    let conn = Connection {
        inner: Arc::new(ConnInner {
            id,
            writer: Mutex::new(std::io::BufWriter::new(stream)),
            timeout_stream,
            generation,
            scope: Mutex::new(None),
            token: Mutex::new(String::new()),
            max_response_bytes: config.max_response_bytes,
            idle_timeout: config.idle_timeout,
        }),
    };

    let mut welcomed = false;
    loop {
        match framing::read_line(&mut reader, config.max_line_bytes) {
            Err(FrameError::Eof) => break,
            Err(FrameError::TooLong) => {
                // §2/§5: request line over the 1 MiB cap → id-less E_LIMIT, then close
                // (the framing can no longer be trusted).
                let value = json!({
                    "ok": false,
                    "error": FrameError::TooLong.to_ipc_error().to_value(),
                });
                let _ = send_raw(&conn, &value);
                break;
            }
            Err(FrameError::Io(e)) => {
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut
                {
                    // Idle timeout (§5; subscriptions exempt via set_idle_exempt).
                    break;
                }
                let value = json!({
                    "ok": false,
                    "error": FrameError::Io(e).to_ipc_error().to_value(),
                });
                let _ = send_raw(&conn, &value);
                break;
            }
            Ok(line) => {
                if line.trim().is_empty() {
                    continue;
                }
                let req = match Request::parse(&line) {
                    Ok(r) => r,
                    Err(err) => {
                        let id = serde_json::from_str::<Value>(line.trim())
                            .ok()
                            .and_then(|v| v.get("id").and_then(Value::as_i64));
                        let resp = Response::error(id, err);
                        if send_response(&conn, req_id(id, resp), &config).is_err() {
                            break;
                        }
                        continue;
                    }
                };
                let resp = dispatch(
                    &conn,
                    &req,
                    &config,
                    &token_dir,
                    &cache,
                    &handler,
                    &mut welcomed,
                );
                let fatal = resp
                    .error
                    .as_ref()
                    .is_some_and(|e| e.code == ErrorCode::EVersion);
                if send_response(&conn, req_id(req.id(), resp), &config).is_err() {
                    break;
                }
                if fatal {
                    break;
                }
            }
        }
    }
}

/// Force the correlation id onto a response (the server owns it).
fn req_id(id: Option<i64>, mut resp: Response) -> Response {
    resp.id = id;
    resp
}

/// The hello/auth/ops state machine. An `E_VERSION` response is fatal: the connection is
/// closed after it is sent.
fn dispatch<H: Handler>(
    conn: &Connection,
    req: &Request,
    config: &ServerConfig,
    token_dir: &Path,
    cache: &Arc<Mutex<TokenCache>>,
    handler: &Arc<H>,
    welcomed: &mut bool,
) -> Response {
    let id = req.id();
    if !*welcomed {
        if req.op() != "hello" {
            return Response::error_code(
                id,
                ErrorCode::EVersion,
                "first request on a connection must be hello",
            );
        }
        let protocol = req.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if protocol != u64::from(crate::PROTOCOL_VERSION) {
            return Response::error_code(
                id,
                ErrorCode::EVersion,
                format!(
                    "protocol {protocol} not supported; this server speaks protocol {}",
                    crate::PROTOCOL_VERSION
                ),
            );
        }
        *welcomed = true;
        return Response::ok(
            id,
            json!({
                "op": "welcome",
                "protocol": crate::PROTOCOL_VERSION,
                "rev": handler.rev(),
                "server": config.server_name,
            }),
        );
    }
    match req.op() {
        "hello" => Response::error_code(id, ErrorCode::EVersion, "hello already done"),
        "auth" => {
            let mut c = cache.lock().unwrap();
            c.refresh(token_dir);
            let result = match (&c.set, &c.error) {
                (Some(set), _) => {
                    let token = req.get("token").and_then(Value::as_str).unwrap_or("");
                    let requested = req
                        .get("scope")
                        .and_then(Value::as_str)
                        .and_then(Scope::parse);
                    set.authenticate(token, requested)
                }
                (None, Some(e)) => Err(IpcError::new(
                    ErrorCode::EAuth,
                    format!("token set unavailable: {e}"),
                )),
                (None, None) => Err(IpcError::new(ErrorCode::EAuth, "token set unavailable")),
            };
            match result {
                Ok(scope) => {
                    let token = req.get("token").and_then(Value::as_str).unwrap_or("");
                    *conn.inner.scope.lock().unwrap() = Some(scope);
                    *conn.inner.token.lock().unwrap() = token.to_string();
                    Response::ok(id, json!({"scope": scope.as_str()}))
                }
                Err(e) => Response::error(id, e),
            }
        }
        op => {
            let Some(granted) = conn.scope() else {
                return Response::error_code(
                    id,
                    ErrorCode::EAuth,
                    "not authenticated: send hello, then auth",
                );
            };
            // Revocation check (S12/M4): re-read the token files if they changed. A token
            // that disappeared is rejected unless we are still inside the ≤ 5 s grace
            // for requests in flight on this established connection.
            let mut c = cache.lock().unwrap();
            c.refresh(token_dir);
            let token = conn.inner.token.lock().unwrap().clone();
            let still_valid = c.set.as_ref().is_some_and(|s| s.contains(&token));
            let within_grace = c
                .rotated_at
                .is_some_and(|t| t.elapsed() <= config.revocation_grace);
            drop(c);
            if !still_valid && !within_grace {
                return Response::error_code(id, ErrorCode::EAuth, "token revoked");
            }
            if let Some(required) = required_scope(req) {
                if !granted.covers(required) {
                    return Response::error_code(
                        id,
                        ErrorCode::EScope,
                        format!(
                            "token scope '{}' cannot perform '{}' (needs '{}')",
                            granted.as_str(),
                            op,
                            required.as_str()
                        ),
                    );
                }
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handler.handle(conn, req)
            }));
            match result {
                Ok(resp) => resp,
                Err(_) => Response::error_code(id, ErrorCode::EInternal, "handler panicked"),
            }
        }
    }
}

/// Validate the response against the schema, enforce the response size cap, and write one
/// line. Returns `Err` when the connection is broken.
fn send_response(conn: &Connection, mut resp: Response, config: &ServerConfig) -> Result<(), ()> {
    if let Err(err) = resp.validate() {
        resp = Response::error(
            resp.id,
            IpcError::new(
                ErrorCode::EInternal,
                format!("invalid response from handler: {err}"),
            ),
        );
        let _ = resp.validate();
    }
    let value = resp.to_value();
    let mut w = conn.inner.writer.lock().unwrap();
    match framing::write_line(&mut *w, &value, config.max_response_bytes) {
        Ok(_) => Ok(()),
        Err(FrameError::TooLong) => {
            let fallback = Response::error_code(
                resp.id,
                ErrorCode::ELimit,
                "response exceeds the 4 MiB NDJSON limit",
            )
            .to_value();
            let _ = framing::write_line(&mut *w, &fallback, config.max_response_bytes);
            Err(())
        }
        Err(_) => Err(()),
    }
}
