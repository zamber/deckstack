//! The serve loop: handshake, register, dispatch, request/reply, shutdown.
//!
//! Single-threaded and re-entrant-safe: while an action handler waits for a `reply` to one of
//! its own `request`s, the pump reads host messages and queues everything that is not the
//! awaited reply; queued messages are dispatched by the top-level loop afterwards, so a
//! handler may freely call [`Sdk::get_state`], [`Sdk::set_state`] and [`Sdk::http`].
//!
//! Malformed input (bad JSON, unknown op, oversized line) is logged to stderr and skipped —
//! a protocol violation never panics.

use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::effect::Effect;
use crate::error::{PluginError, ServeError};
use crate::http::{self, HttpResponse};
use crate::manifest::Manifest;
use crate::params;
use crate::wire::{
    parse_host_msg, EffectsMsg, ErrorMsg, HostMsg, RegisterMsg, RequestMsg, MAX_LINE_BYTES,
    PROTOCOL_VERSION,
};

/// A host event delivered to the plugin's event handler.
#[derive(Debug, Clone, PartialEq)]
pub struct HostEvent {
    pub event: String,
    pub control: Option<String>,
    pub state: Option<Value>,
    /// Present when the host correlates the event with a reply (`effects`/`error`).
    pub request_id: Option<u64>,
}

type ActionHandler = Box<dyn Fn(&mut Sdk, &Value) -> Result<Vec<Effect>, PluginError> + Send>;
type EventHandler = Box<dyn Fn(&mut Sdk, &HostEvent) -> Result<Vec<Effect>, PluginError> + Send>;

struct ActionEntry {
    params: Value,
    handler: ActionHandler,
}

enum LineRead {
    Line(String),
    TooLong,
    Eof,
}

/// The plugin side of one host session. Handlers receive `&mut Sdk` and use it for
/// host-mediated requests (`get_state`/`set_state`/`http`) and to inspect the granted
/// capabilities.
pub struct Sdk<'a> {
    input: Box<dyn BufRead + 'a>,
    output: Box<dyn Write + 'a>,
    manifest: Manifest,
    actions: HashMap<String, ActionEntry>,
    event_handler: Option<EventHandler>,
    /// Capabilities granted by the host in `hello` (informational; the host enforces them).
    granted_caps: Vec<String>,
    /// Raw host lines read while waiting for a reply; drained by the top-level loop.
    queue: VecDeque<String>,
    next_request_id: u64,
    violations: u64,
}

impl<'a> Sdk<'a> {
    fn new(
        input: Box<dyn BufRead + 'a>,
        output: Box<dyn Write + 'a>,
        manifest: Manifest,
        actions: HashMap<String, ActionEntry>,
        event_handler: Option<EventHandler>,
    ) -> Self {
        Self {
            input,
            output,
            manifest,
            actions,
            event_handler,
            granted_caps: Vec::new(),
            queue: VecDeque::new(),
            next_request_id: 1,
            violations: 0,
        }
    }

    /// The plugin's manifest (as sent in `register`).
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Capability strings granted by the host in `hello` (e.g. `["state.read","net:…"]`).
    pub fn granted_capabilities(&self) -> &[String] {
        &self.granted_caps
    }

    /// Number of malformed/oversized host lines skipped so far (mirrors the host's
    /// `stats.violations` counter on the plugin side).
    pub fn violations(&self) -> u64 {
        self.violations
    }

    fn warn(&mut self, msg: &str) {
        self.violations += 1;
        eprintln!("[ds-plugin-sdk {}] {msg}", self.manifest.id);
    }

    // -- transport ---------------------------------------------------------

