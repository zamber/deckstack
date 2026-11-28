//! DS-113 requirement 2/3: the opt-in, read-only, **localhost-only `/preview` debug
//! surface** on the loopback WS/HTTP adapter (contract `docs/contracts/preview.md`,
//! schema `schemas/preview.json/v1`; transport contract `docs/contracts/ipc.md` §1.1).
//!
//! This is a debug endpoint, not a UI deliverable (ADR-014; ADR-019): it binds
//! **127.0.0.1 only**, requires the install/scoped **bearer token** on every request
//! (including the WS upgrade, where the browser-compatible `?token=` query form is
//! also accepted) and refuses any **non-loopback `Host`** header with 421 (T7).
//! It is **read-only**: HTTP serves GET only (anything else on `/preview*` is 405,
//! everything else 404), the WS accepts only `ping`/`get`/`subscribe`/`unsubscribe`
//! (any other op is answered with an `E_SCOPE` error frame and never executed), and
//! the surface never issues `set`/`emit`/`plugin`/`exec` and never touches the device
//! — a preview session performs zero state mutations and zero HID writes.
//!
//! Routes (all GET, all behind bearer + loopback-Host + the enable check):
//! - `/preview` → 200 `text/html` — the virtual deck page (renders `/controls`, hover
//!   shows the action docs from the aggregate of [`crate::actions_meta`]);
//! - `/preview/api/action-metadata` → 200 `application/json` — the action-metadata
//!   aggregate (`schemas/action-metadata.json/v1`);
//! - `/preview/api/state` → 200 `application/json` — `{rev, controls, bindings,
//!   plugins, devices}` snapshot;
//! - `/preview/ws` → WebSocket upgrade: `preview.hello` + `preview.snapshot` on
//!   connect, then live bus events for the client's subscriptions.
//!
//! Enablement: default-off. Started only when `ds-daemon run --preview` was given or
//! `/policy/preview` was `true` at boot; `/policy/preview=false` disables it live
//! (every request then 404s and WS messages are refused). The bound port is published
//! in `<data-dir>/http.port` (0600, removed on shutdown) per `docs/contracts/ipc.md`
//! §1.1.

use crate::actions_meta;
use crate::lifecycle::Shutdown;
use crate::router::{Bus, FnSink, Pattern};
use crate::frames::service::FrameService;
use crate::frames::FrameTarget;
use crate::state::StateHandle;
use ds_ipc::auth::TokenSet;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `schemas/preview.json`, embedded at compile time (served by the `schema` op).
pub const PREVIEW_SCHEMA_JSON: &str = include_str!("../../../schemas/preview.json");

/// Cap on the HTTP request head (request line + headers).
const MAX_HEAD: usize = 16 * 1024;
/// Cap on one WS frame payload (same order as the control plane's 1 MiB line cap).
const MAX_WS_FRAME: usize = 1024 * 1024;
/// Accept-loop poll interval.
const ACCEPT_POLL: Duration = Duration::from_millis(50);
/// HTTP request read timeout (the WS read loop is exempt: subscriptions are long-lived).
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Connection-id base for preview WS connections (never collides with IPC conn ids).
const WS_CONN_ID_BASE: u64 = 1 << 40;

/// Mutating ops refused on the preview WS (answered E_SCOPE, never executed).
const REFUSED_OPS: [&str; 12] = [
    "set",
    "emit",
    "bind",
    "unbind",
    "plugin",
    "exec",
    "stream-open",
    "stream-stop",
    "stream-stats",
    "daemon",
    "token",
    "auth",
];