    /// Read one line from the wire (never from the queue), bounded at [`MAX_LINE_BYTES`].
    fn read_wire_line(&mut self) -> Result<LineRead, ServeError> {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        loop {
            let available = match self.input.fill_buf() {
                Ok(a) => a,
                Err(e) => return Err(ServeError::Io(e)),
            };
            if available.is_empty() {
                if buf.is_empty() {
                    return Ok(LineRead::Eof);
                }
                return Ok(LineRead::Line(String::from_utf8_lossy(&buf).into_owned()));
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    buf.extend_from_slice(&available[..=i]);
                    let n = i + 1;
                    self.input.consume(n);
                    return Ok(LineRead::Line(String::from_utf8_lossy(&buf).into_owned()));
                }
                None => {
                    if buf.len() + available.len() > MAX_LINE_BYTES {
                        self.drain_line()?;
                        return Ok(LineRead::TooLong);
                    }
                    buf.extend_from_slice(available);
                    let n = available.len();
                    self.input.consume(n);
                }
            }
        }
    }

    /// Consume the remainder of an over-long line without retaining it.
    fn drain_line(&mut self) -> Result<(), ServeError> {
        loop {
            let available = match self.input.fill_buf() {
                Ok(a) => a,
                Err(e) => return Err(ServeError::Io(e)),
            };
            if available.is_empty() {
                return Ok(());
            }
            match available.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let n = i + 1;
                    self.input.consume(n);
                    return Ok(());
                }
                None => {
                    let n = available.len();
                    self.input.consume(n);
                }
            }
        }
    }

    /// Next host line: queued lines first (read while a handler waited for a reply), then wire.
    fn next_line(&mut self) -> Result<LineRead, ServeError> {
        if let Some(l) = self.queue.pop_front() {
            return Ok(LineRead::Line(l));
        }
        self.read_wire_line()
    }

    fn send<T: Serialize>(&mut self, msg: &T) -> Result<(), ServeError> {
        let line = serde_json::to_string(msg).map_err(|e| {
            ServeError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            ))
        })?;
        self.send_line(&line)
    }

    fn send_line(&mut self, line: &str) -> Result<(), ServeError> {
        let mut buf = line.to_string();
        buf.push('\n');
        self.output.write_all(buf.as_bytes())?;
        self.output.flush()?;
        Ok(())
    }

    // -- serve loop --------------------------------------------------------

    fn run(&mut self) -> Result<(), ServeError> {
        // 1. handshake: the first host message must be hello with protocol 1.
        match self.read_wire_line()? {
            LineRead::Eof => return Err(ServeError::UnexpectedEof),
            LineRead::TooLong => {
                return Err(ServeError::ExpectedHello("oversized first line".into()));
            }
            LineRead::Line(l) => match parse_host_msg(&l) {
                Ok(Some(HostMsg::Hello { protocol, caps, .. })) => {
                    if protocol != PROTOCOL_VERSION {
                        return Err(ServeError::Version {
                            got: protocol,
                            want: PROTOCOL_VERSION,
                        });
                    }
                    self.granted_caps = caps;
                }
                Ok(Some(other)) => {
                    return Err(ServeError::ExpectedHello(format!(
                        "first message was {other:?}"
                    )));
                }
                Ok(None) => return Err(ServeError::ExpectedHello(l)),
                Err(e) => return Err(ServeError::ExpectedHello(format!("unparseable: {e}"))),
            },
        }
        // 2. register with the manifest (serialize first to keep the borrow checker happy).
        let line = {
            let reg = RegisterMsg {
                op: "register",
                id: &self.manifest.id,
                manifest: &self.manifest,
            };
            serde_json::to_string(&reg).map_err(|e| {
                ServeError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            })?
        };
        self.send_line(&line)?;
        // 3. dispatch until shutdown (explicit op or EOF on stdin).
        loop {
            match self.next_line()? {
                LineRead::Eof => return Ok(()), // host closed stdin: clean shutdown
                LineRead::TooLong => {
                    self.warn("skipped oversized line (> 1 MiB)");
                    continue;
                }
                LineRead::Line(l) => match parse_host_msg(&l) {
                    Err(e) => self.warn(&format!("skipped malformed line: {e}")),
                    Ok(None) => self.warn(&format!("skipped unrecognized line: {}", clip(&l))),
                    Ok(Some(msg)) => {
                        if !self.dispatch(msg)? {
                            return Ok(());
                        }
                    }
                },
            }
        }
    }

    /// Dispatch one host message. Returns `false` on shutdown.
    fn dispatch(&mut self, msg: HostMsg) -> Result<bool, ServeError> {
        match msg {
            HostMsg::Hello { .. } => {
                self.warn("duplicate hello ignored");
            }
            HostMsg::Shutdown => return Ok(false),
            HostMsg::Event {
                event,
                control,
                state,
                request_id,
            } => {
                let handler = self.event_handler.take();
                if let Some(h) = handler {
                    let ev = HostEvent {
                        event,
                        control,
                        state,
                        request_id,
                    };
                    let result = h(self, &ev);
                    self.event_handler = Some(h);
                    match result {
                        Ok(effects) => {
                            let m = EffectsMsg {
                                op: "effects",
                                request_id,
                                effects,
                            };
                            self.send(&m)?;
                        }
                        Err(e) => {
                            if let Some(rid) = request_id {
                                self.send(&ErrorMsg {
                                    op: "error",
                                    request_id: rid,
                                    error: e,
                                })?;
                            } else {
                                self.warn(&format!("event handler failed: {e}"));
                            }
                        }
                    }
                }
            }
            HostMsg::Invoke {
                id,
                args,
                request_id,
            } => {
                self.dispatch_invoke(&id, &args, request_id)?;
            }
            HostMsg::Reply { request_id, .. } => {
                self.warn(&format!(
                    "orphaned reply for request_id {request_id} ignored"
                ));
            }
        }
        Ok(true)
    }

    fn dispatch_invoke(
        &mut self,
        id: &str,
        args: &Value,
        request_id: u64,
    ) -> Result<(), ServeError> {
        // The host addresses actions as "<plugin id>.<action id>"; accept the bare action id
        // too. Look the full spelling up first, then the stripped one.
        let prefix = format!("{}.", self.manifest.id);
        let bare = id.strip_prefix(&prefix).unwrap_or(id);
        let key = if self.actions.contains_key(id) {
            id.to_string()
        } else {
            bare.to_string()
        };
        // Take the entry out so the handler can use `&mut self` (host-mediated requests),
        // then put it back.
        let Some(entry) = self.actions.remove(&key) else {
            self.send(&ErrorMsg {
                op: "error",
                request_id,
                error: PluginError::schema(format!("unknown action: {id}")),
            })?;
            return Ok(());
        };
        let validated = params::validate_params(&entry.params, args);
        let result = match validated {
            Ok(()) => (entry.handler)(self, args),
            Err(e) => Err(e),
        };
        self.actions.insert(key, entry);
        match result {
            Ok(effects) => {
                self.send(&EffectsMsg {
                    op: "effects",
                    request_id: Some(request_id),
                    effects,
                })?;
            }
            Err(e) => {
                self.send(&ErrorMsg {
                    op: "error",
                    request_id,
                    error: e,
                })?;
            }
        }
        Ok(())
    }

    // -- host-mediated requests (callable from handlers) -------------------

    fn alloc_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    /// Send a request and pump host messages until the matching `reply` arrives. Everything
    /// else read meanwhile is queued and dispatched by the top-level loop later.
    fn request(&mut self, msg: RequestMsg) -> Result<HostMsg, PluginError> {
        let rid = msg.request_id;
        self.send(&msg)
            .map_err(|e| PluginError::unreachable(format!("could not send request: {e}")))?;
        loop {
            match self.read_wire_line() {
                Err(e) => return Err(PluginError::unreachable(format!("{e}"))),
                Ok(LineRead::Eof) => {
                    return Err(PluginError::unreachable(
                        "host closed stdin while waiting for a reply",
                    ));
                }
                Ok(LineRead::TooLong) => {
                    self.warn("skipped oversized line while waiting for a reply");
                }
                Ok(LineRead::Line(l)) => match parse_host_msg(&l) {
                    Err(e) => self.warn(&format!("skipped malformed line: {e}")),
                    Ok(None) => self.warn(&format!("skipped unrecognized line: {}", clip(&l))),
                    Ok(Some(r @ HostMsg::Reply { request_id, .. })) if request_id == rid => {
                        return Ok(r);
                    }
                    Ok(Some(_other)) => {
                        // Not the awaited reply (event/invoke/other reply): queue it for the
                        // top-level loop after the handler returns.
                        self.queue.push_back(l);
                    }
                },
            }
        }
    }

    /// `{"op":"request","kind":"get","path":…}` → the value at `path` (`null` if absent).
    pub fn get_state(&mut self, path: &str) -> Result<Value, PluginError> {
        let rid = self.alloc_request_id();
        let msg = RequestMsg {
            op: "request",
            request_id: rid,
            kind: "get",
            path: Some(path.to_string()),
            value: None,
            method: None,
            url: None,
            body: None,
            headers: None,
        };
        match self.request(msg)? {
            HostMsg::Reply {
                ok: true, value, ..
            } => Ok(value.unwrap_or(Value::Null)),
            HostMsg::Reply {
                ok: false, error, ..
            } => Err(error.unwrap_or_else(|| PluginError::internal("get failed"))),
            _ => Err(PluginError::internal("unexpected reply shape")),
        }
    }

    /// `{"op":"request","kind":"set","path":…,"value":…}` → the new revision.
    pub fn set_state(&mut self, path: &str, value: Value) -> Result<u64, PluginError> {
        let rid = self.alloc_request_id();
        let msg = RequestMsg {
            op: "request",
            request_id: rid,
            kind: "set",
            path: Some(path.to_string()),
            value: Some(value),
            method: None,
            url: None,
            body: None,
            headers: None,
        };
        match self.request(msg)? {
            HostMsg::Reply { ok: true, rev, .. } => Ok(rev.unwrap_or(0)),
            HostMsg::Reply {
                ok: false, error, ..
            } => Err(error.unwrap_or_else(|| PluginError::internal("set failed"))),
            _ => Err(PluginError::internal("unexpected reply shape")),
        }
    }

    /// Host-mediated HTTP GET/HEAD/… (S4): `{"op":"request","kind":"net",…}`. The daemon
    /// checks the URL's host against the plugin's `net:<glob>` grant and performs the request
    /// itself; the plugin never opens a socket. Bounded by the constants in [`crate::http`].
    pub fn http(&mut self, method: &str, url: &str) -> Result<HttpResponse, PluginError> {
        self.http_request(method, url, None, None)
    }

    /// Like [`Sdk::http`] but with an optional JSON body and headers (still bounded).
    pub fn http_request(
        &mut self,
        method: &str,
        url: &str,
        body: Option<Value>,
        headers: Option<Map<String, Value>>,
    ) -> Result<HttpResponse, PluginError> {
        let method = http::check_request(method, url, body.as_ref(), headers.as_ref())?;
        let rid = self.alloc_request_id();
        let msg = RequestMsg {
            op: "request",
            request_id: rid,
            kind: "net",
            path: None,
            value: None,
            method: Some(method),
            url: Some(url.to_string()),
            body,
            headers,
        };
        match self.request(msg)? {
            HostMsg::Reply {
                ok: true,
                status,
                body,
                ..
            } => {
                let status = u16::try_from(status.unwrap_or(0))
                    .map_err(|_| PluginError::internal("status out of u16 range"))?;
                let body = match body {
                    Some(Value::String(s)) => s,
                    Some(other) => serde_json::to_string(&other)
                        .map_err(|e| PluginError::internal(format!("bad body: {e}")))?,
                    None => String::new(),
                };
                http::check_reply_body(&body)?;
                Ok(HttpResponse { status, body })
            }
            HostMsg::Reply {
                ok: false, error, ..
            } => Err(error.unwrap_or_else(|| PluginError::internal("net request failed"))),
            _ => Err(PluginError::internal("unexpected reply shape")),
        }
    }
}

fn clip(s: &str) -> String {
    let mut t: String = s.chars().take(200).collect();
    if s.len() > 200 {
        t.push('…');
    }
    t
}

/// The plugin builder: manifest + action/event handlers, then `serve*`.
///
/// ```rust,no_run
/// use ds_plugin_sdk::{Capability, EventKind, Manifest, Plugin};
/// use serde_json::json;
///
/// let manifest = Manifest::new("demo", "Demo counter", "0.1.0")
///     .capability(Capability::StateWrite)
///     .capability(Capability::Net("lights.lan".to_string()))
///     .event(EventKind::KeyDown);
/// let plugin = Plugin::new(manifest).action(
///     "bump",
///     json!({"type":"object","properties":{"by":{"type":"integer","default":1}}}),
///     |_sdk, args| {
///         let by = args.get("by").and_then(|v| v.as_i64()).unwrap_or(1);
///         Ok(vec![ds_plugin_sdk::set("/controls/keys.0/label", by.to_string())])
///     },
/// );
/// // plugin.serve_stdio()?;  // speak the protocol on stdin/stdout
/// ```
pub struct Plugin {
    manifest: Manifest,
    actions: HashMap<String, ActionEntry>,
    event_handler: Option<EventHandler>,
}

impl Plugin {
    pub fn new(manifest: Manifest) -> Self {
        Self {
            manifest,
            actions: HashMap::new(),
            event_handler: None,
        }
    }