// ---- SHA-1 + base64 (RFC 6455 handshake), hand-rolled: no new dependencies ------

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const TBL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TBL[(n >> 18) as usize & 63] as char);
        out.push(TBL[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TBL[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TBL[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The RFC 6455 accept value for a `Sec-WebSocket-Key`.
fn ws_accept(key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut input = key.trim().to_string();
    input.push_str(GUID);
    base64_encode(&sha1(input.as_bytes()))
}

// ---- the server -----------------------------------------------------------------

/// The running `/preview` server: one accept thread + one thread per connection.
pub struct PreviewServer {
    port: u16,
    http_port_file: PathBuf,
    enabled_at_boot: Arc<AtomicBool>,
    shutdown: Shutdown,
    state: StateHandle,
    bus: Bus,
    frames: Option<Arc<FrameService>>,
    data_dir: PathBuf,
}

fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

impl PreviewServer {
    /// Bind `127.0.0.1:0`, publish the port in `<data-dir>/http.port` (0600) and start
    /// the accept loop. `enabled_at_boot` records why the surface started (flag or
    /// policy); `/policy/preview=false` still disables it live (checked per request).
    pub fn start(
        state: StateHandle,
        bus: Bus,
        frames: Option<Arc<FrameService>>,
        data_dir: &Path,
        shutdown: Shutdown,
        enabled_at_boot: bool,
    ) -> std::io::Result<Arc<Self>> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let http_port_file = data_dir.join("http.port");
        write_0600(&http_port_file, format!("{port}\n").as_bytes())?;
        let server = Arc::new(Self {
            port,
            http_port_file,
            enabled_at_boot: Arc::new(AtomicBool::new(enabled_at_boot)),
            shutdown,
            state,
            bus,
            frames,
            data_dir: data_dir.to_path_buf(),
        });
        let srv = Arc::clone(&server);
        std::thread::Builder::new()
            .name("ds-preview-accept".into())
            .spawn(move || srv.accept_loop(listener))?;
        Ok(server)
    }

    /// Fallback for the frame stream: re-encode the current control image from state
    /// (read-only) when no frame has been sent yet on the frame plane.
    fn frame_from_state(&self, target: crate::frames::FrameTarget) -> Option<(String, Vec<u8>)> {
        let path = match target {
            crate::frames::FrameTarget::Panel => "/controls/panel/image",
            crate::frames::FrameTarget::Dial => "/controls/dial/image",
            crate::frames::FrameTarget::Key(n) => {
                return self.state.get(&format!("/controls/keys/{n}/image"), None).ok().and_then(|r| {
                    let v = r.value;
                    if v.get("kind").and_then(|k| k.as_str()) == Some("file") {
                        v.get("path").and_then(|p| p.as_str()).map(|p| p.to_string())
                    } else { None }
                }).and_then(|p| self.encode_image_file(target, &p));
            }
        };
        let v = self.state.get(path, None).ok()?.value;
        if v.get("kind").and_then(|k| k.as_str()) != Some("file") {
            return None;
        }
        let p = v.get("path")?.as_str()?;
        self.encode_image_file(target, p)
    }

    fn encode_image_file(&self, target: crate::frames::FrameTarget, path: &str) -> Option<(String, Vec<u8>)> {
        let wire = crate::device::read_image_file(path).ok()?;
        let jpeg = crate::frames::encode::encode_frame(target, &wire).ok()?;
        let sha = ds_device::chunks::sha256_of(&jpeg).iter().map(|b| format!("{b:02x}")).collect();
        Some((sha, jpeg))
    }

    /// The bound port (also in `<data-dir>/http.port`).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the surface answers at all right now: enabled at boot AND not disabled
    /// live via `/policy/preview=false` (read through the state task, so an admin
    /// `ds set /policy/preview false` takes effect on the next request).
    pub fn surface_enabled(&self) -> bool {
        if !self.enabled_at_boot.load(Ordering::SeqCst) {
            return false;
        }
        match self.state.get("/policy/preview", None) {
            Ok(reply) => reply.value != json!(false),
            Err(_) => true, // absent => default (off unless the flag/policy enabled it)
        }
    }

    /// Constant-time token check against the current on-disk token set (S12).
    fn token_valid(&self, token: &str) -> bool {
        TokenSet::load(&self.data_dir)
            .map(|set| set.scope_of(token).is_some())
            .unwrap_or(false)
    }

    fn accept_loop(self: &Arc<Self>, listener: TcpListener) {
        while !self.shutdown.get() {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let srv = Arc::clone(self);
                    let _ = std::thread::Builder::new()
                        .name("ds-preview-conn".into())
                        .spawn(move || srv.handle_conn(stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(_) => std::thread::sleep(ACCEPT_POLL),
            }
        }
    }

    /// Remove the published port file (daemon shutdown).
    pub fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.http_port_file);
    }

    // ---- one HTTP connection ----------------------------------------------------

    fn handle_conn(self: &Arc<Self>, mut stream: TcpStream) {
        let _ = stream.set_read_timeout(Some(HTTP_READ_TIMEOUT));
        let _ = stream.set_nodelay(true);
        let Some((method, raw_path, headers)) = read_head(&mut stream) else {
            return;
        };
        let path = raw_path.split(['?', '#']).next().unwrap_or("");
        let header = |name: &str| -> Option<String> {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };

        // T7: refuse any non-loopback Host header (DNS-rebinding protection).
        let host = header("host").unwrap_or_default();
        let host_name = host.split(':').next().unwrap_or("");
        if !matches!(host_name, "127.0.0.1" | "localhost") {
            return respond(
                &mut stream,
                421,
                "text/plain; charset=utf-8",
                "421 Misdirected Request: Host must be 127.0.0.1 or localhost (T7)",
            );
        }
        // Bearer token (re-loaded per request: instant revocation, S12). The WS
        // upgrade additionally accepts ?token=… (browsers cannot set headers on a
        // WebSocket handshake); every other route requires the header.
        let auth = header("authorization").unwrap_or_default();
        let header_token = auth.strip_prefix("Bearer ").map(str::trim);
        let header_ok = header_token.map(|t| self.token_valid(t)).unwrap_or(false);
        let query_token = raw_path
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or("")
            .split('&')
            .find_map(|kv| {
                kv.split_once('=')
                    .filter(|(k, _)| *k == "token")
                    .map(|(_, v)| v.to_string())
            });
        let query_ok = query_token
            .as_deref()
            .map(|t| self.token_valid(t))
            .unwrap_or(false);
        let token_ok = header_ok || (path == "/preview/ws" && query_ok);
        if !token_ok {
            return respond(
                &mut stream,
                401,
                "text/plain; charset=utf-8",
                "401 Unauthorized: Bearer token required",
            );
        }
        // Live enable/disable via /policy/preview.
        if !self.surface_enabled() {
            return respond(
                &mut stream,
                404,
                "text/plain; charset=utf-8",
                "404 Not Found: the /preview surface is disabled (/policy/preview)",
            );
        }

        if method != "GET" {
            // /preview accepts NO writes: any non-GET on the surface is 405.
            return respond(
                &mut stream,
                405,
                "text/plain; charset=utf-8",
                "405 Method Not Allowed: /preview is read-only (GET only)",
            );
        }

        match path {
            "/preview" => {
                let html = self.render_page();
                respond(&mut stream, 200, "text/html; charset=utf-8", &html);
            }
            "/preview/api/action-metadata" => {
                let doc = actions_meta::aggregate(&self.state, &self.bus);
                respond_json(&mut stream, 200, &doc);
            }
            "/preview/api/state" => {
                let snap = self.snapshot();
                respond_json(&mut stream, 200, &snap);
            }
            "/preview/ws" => {
                let upgrade = header("upgrade").unwrap_or_default().to_ascii_lowercase();
                let key = header("sec-websocket-key");
                if upgrade != "websocket" || key.is_none() {
                    return respond(
                        &mut stream,
                        400,
                        "text/plain; charset=utf-8",
                        "400 Bad Request: expected a WebSocket upgrade",
                    );
                }
                let accept = ws_accept(key.as_deref().unwrap_or(""));
                let resp = format!(
                    "HTTP/1.1 101 Switching Protocols\r\n\
                     Upgrade: websocket\r\n\
                     Connection: Upgrade\r\n\
                     Sec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                if stream.write_all(resp.as_bytes()).is_err() {
                    return;
                }
                self.run_ws(stream);
            }
            _ => {
                respond(
                    &mut stream,
                    404,
                    "text/plain; charset=utf-8",
                    "404 Not Found: the debug surface serves /preview only",
                );
            }
        }
    }

    // ---- documents ----------------------------------------------------------------

    /// `{rev, controls, bindings, plugins, devices}` — the live state the page and the
    /// WS snapshot render. Pure reads.
    fn snapshot(&self) -> Value {
        let get = |p: &str| {
            self.state
                .get(p, None)
                .map(|r| r.value)
                .unwrap_or(Value::Null)
        };
        json!({
            "rev": self.state.rev().unwrap_or(0),
            "controls": get("/controls"),
            "bindings": get("/bindings"),
            "plugins": get("/plugins"),
            "devices": get("/devices"),
        })
    }

    /// The virtual-deck HTML page (DS-113 acceptance: the deck renders, hover shows
    /// the bound actions with param docs from the registered manifests).
    fn render_page(&self) -> String {
        let snap = self.snapshot();
        let actions = actions_meta::aggregate(&self.state, &self.bus);
        let token = std::fs::read_to_string(self.data_dir.join("token"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut page = PAGE_TEMPLATE.to_string();
        // escape "</" so embedded JSON can never break out of the script element
        let esc = |v: &Value| serde_json::to_string(v).unwrap().replace("</", "<\\/");
        page = page.replace("__PORT__", &self.port.to_string());
        page = page.replace("__TOKEN__", &token);
        page = page.replace("__STATE__", &esc(&snap));
        page = page.replace("__ACTIONS__", &esc(&actions));
        page
    }

    // ---- the read-only WebSocket --------------------------------------------------

    fn run_ws(self: &Arc<Self>, stream: TcpStream) {
        let conn_id = WS_CONN_ID_BASE + NEXT_CONN.fetch_add(1, Ordering::SeqCst);
        let writer = Arc::new(WsWriter {
            stream: Mutex::new(stream.try_clone().expect("clone ws stream")),
        });
        let mut subs: Vec<u64> = Vec::new();
        // hello + snapshot (read-only surface, protocol 1)
        let rev = self.state.rev().unwrap_or(0);
        if writer
            .send(json!({
                "op": "preview.hello", "protocol": 1, "rev": rev,
                "server": crate::server_name(), "read_only": true,
                "note": "read-only debug surface: ping/get/subscribe/unsubscribe/frame (read-only)",
            }))
            .is_err()
        {
            return;
        }
        if writer
            .send(json!({
                "op": "preview.snapshot", "rev": rev,
                "state": self.snapshot(),
                "actions": actions_meta::aggregate(&self.state, &self.bus),
            }))
            .is_err()
        {
            return;
        }
        let mut stream = stream;
        let mut frag: Vec<u8> = Vec::new();
        let mut frag_op: u8 = 0x1;
        loop {
            if !self.surface_enabled() {
                let _ = writer.send(json!({
                    "op": "error",
                    "error": {"code": "E_SCOPE",
                              "message": "the /preview surface was disabled (/policy/preview=false)"}
                }));
                let _ = writer.send_close();
                break;
            }
            let Some((fin, opcode, payload)) = read_frame(&mut stream) else {
                break;
            };
            match opcode {
                0x8 => {
                    let _ = writer.send_close();
                    break;
                }
                0x9 => {
                    if writer.send_raw(&payload, 0xA).is_err() {
                        break;
                    }
                    continue;
                }
                0xA => continue,
                0x2 => {
                    let _ = writer.send(json!({
                        "op": "error",
                        "error": {"code": "E_SCHEMA", "message": "binary frames are not accepted on /preview/ws"}
                    }));
                    continue;
                }
                0x0 | 0x1 => {}
                _ => break,
            }
            if opcode == 0x1 {
                frag.clear();
                frag.extend_from_slice(&payload);
                frag_op = 0x1;
            } else {
                frag.extend_from_slice(&payload);
                if frag.len() > MAX_WS_FRAME {
                    break;
                }
            }
            if !fin {
                continue;
            }
            let payload = std::mem::take(&mut frag);
            if frag_op == 0x1 {
                {
                    let msg: Value = match serde_json::from_slice(&payload) {
                        Ok(v) => v,
                        Err(_) => {
                            let _ = writer.send(json!({
                                "op": "error",
                                "error": {"code": "E_SCHEMA", "message": "WS frame is not one JSON object"}
                            }));
                            continue;
                        }
                    };
                    let op = msg.get("op").and_then(Value::as_str).unwrap_or("");
                    let id = msg.get("id").cloned();
                    match op {
                        "ping" => {
                            let mut r = json!({"op": "preview.result", "pong": true});
                            if let Some(id) = id {
                                r["id"] = id;
                            }
                            if writer.send(r).is_err() {
                                break;
                            }
                        }
                        "get" => {
                            let Some(path) = msg.get("path").and_then(Value::as_str) else {
                                let _ = writer.send(err_reply(id, "E_SCHEMA", "get needs a path"));
                                continue;
                            };
                            match self.state.get(path, None) {
                                Ok(reply) => {
                                    let mut r =
                                        json!({"op": "preview.result", "value": reply.value});
                                    if let Some(rev) = reply.rev {
                                        r["rev"] = json!(rev);
                                    }
                                    if let Some(id) = id {
                                        r["id"] = id;
                                    }
                                    if writer.send(r).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ = writer.send(err_reply(id, e.code.as_str(), &e.message));
                                }
                            }
                        }
                        "frame" => {
                            let Some(tname) = msg.get("target").and_then(Value::as_str) else {
                                let _ = writer.send(err_reply(id, "E_SCHEMA", "frame needs a target"));
                                continue;
                            };
                            let Some(target) = FrameTarget::parse(tname) else {
                                let _ = writer.send(err_reply(id, "E_SCHEMA", "unknown frame target"));
                                continue;
                            };
                            let mut r = match &self.frames {
                                Some(f) => match f.last_frame(target) {
                                    Some((sha, bytes)) => json!({"op": "preview.frame", "target": tname, "sha256": sha, "encoding": "base64", "data": b64(&bytes)}),
                                    None => match self.frame_from_state(target) {
                                        Some((sha, bytes)) => json!({"op": "preview.frame", "target": tname, "sha256": sha, "encoding": "base64", "data": b64(&bytes)}),
                                        None => json!({"op": "preview.frame", "target": tname, "sha256": null, "data": null}),
                                    },
                                },
                                None => json!({"op": "preview.frame", "target": tname, "sha256": null, "data": null}),
                            };
                            if let Some(id) = id { r["id"] = id; }
                            if writer.send(r).is_err() { break; }
                        }
                        "subscribe" => {
                            let Some(path) = msg.get("path").and_then(Value::as_str) else {
                                let _ = writer.send(err_reply(
                                    id,
                                    "E_SCHEMA",
                                    "subscribe needs a path",
                                ));
                                continue;
                            };
                            match Pattern::parse_wire(path) {
                                Ok(pat) => {
                                    let wire = pat.as_wire();
                                    let sink = Arc::new(FnSink(Arc::new({
                                        let writer = Arc::clone(&writer);
                                        move |m: &Value| {
                                            let _ = writer.send(m.clone());
                                        }
                                    })));
                                    let sub =
                                        self.bus.subscribe(pat, Some(conn_id), wire.clone(), sink);
                                    subs.push(sub);
                                    let mut r = json!({"op": "preview.result", "subscribed": wire, "sub": sub});
                                    if let Some(id) = id {
                                        r["id"] = id;
                                    }
                                    if writer.send(r).is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ = writer.send(err_reply(id, e.code.as_str(), &e.message));
                                }
                            }
                        }
                        "unsubscribe" => {
                            let Some(path) = msg.get("path").and_then(Value::as_str) else {
                                let _ = writer.send(err_reply(
                                    id,
                                    "E_SCHEMA",
                                    "unsubscribe needs a path",
                                ));
                                continue;
                            };
                            let removed = self.bus.unsubscribe_wire(conn_id, path);
                            let mut r = json!({"op": "preview.result", "unsubscribed": path, "removed": removed});
                            if let Some(id) = id {
                                r["id"] = id;
                            }
                            if writer.send(r).is_err() {
                                break;
                            }
                        }
                        other if REFUSED_OPS.contains(&other) => {
                            // read-only guarantee: the op is refused, never executed
                            let _ = writer.send(err_reply(
                                id,
                                "E_SCOPE",
                                "the /preview surface is read-only: mutating ops are refused",
                            ));
                        }
                        _ => {
                            let _ = writer.send(err_reply(
                                id,
                                "E_SCHEMA",
                                "unknown op on the preview WS",
                            ));
                        }
                    }
                }
            }
        }
        for sub in subs {
            self.bus.unsubscribe(sub);
        }
    }
}

static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

fn err_reply(id: Option<Value>, code: &str, message: &str) -> Value {
    let mut r = json!({"op": "error", "error": {"code": code, "message": message}});
    if let Some(id) = id {
        r["id"] = id;
    }
    r
}

/// One WS connection's thread-safe text sender.
struct WsWriter {
    stream: Mutex<TcpStream>,
}

impl WsWriter {
    fn send(&self, v: Value) -> std::io::Result<()> {
        let line = serde_json::to_vec(&v)?;
        self.send_raw(&line, 0x1)
    }

    fn send_raw(&self, payload: &[u8], opcode: u8) -> std::io::Result<()> {
        let frame = ws_frame(payload, opcode);
        let mut s = self.stream.lock().unwrap();
        s.write_all(&frame)?;
        s.flush()
    }

    fn send_close(&self) -> std::io::Result<()> {
        self.send_raw(&[], 0x8)
    }
}

/// Encode one server→client frame (FIN set, unmasked).
fn ws_frame(payload: &[u8], opcode: u8) -> Vec<u8> {
    let len = payload.len();
    let mut out = Vec::with_capacity(len + 10);
    out.push(0x80 | opcode);
    if len < 126 {
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// One client→server frame: (fin, opcode, unmasked payload). `None` on EOF/error.
fn read_frame(stream: &mut TcpStream) -> Option<(bool, u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).ok()?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7F) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b).ok()?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        stream.read_exact(&mut b).ok()?;
        len = u64::from_be_bytes(b);
    }
    if len > MAX_WS_FRAME as u64 {
        return None;
    }
    let mask = if masked {
        let mut m = [0u8; 4];
        stream.read_exact(&mut m).ok()?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    stream.read_exact(&mut payload).ok()?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    let _ = fin;
    Some((fin, opcode, payload))
}

// ---- HTTP head parsing / responses ------------------------------------------------

/// Read and parse the request head. Returns (method, raw path incl. query, headers as
/// (lowercase name, value) pairs).
/// One parsed HTTP request head: (method, raw path incl. query, headers as
/// (lowercase name, value) pairs).
type Head = (String, String, Vec<(String, String)>);

fn read_head(stream: &mut TcpStream) -> Option<Head> {
    let mut buf = Vec::with_capacity(1024);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.len() > MAX_HEAD {
                    return None;
                }
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => return None,
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let _version = parts.next(); // we only speak HTTP/1.1 responses
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Some((method, path, headers))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        421 => "Misdirected Request",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, status: u16, v: &Value) {
    let body = serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string());
    respond(stream, status, "application/json", &body);
}

fn write_0600(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    f.write_all(contents)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

// ---- the virtual-deck page --------------------------------------------------------

const PAGE_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>deckstack /preview (debug)</title>
<style>
 body { font: 13px/1.4 system-ui, sans-serif; background: #14171c; color: #d7dde6; margin: 16px; }
 h1 { font-size: 16px; } .sub { color: #8b94a3; margin-bottom: 12px; }
 #deck { display: grid; grid-template-columns: repeat(5, 96px); gap: 8px; }
 .ctl { background: #1e232b; border: 1px solid #333c48; border-radius: 6px; padding: 6px; height: 64px; cursor: default; }
 .ctl .nm { font-weight: 600; } .ctl .lb { color: #8b94a3; overflow: hidden; text-overflow: ellipsis; }
 #docs { position: fixed; right: 16px; top: 16px; width: 380px; max-height: 90vh; overflow: auto;
         background: #1e232b; border: 1px solid #333c48; border-radius: 6px; padding: 10px; display: none; }
 #docs h2 { font-size: 13px; margin: 8px 0 4px; }
 #docs code { background: #14171c; padding: 1px 4px; border-radius: 3px; }
 .act { margin: 4px 0; } .cap { color: #8b94a3; }
 #log { color: #8b94a3; margin-top: 12px; white-space: pre-wrap; }
</style>
</head>
<body>
<h1>deckstack /preview <span class="sub">read-only debug surface · ds-daemon · rev <span id="rev"></span></span></h1>
<div id="deck"></div>
<div id="docs"></div>
<div id="log"></div>
<script>
const PORT = __PORT__, TOKEN = "__TOKEN__";
const STATE = __STATE__, ACTIONS = __ACTIONS__;
const $ = (s) => document.querySelector(s);
function esc(s) { return String(s).replace(/[&<>"]/g, (c) => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c])); }
function render() {
  $("#rev").textContent = STATE.rev;
  const deck = $("#deck"); deck.innerHTML = "";
  const ctrls = STATE.controls || {};
  const keys = ctrls.keys || {};
  for (let i = 0; i < 15; i++) deck.appendChild(ctl("keys/" + i, keys[i] || {}));
  for (const [k, v] of Object.entries({dial: ctrls.dial || {}, panel: ctrls.panel || {}}))
    deck.appendChild(ctl(k, v));
  (ctrls.knobs ? Object.keys(ctrls.knobs) : []).forEach((k) => deck.appendChild(ctl("knobs/" + k, ctrls.knobs[k])));
  (ctrls.faders ? Object.keys(ctrls.faders) : []).forEach((k) => deck.appendChild(ctl("faders/" + k, ctrls.faders[k])));
  (ctrls.rollers ? Object.keys(ctrls.rollers) : []).forEach((k) => deck.appendChild(ctl("rollers/" + k, ctrls.rollers[k])));
}
function ctl(path, v) {
  const d = document.createElement("div"); d.className = "ctl";
  const img = v.image && v.image.kind === "color" ? v.image.value : "#2a313b";
  d.innerHTML = '<div class="nm">' + esc(path) + '</div><div class="lb">' + esc(v.label ?? "") + '</div>';
  d.style.borderTop = "4px solid " + esc(img);
  d.onmouseenter = () => showDocs(path, v); d.onmouseleave = () => ($("#docs").style.display = "none");
  return d;
}
function showDocs(path, v) {
  const d = $("#docs"); let h = "<h2>" + esc(path) + "</h2>";
  h += '<div class="cap">label: ' + esc(JSON.stringify(v.label ?? null)) + "</div>";
  const binds = (STATE.bindings || []).filter((b) => (b.topic || "").includes(path.replace("/", ".")));
  h += "<h2>bindings</h2>" + (binds.length ? binds.map((b) => '<div class="act"><code>' + esc(b.topic) + "</code> → " + esc(JSON.stringify(b.steps)) + "</div>").join("") : '<div class="cap">none</div>');
  h += "<h2>core steps</h2>" + ACTIONS.core.map((a) => act(a)).join("");
  for (const p of ACTIONS.plugins)
    h += "<h2>plugin " + esc(p.id) + " <span class='cap'>" + esc((p.capabilities || []).join(", ")) + "</span></h2>" + p.actions.map((a) => act(a, p.id)).join("");
  d.innerHTML = h; d.style.display = "block";
}
function act(a, plugin) {
  return '<div class="act"><code>' + esc((plugin ? plugin + "." : "") + a.do + "/" + a.id) + "</code> " +
    esc(a.description || a.name || "") + '<div class="cap">' + esc(a.capability || "") +
    (a.params ? "<br>params: <code>" + esc(JSON.stringify(a.params)) + "</code>" : "") + "</div></div>";
}
function log(m) { $("#log").textContent += m + "\n"; }
render();
const ws = new WebSocket("ws://127.0.0.1:" + PORT + "/preview/ws?token=" + encodeURIComponent(TOKEN));
ws.onmessage = (ev) => {
  const m = JSON.parse(ev.data);
  if (m.op === "preview.snapshot") { Object.assign(STATE, m.state); STATE.rev = m.rev; render(); }
  else if (m.op === "event") {
    if (m.path && m.path.startsWith("/controls/")) {
      const parts = m.path.split("/"); // /controls/<kind>/<i>/<field>
      const kind = parts[2], idx = parts[3];
      STATE.controls[kind] = STATE.controls[kind] || {};
      STATE.controls[kind][idx] = STATE.controls[kind][idx] || {};
      STATE.controls[kind][idx][parts[4]] = m.value;
      if (m.rev != null) STATE.rev = m.rev;
      render();
    }
    log("event " + (m.topic || m.path) + (m.rev != null ? " rev=" + m.rev : ""));
  } else if (m.op === "error") log("error " + JSON.stringify(m.error));
};
ws.onclose = () => log("ws closed");
ws.onerror = () => log("ws error");
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle;
    use crate::router::Bus;
    use ds_state::Store;
    use std::io::{Read, Write};

    fn boot() -> (
        tempfile::TempDir,
        Arc<PreviewServer>,
        StateHandle,
        std::thread::JoinHandle<()>,
        Bus,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("data");
        lifecycle::prepare_data_dir(&dir).unwrap();
        lifecycle::ensure_token(&dir).unwrap();
        let bus = Bus::new();
        let (state, st) = crate::state::StateTask::spawn(Store::fresh(), None, bus.clone());
        let srv = PreviewServer::start(state.clone(), bus.clone(), None, &dir, Shutdown::manual(), true)
            .unwrap();
        (tmp, srv, state, st, bus)
    }

    fn token_of(tmp: &tempfile::TempDir) -> String {
        std::fs::read_to_string(tmp.path().join("data").join("token"))
            .unwrap()
            .trim()
            .to_string()
    }

    /// One raw HTTP request; returns (status, body).
    fn http(port: u16, req: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        s.flush().unwrap();
        let mut buf = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            match s.read(&mut b) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&b[..n]),
                Err(_) => break,
            }
        }
        let text = String::from_utf8_lossy(&buf).to_string();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .unwrap_or(0);
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (status, body)
    }

    fn get(port: u16, path: &str, token: Option<&str>, host: &str) -> (u16, String) {
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        http(
            port,
            &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{auth}Connection: close\r\n\r\n"),
        )
    }

    #[test]
    fn sha1_and_base64_rfc_vectors() {
        // RFC 6455 §4.2.2 example: key "dGhlIHNhbXBsZSBub25jZQ==" ->
        // accept "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(
            sha1(b"abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn http_routes_auth_host_and_read_only() {
        let (tmp, srv, state, st, _bus) = boot();
        let port = srv.port();
        let token = token_of(&tmp);
        // port file published
        let pf = std::fs::read_to_string(tmp.path().join("data").join("http.port")).unwrap();
        assert_eq!(pf.trim().parse::<u16>().unwrap(), port);
        // 200 HTML with token
        let (st_, body) = get(port, "/preview", Some(&token), "127.0.0.1");
        assert_eq!(st_, 200, "{body}");
        assert!(body.contains("deckstack /preview"));
        assert!(body.contains("preview/ws"));
        // localhost Host also accepted
        assert_eq!(get(port, "/preview", Some(&token), "localhost").0, 200);
        // missing token -> 401
        assert_eq!(get(port, "/preview", None, "127.0.0.1").0, 401);
        // bad token -> 401
        assert_eq!(
            get(port, "/preview", Some("0".repeat(64).as_str()), "127.0.0.1").0,
            401
        );
        // non-loopback Host -> 421 (T7), even with a valid token
        assert_eq!(get(port, "/preview", Some(&token), "evil.example").0, 421);
        assert_eq!(
            get(port, "/preview", Some(&token), "127.0.0.1.evil.com").0,
            421
        );
        // action-metadata JSON
        let (st_, body) = get(
            port,
            "/preview/api/action-metadata",
            Some(&token),
            "127.0.0.1",
        );
        assert_eq!(st_, 200);
        let doc: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(doc["protocol"], json!(1));
        assert_eq!(doc["core"].as_array().unwrap().len(), 5);
        // state snapshot
        let (st_, body) = get(port, "/preview/api/state", Some(&token), "127.0.0.1");
        assert_eq!(st_, 200);
        let snap: Value = serde_json::from_str(&body).unwrap();
        assert!(snap.get("rev").is_some() && snap.get("controls").is_some());
        // root and unknown paths -> 404
        assert_eq!(get(port, "/", Some(&token), "127.0.0.1").0, 404);
        assert_eq!(get(port, "/api/get", Some(&token), "127.0.0.1").0, 404);
        // POST on the surface -> 405 (no writes accepted)
        let (st_, _) = http(
            port,
            &format!("POST /preview HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(st_, 405);
        let (st_, _) = http(
            port,
            &format!("POST /preview/api/state HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
        );
        assert_eq!(st_, 405);
        // policy disable -> 404 live
        state
            .set(ds_state::Transaction::new(vec![ds_state::Op::set(
                "/policy/preview",
                json!(false),
            )]))
            .unwrap();
        assert_eq!(get(port, "/preview", Some(&token), "127.0.0.1").0, 404);
        // re-enable
        state
            .set(ds_state::Transaction::new(vec![ds_state::Op::set(
                "/policy/preview",
                json!(true),
            )]))
            .unwrap();
        assert_eq!(get(port, "/preview", Some(&token), "127.0.0.1").0, 200);
        state.stop();
        st.join().unwrap();
        srv.cleanup();
        assert!(!tmp.path().join("data").join("http.port").exists());
    }

    /// Minimal WS client for tests: handshake + one masked text frame send + frame recv.
    struct WsClient {
        s: TcpStream,
    }

    impl WsClient {
        fn connect(port: u16, token: &str, host: &str) -> WsClient {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let req = format!(
                "GET /preview/ws?token={token} HTTP/1.1\r\nHost: {host}\r\n\
                 Upgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
            );
            s.write_all(req.as_bytes()).unwrap();
            let mut buf = Vec::new();
            let mut b = [0u8; 1024];
            loop {
                let n = s.read(&mut b).unwrap();
                buf.extend_from_slice(&b[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf).to_string();
            assert!(head.starts_with("HTTP/1.1 101"), "{head}");
            assert!(head.contains("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
            WsClient { s }
        }

        fn send_text(&mut self, v: &Value) {
            let payload = serde_json::to_vec(v).unwrap();
            let mask = [0x11, 0x22, 0x33, 0x44];
            let mut frame = vec![0x81u8];
            if payload.len() < 126 {
                frame.push(0x80 | payload.len() as u8);
            } else {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            frame.extend_from_slice(&mask);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            self.s.write_all(&frame).unwrap();
        }

        fn recv(&mut self) -> Value {
            let mut head = [0u8; 2];
            self.s.read_exact(&mut head).unwrap();
            let opcode = head[0] & 0x0F;
            assert_eq!(opcode, 0x1, "expected a text frame");
            let mut len = (head[1] & 0x7F) as usize;
            if len == 126 {
                let mut b = [0u8; 2];
                self.s.read_exact(&mut b).unwrap();
                len = u16::from_be_bytes(b) as usize;
            }
            let mut payload = vec![0u8; len];
            self.s.read_exact(&mut payload).unwrap();
            serde_json::from_slice(&payload).unwrap()
        }
    }

    #[test]
    fn ws_streams_state_changes_and_refuses_writes() {
        let (_tmp, srv, state, st, _bus) = boot();
        let port = srv.port();
        let token = std::fs::read_to_string(_tmp.path().join("data").join("token"))
            .unwrap()
            .trim()
            .to_string();
        // upgrade without any token -> 401
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"GET /preview/ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").unwrap();
        let mut buf = String::new();
        let mut b = [0u8; 512];
        let n = s.read(&mut b).unwrap();
        buf.push_str(&String::from_utf8_lossy(&b[..n]));
        assert!(buf.starts_with("HTTP/1.1 401"), "{buf}");

        let mut ws = WsClient::connect(port, &token, "127.0.0.1");
        let hello = ws.recv();
        assert_eq!(hello["op"], json!("preview.hello"));
        assert_eq!(hello["read_only"], json!(true));
        let snap = ws.recv();
        assert_eq!(snap["op"], json!("preview.snapshot"));
        assert!(snap["state"].get("controls").is_some());
        assert_eq!(snap["actions"]["core"].as_array().unwrap().len(), 5);

        // subscribe to control state, then a set (through the state task, as the CLI
        // would) must stream an event frame
        ws.send_text(&json!({"id": 1, "op": "subscribe", "path": "/controls/*"}));
        let ack = ws.recv();
        assert_eq!(ack["op"], json!("preview.result"));
        assert_eq!(ack["subscribed"], json!("/controls/*"));
        let rev0 = state.rev().unwrap();
        state
            .set(ds_state::Transaction::new(vec![ds_state::Op::set(
                "/controls/keys/0/label",
                json!("hover-me"),
            )]))
            .unwrap();
        let ev = ws.recv();
        assert_eq!(ev["op"], json!("event"));
        assert_eq!(ev["path"], json!("/controls/keys/0/label"));
        assert_eq!(ev["value"], json!("hover-me"));
        assert_eq!(ev["rev"], json!(rev0 + 1));

        // a mutating op on the WS is refused with E_SCOPE and does NOT run
        ws.send_text(&json!({"id": 2, "op": "set", "ops": [{"op": "set", "path": "/controls/keys/1/label", "value": "nope"}]}));
        let err = ws.recv();
        assert_eq!(err["op"], json!("error"));
        assert_eq!(err["error"]["code"], json!("E_SCOPE"));
        assert!(
            state.get("/controls/keys/1/label", None).is_err(),
            "the set must not have run"
        );
        assert_eq!(
            state.rev().unwrap(),
            rev0 + 1,
            "rev unchanged by the refused op"
        );

        // get + ping work
        ws.send_text(&json!({"id": 3, "op": "get", "path": "/controls/keys/0/label"}));
        let r = ws.recv();
        assert_eq!(r["value"], json!("hover-me"));
        ws.send_text(&json!({"id": 4, "op": "ping"}));
        assert_eq!(ws.recv()["pong"], json!(true));

        state.stop();
        st.join().unwrap();
    }
}