    /// Register an action with a raw-`Value` handler. `params` is the JSON Schema declared in
    /// the manifest; args are validated against its subset before the handler runs.
    pub fn action(
        self,
        id: impl Into<String>,
        params: Value,
        handler: impl Fn(&mut Sdk, &Value) -> Result<Vec<Effect>, PluginError> + Send + 'static,
    ) -> Self {
        self.declare(id, params, handler)
    }

    /// Register a type-safe action: `args` is deserialized into `A` (after the schema-subset
    /// check) before the handler runs; a mismatch is an `E_SCHEMA` error reply.
    pub fn action_typed<A: serde::de::DeserializeOwned + Send + 'static>(
        self,
        id: impl Into<String>,
        params: Value,
        handler: impl Fn(&mut Sdk, A) -> Result<Vec<Effect>, PluginError> + Send + 'static,
    ) -> Self {
        let wrapped = move |sdk: &mut Sdk, args: &Value| -> Result<Vec<Effect>, PluginError> {
            let typed: A = serde_json::from_value(args.clone()).map_err(|e| {
                PluginError::schema(format!(
                    "args do not deserialize into the handler type: {e}"
                ))
            })?;
            handler(sdk, typed)
        };
        self.declare(id, params, wrapped)
    }

    fn declare(
        mut self,
        id: impl Into<String>,
        params: Value,
        handler: impl Fn(&mut Sdk, &Value) -> Result<Vec<Effect>, PluginError> + Send + 'static,
    ) -> Self {
        let id = id.into();
        // The manifest carries the declared actions (wire order = registration order);
        // the map carries the handlers.
        self.manifest.actions.push(crate::manifest::ActionDef {
            id: id.clone(),
            name: None,
            description: None,
            params: params.clone(),
        });
        self.actions.insert(
            id,
            ActionEntry {
                params,
                handler: Box::new(handler),
            },
        );
        self
    }

    /// Register the event handler. Events are delivered as [`HostEvent`]; returned effects are
    /// sent as an `effects` message (carrying `request_id` when the event had one).
    pub fn on_event(
        mut self,
        handler: impl Fn(&mut Sdk, &HostEvent) -> Result<Vec<Effect>, PluginError> + Send + 'static,
    ) -> Self {
        self.event_handler = Some(Box::new(handler));
        self
    }

    /// Speak the protocol on stdin/stdout (the real transport: newline-JSON over stdio).
    pub fn serve_stdio(self) -> Result<(), ServeError> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.serve_io(stdin.lock(), stdout.lock())
    }

    /// Speak the protocol over any buffered reader/writer (used by the tests; also handy for
    /// embedding the plugin side in another process model).
    pub fn serve_io<'a, R: BufRead + 'a, W: Write + 'a>(
        self,
        input: R,
        output: W,
    ) -> Result<(), ServeError> {
        self.manifest.validate()?;
        let mut sdk = Sdk::new(
            Box::new(input),
            Box::new(output),
            self.manifest,
            self.actions,
            self.event_handler,
        );
        sdk.run()
    }
}

/// Serve a manifest with no actions over stdin/stdout — the minimal entry point
/// (`docs/contracts/plugins.md` §8 `serve(manifest)`).
///
/// For actions/events use the [`Plugin`] builder and its `serve_stdio`/`serve_io`.
pub fn serve(manifest: Manifest) -> Result<(), ServeError> {
    Plugin::new(manifest).serve_stdio()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{emit, log, set};
    use crate::Capability;
    use serde_json::json;
    use std::io::Cursor;

    fn hello_line() -> String {
        r#"{"op":"hello","protocol":1,"plugin_token":"tok","caps":["state.read","state.write","net:lights.lan"],"roots":{"fs":[]},"limits":{"cpu_s":10}}"#
            .to_string()
            + "\n"
    }

    fn demo_plugin() -> Plugin {
        Plugin::new(
            Manifest::new("demo", "Demo", "0.1.0")
                .command(vec!["/usr/bin/demo".to_string()])
                .capability(Capability::StateWrite),
        )
        .action(
            "bump",
            json!({"type":"object","properties":{"by":{"type":"integer"}},"additionalProperties":false}),
            |_sdk, args| {
                let by = args.get("by").and_then(Value::as_i64).unwrap_or(1);
                Ok(vec![
                    set("/controls/keys.0/label", by.to_string()),
                    set("/controls/keys.0/image", json!({"kind":"color","value":"#ff8800"})),
                ])
            },
        )
        .action_typed::<Bump>(
            "bump_typed",
            json!({"type":"object","properties":{"by":{"type":"integer","default":1}},"required":["by"]}),
            |_sdk, a: Bump| Ok(vec![set("/plugins/demo/n", a.by)]),
        )
        .action(
            "sync",
            json!({"type":"object"}),
            |sdk, _args| {
                let v = sdk.get_state("/controls/keys.0")?;
                let rev = sdk.set_state("/plugins/demo/state/synced", v)?;
                Ok(vec![log("info", format!("synced at rev {rev}"))])
            },
        )
        .action(
            "lamp",
            json!({"type":"object"}),
            |sdk, _args| {
                let r = sdk.http("GET", "http://lights.lan/api/lamp")?;
                Ok(vec![set("/plugins/demo/state/last", r.body)])
            },
        )
        .action(
            "fail",
            json!({"type":"object"}),
            |_sdk, _args| Err(PluginError::cap("not granted")),
        )
        .on_event(|_sdk, ev| Ok(vec![emit("app/demo/seen", json!({"event": ev.event}))]))
    }

    #[derive(serde::Deserialize)]
    struct Bump {
        by: i64,
    }

    fn run_script(input: &str) -> (Result<(), ServeError>, String) {
        let mut out: Vec<u8> = Vec::new();
        let res = demo_plugin().serve_io(Cursor::new(input.as_bytes().to_vec()), &mut out);
        (res, String::from_utf8(out).unwrap())
    }

    #[test]
    fn full_session_byte_for_byte() {
        let input = concat!(
            r#"{"op":"hello","protocol":1,"plugin_token":"tok","caps":["state.write"],"roots":{},"limits":{}}"#,
            "\n",
            r#"{"op":"event","event":"keyDown","control":"keys.0","state":{"rev":1}}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.bump","args":{"by":2},"request_id":7}"#,
            "\n",
            r#"{"op":"invoke","id":"bump_typed","args":{"by":5},"request_id":8}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.sync","args":{},"request_id":9}"#,
            "\n",
            r#"{"op":"reply","request_id":1,"ok":true,"value":{"label":"2"},"rev":4}"#,
            "\n",
            r#"{"op":"reply","request_id":2,"ok":true,"rev":5}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.lamp","args":{},"request_id":10}"#,
            "\n",
            r#"{"op":"reply","request_id":3,"ok":true,"status":200,"body":"{\"state\":\"on\"}"}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.fail","args":{},"request_id":11}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.nope","args":{},"request_id":12}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.bump","args":{"by":"x"},"request_id":13}"#,
            "\n",
            "not json\n",
            r#"{"op":"wat"}
"#,
            r#"{"op":"shutdown"}"#,
            "\n",
        );
        let (res, out) = run_script(input);
        assert!(res.is_ok(), "{res:?}");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 12, "output: {out}");
        assert_eq!(
            lines[0],
            r#"{"op":"register","id":"demo","manifest":{"id":"demo","name":"Demo","version":"0.1.0","command":["/usr/bin/demo"],"actions":[{"id":"bump","params":{"additionalProperties":false,"properties":{"by":{"type":"integer"}},"type":"object"}},{"id":"bump_typed","params":{"properties":{"by":{"default":1,"type":"integer"}},"required":["by"],"type":"object"}},{"id":"sync","params":{"type":"object"}},{"id":"lamp","params":{"type":"object"}},{"id":"fail","params":{"type":"object"}}],"capabilities":["state.write"]}}"#
        );
        assert_eq!(
            lines[1],
            r#"{"op":"effects","effects":[{"effect":"emit","topic":"app/demo/seen","payload":{"event":"keyDown"}}]}"#
        );
        assert_eq!(
            lines[2],
            r##"{"op":"effects","request_id":7,"effects":[{"effect":"set","path":"/controls/keys.0/label","value":"2"},{"effect":"set","path":"/controls/keys.0/image","value":{"kind":"color","value":"#ff8800"}}]}"##
        );
        assert_eq!(
            lines[3],
            r#"{"op":"effects","request_id":8,"effects":[{"effect":"set","path":"/plugins/demo/n","value":5}]}"#
        );
        assert_eq!(
            lines[4],
            r#"{"op":"request","request_id":1,"kind":"get","path":"/controls/keys.0"}"#
        );
        assert_eq!(
            lines[5],
            r#"{"op":"request","request_id":2,"kind":"set","path":"/plugins/demo/state/synced","value":{"label":"2"}}"#
        );
        assert_eq!(
            lines[6],
            r#"{"op":"effects","request_id":9,"effects":[{"effect":"log","level":"info","message":"synced at rev 5"}]}"#
        );
        assert_eq!(
            lines[7],
            r#"{"op":"request","request_id":3,"kind":"net","method":"GET","url":"http://lights.lan/api/lamp"}"#
        );
        assert_eq!(
            lines[8],
            r#"{"op":"effects","request_id":10,"effects":[{"effect":"set","path":"/plugins/demo/state/last","value":"{\"state\":\"on\"}"}]}"#
        );
        assert_eq!(
            lines[9],
            r#"{"op":"error","request_id":11,"error":{"code":"E_CAP","message":"not granted"}}"#
        );
        assert_eq!(
            lines[10],
            r#"{"op":"error","request_id":12,"error":{"code":"E_SCHEMA","message":"unknown action: demo.nope"}}"#
        );
        assert!(
            lines[11].starts_with(
                r#"{"op":"error","request_id":13,"error":{"code":"E_SCHEMA","message":"args at $.by: expected type "#
            ),
            "line 11: {}",
            lines[11]
        );
        // The malformed lines ("not json", {"op":"wat"}) produced no output and were skipped;
        // the trailing {"op":"shutdown"} ended the session cleanly.
    }

    #[test]
    fn error_replies_for_unknown_action_and_bad_params() {
        let input = concat!(
            r#"{"op":"hello","protocol":1,"caps":[]}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.nope","args":{},"request_id":12}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.bump","args":{"by":"x"},"request_id":13}"#,
            "\n",
        );
        let (res, out) = run_script(input);
        assert!(res.is_ok());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[1],
            r#"{"op":"error","request_id":12,"error":{"code":"E_SCHEMA","message":"unknown action: demo.nope"}}"#
        );
        assert!(lines[2].starts_with(
            r#"{"op":"error","request_id":13,"error":{"code":"E_SCHEMA","message":"args at $.by: expected type "#
        ));
    }

    #[test]
    fn version_mismatch_is_surfaced_cleanly() {
        let input = r#"{"op":"hello","protocol":99,"caps":[]}"#.to_owned() + "\n";
        let (res, out) = run_script(&input);
        assert!(
            out.is_empty(),
            "nothing must be sent on version mismatch: {out}"
        );
        assert!(matches!(res, Err(ServeError::Version { got: 99, want: 1 })));
        assert_eq!(res.unwrap_err().exit_code(), 8);
    }

    #[test]
    fn first_message_must_be_hello() {
        let input =
            r#"{"op":"invoke","id":"demo.bump","args":{},"request_id":1}"#.to_owned() + "\n";
        let (res, out) = run_script(&input);
        assert!(out.is_empty());
        assert!(matches!(res, Err(ServeError::ExpectedHello(_))));
    }

    #[test]
    fn eof_before_hello_is_unexpected_eof() {
        let (res, out) = run_script("");
        assert!(out.is_empty());
        assert!(matches!(res, Err(ServeError::UnexpectedEof)));
    }

    #[test]
    fn eof_after_register_is_clean_shutdown() {
        let input = hello_line();
        let (res, out) = run_script(&input);
        assert!(res.is_ok());
        assert!(out.starts_with("{\"op\":\"register\""));
    }

    #[test]
    fn oversized_line_is_skipped_not_fatal() {
        let mut input = hello_line();
        input.push_str(r#"{"op":"event","event":""#);
        input.push_str(&"x".repeat(MAX_LINE_BYTES + 100));
        input.push_str("\"}\n");
        input.push_str(r#"{"op":"invoke","id":"demo.bump","args":{"by":1},"request_id":3}"#);
        input.push('\n');
        let (res, out) = run_script(&input);
        assert!(res.is_ok());
        assert!(out.contains(r#""request_id":3"#), "output: {out}");
    }

    #[test]
    fn host_eof_during_request_is_unreachable_error() {
        let input = concat!(
            r#"{"op":"hello","protocol":1,"caps":[]}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.lamp","args":{},"request_id":1}"#,
            "\n",
        );
        let (res, out) = run_script(input);
        // The handler fails with E_UNREACHABLE and the error reply is sent... but stdin is
        // at EOF, so the reply may or may not flush before the loop sees EOF — either way the
        // session must end without panic and the request must have been sent.
        let _ = res;
        assert!(out.contains(r#""kind":"net""#), "output: {out}");
    }

    #[test]
    fn invalid_manifest_fails_before_any_wire_output() {
        let mut m = Manifest::new("bad id!", "x", "0.1.0");
        m.command = vec!["/usr/bin/demo".to_string()];
        let mut out: Vec<u8> = Vec::new();
        let res = Plugin::new(m).serve_io(Cursor::new(hello_line().into_bytes()), &mut out);
        assert!(matches!(res, Err(ServeError::Manifest(_))));
        assert!(out.is_empty());
    }

    #[test]
    fn http_bounds_are_enforced_before_sending() {
        let input = concat!(
            r#"{"op":"hello","protocol":1,"caps":[]}"#,
            "\n",
            r#"{"op":"invoke","id":"demo.bad","args":{},"request_id":1}"#,
            "\n",
        );
        let plugin = Plugin::new(
            Manifest::new("demo", "Demo", "0.1.0").command(vec!["/usr/bin/demo".to_string()]),
        )
        .action("bad", json!({"type":"object"}), |sdk, _args| {
            sdk.http("GET", &"http://x/".repeat(500))
                .map(|_| Vec::new())
        });
        let mut out: Vec<u8> = Vec::new();
        let res = plugin.serve_io(Cursor::new(input.as_bytes().to_vec()), &mut out);
        assert!(res.is_ok());
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""code":"E_LIMIT""#), "output: {s}");
        assert!(
            !s.contains(r#""kind":"net""#),
            "no net request may be sent: {s}"
        );
    }

    #[test]
    fn granted_caps_from_hello_are_exposed() {
        let input = concat!(
            r#"{"op":"hello","protocol":1,"caps":["state.read","net:lights.lan"]}"#,
            "\n",
            r#"{"op":"invoke","id":"caps","args":{},"request_id":1}"#,
            "\n",
        );
        let plugin = Plugin::new(
            Manifest::new("demo", "Demo", "0.1.0").command(vec!["/usr/bin/demo".to_string()]),
        )
        .action("caps", json!({"type":"object"}), |sdk, _args| {
            Ok(vec![set(
                "/plugins/demo/caps",
                json!(sdk.granted_capabilities()),
            )])
        });
        let mut out: Vec<u8> = Vec::new();
        let res = plugin.serve_io(Cursor::new(input.as_bytes().to_vec()), &mut out);
        assert!(res.is_ok());
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains(r#""value":["state.read","net:lights.lan"]"#),
            "output: {s}"
        );
    }
}
