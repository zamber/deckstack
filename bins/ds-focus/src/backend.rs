//! Focus backends behind a small trait, plus autodetection.
//!
//! - **kwin** — KDE Plasma (Wayland or X11): D-Bus `org.kde.KWin` at `/KWin`, interface
//!   `org.kde.KWin`, method `queryWindowInfo` → `a{sv}` (the active-window interface,
//!   verified present on this Bazzite Plasma 6 session with `busctl --user introspect`;
//!   the `/Scripting` path also exists but needs a script file on disk, so polling the
//!   active-window method at `--interval` is the chosen mechanism). Keys read:
//!   `resourceClass`/`class` (wm-class), `caption`/`name` (title), `pid`.
//! - **wlr** — wlroots compositors (sway, Hyprland, river, …): the Wayland global
//!   `zwlr_foreign_toplevel_manager_v1` (`wayland-protocols-wlr`), whose handle `state`
//!   event carries the `activated` flag. Note: `ext-foreign-toplevel-list-v1` (staging)
//!   has NO activation state (only title/app_id/identifier/closed), so it cannot drive
//!   focus detection on its own — that is the measured answer to OPEN O1 on wlroots.
//!
//! `x11` is accepted as a `--backend` value but not built in the Linux alpha (Wayland/
//! KDE-first, `docs/40-platforms.md` §1): selecting it exits 3 with the list of backends
//! actually available on the session.

use crate::args::Backend as BackendArg;
use crate::focus::Focus;
use std::collections::HashMap;

/// One backend poll can fail transiently (compositor busy); the watch loop logs and
/// retries instead of exiting.
#[derive(Debug)]
pub struct BackendError(pub String);

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A focus source.
pub trait Backend {
    /// Backend name (`kwin` / `wlr`).
    fn name(&self) -> &'static str;
    /// Human-readable detail for the startup log (e.g. the D-Bus path).
    fn detail(&self) -> String;
    /// Poll the focused window once. `Ok(None)` = no focused window right now.
    fn poll(&mut self) -> Result<Option<Focus>, BackendError>;
}

/// The selected backend.
pub enum AnyBackend {
    Kwin(KwinBackend),
    Wlr(WlrBackend),
}

impl Backend for AnyBackend {
    fn name(&self) -> &'static str {
        match self {
            Self::Kwin(b) => b.name(),
            Self::Wlr(b) => b.name(),
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Kwin(b) => b.detail(),
            Self::Wlr(b) => b.detail(),
        }
    }

    fn poll(&mut self) -> Result<Option<Focus>, BackendError> {
        match self {
            Self::Kwin(b) => b.poll(),
            Self::Wlr(b) => b.poll(),
        }
    }
}

/// Snapshot of the session-relevant environment, for a pure, testable detection rule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfo {
    pub session_type: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub kde: bool,
}

impl SessionInfo {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let desktop = var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        Self {
            session_type: var("XDG_SESSION_TYPE"),
            wayland_display: var("WAYLAND_DISPLAY"),
            display: var("DISPLAY"),
            kde: var("KDE_FULL_SESSION").as_deref() == Some("true") || desktop.contains("KDE"),
        }
    }

    pub fn is_wayland(&self) -> bool {
        self.session_type.as_deref() == Some("wayland") || self.wayland_display.is_some()
    }
}

/// The pure autodetect rule. `kwin_up` = the session bus answers and owns `org.kde.KWin`;
/// `wlr_up` = the Wayland registry advertises `zwlr_foreign_toplevel_manager_v1`.
///
/// KDE sessions prefer the KWin backend (proven path, works on Wayland and X11); other
/// Wayland sessions use wlr; anything else has no usable backend.
pub fn detect(s: &SessionInfo, kwin_up: bool, wlr_up: bool) -> Option<BackendArg> {
    if s.kde && kwin_up {
        return Some(BackendArg::Kwin);
    }
    if s.is_wayland() && wlr_up {
        return Some(BackendArg::Wlr);
    }
    if kwin_up && !s.is_wayland() {
        // non-Wayland session with KWin running (KDE on X11): the D-Bus backend works
        return Some(BackendArg::Kwin);
    }
    None
}

/// Backends usable on this session right now (for the exit-3 message).
pub fn available_backends() -> Vec<&'static str> {
    let mut out = Vec::new();
    if KwinBackend::probe() {
        out.push("kwin");
    }
    if WlrBackend::probe() {
        out.push("wlr");
    }
    out
}

/// Select and construct the backend for `--backend <name>`. `Auto` probes the session.
///
/// Errors:
/// - requested backend known but not usable here → `SelectError::Unavailable(name, available)`
/// - `x11` → not built in the alpha → `SelectError::NotBuilt("x11", available)`
pub fn select(requested: BackendArg) -> Result<AnyBackend, SelectError> {
    let session = SessionInfo::from_env();
    let kwin_up = KwinBackend::probe();
    let wlr_up = WlrBackend::probe();
    match requested {
        BackendArg::Auto => match detect(&session, kwin_up, wlr_up) {
            Some(BackendArg::Kwin) => Ok(AnyBackend::Kwin(
                KwinBackend::new().map_err(SelectError::Init)?,
            )),
            Some(BackendArg::Wlr) => Ok(AnyBackend::Wlr(
                WlrBackend::new().map_err(SelectError::Init)?,
            )),
            _ => Err(SelectError::Unavailable(
                "auto".to_string(),
                available_backends(),
            )),
        },
        BackendArg::Kwin => {
            if !kwin_up {
                return Err(SelectError::Unavailable(
                    "kwin".to_string(),
                    available_backends(),
                ));
            }
            Ok(AnyBackend::Kwin(
                KwinBackend::new().map_err(SelectError::Init)?,
            ))
        }
        BackendArg::Wlr => {
            if !wlr_up {
                return Err(SelectError::Unavailable(
                    "wlr".to_string(),
                    available_backends(),
                ));
            }
            Ok(AnyBackend::Wlr(
                WlrBackend::new().map_err(SelectError::Init)?,
            ))
        }
        BackendArg::X11 => Err(SelectError::NotBuilt(
            "x11".to_string(),
            available_backends(),
        )),
    }
}

#[derive(Debug)]
pub enum SelectError {
    /// The backend exists in principle but is not usable on this session.
    Unavailable(String, Vec<&'static str>),
    /// Not compiled in this alpha build.
    NotBuilt(String, Vec<&'static str>),
    /// Construction failed after the probe succeeded.
    Init(String),
}

impl SelectError {
    /// The log/exit-3 message: names the problem and lists the available backends.
    pub fn message(&self) -> String {
        match self {
            Self::Unavailable(name, avail) => format!(
                "backend '{name}' is not available on this session (available: {})",
                if avail.is_empty() {
                    "none".to_string()
                } else {
                    avail.join(", ")
                }
            ),
            Self::NotBuilt(name, avail) => format!(
                "backend '{name}' is not built in this alpha (Linux alpha is Wayland/KDE-first, docs/40-platforms.md §1; available here: {})",
                if avail.is_empty() {
                    "none".to_string()
                } else {
                    avail.join(", ")
                }
            ),
            Self::Init(e) => format!("backend init failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------- KWin (D-Bus) ---

pub struct KwinBackend {
    proxy: zbus::blocking::Proxy<'static>,
}

const KWIN_DEST: &str = "org.kde.KWin";
const KWIN_PATH: &str = "/KWin";
const KWIN_IFACE: &str = "org.kde.KWin";

impl KwinBackend {
    /// Probe: session bus reachable AND `org.kde.KWin` has an owner.
    pub fn probe() -> bool {
        Self::new().is_ok()
    }

    pub fn new() -> Result<Self, String> {
        let conn = zbus::blocking::Connection::session()
            .map_err(|e| format!("cannot connect to the session bus: {e}"))?;
        let dbus = zbus::blocking::fdo::DBusProxy::new(&conn)
            .map_err(|e| format!("cannot create the org.freedesktop.DBus proxy: {e}"))?;
        let name: zbus::names::BusName = KWIN_DEST
            .try_into()
            .map_err(|e| format!("bad bus name: {e}"))?;
        let owned = dbus
            .name_has_owner(name)
            .map_err(|e| format!("NameHasOwner failed: {e}"))?;
        if !owned {
            return Err(format!("{KWIN_DEST} has no owner on the session bus"));
        }
        let proxy = zbus::blocking::Proxy::new(&conn, KWIN_DEST, KWIN_PATH, KWIN_IFACE)
            .map_err(|e| format!("cannot create the KWin proxy: {e}"))?;
        Ok(Self { proxy })
    }

    /// Map the `queryWindowInfo` result to a Focus. KWin's map keys (checked in order):
    /// wm-class `resourceClass`/`class`/`resourceName`, title `caption`/`name`/`title`,
    /// `pid`. (`OwnedValue` derefs to `Value`, so the plain accessors work.)
    fn parse_info(map: &HashMap<String, zbus::zvariant::OwnedValue>) -> Option<Focus> {
        let str_of = |keys: &[&str]| -> Option<String> {
            keys.iter().find_map(|k| {
                map.get(*k).and_then(|v| match &**v {
                    zbus::zvariant::Value::Str(s) => Some(s.to_string()),
                    _ => None,
                })
            })
        };
        let app = str_of(&["resourceClass", "class", "resourceName"])?;
        if app.is_empty() {
            return None;
        }
        let title = str_of(&["caption", "name", "title"]).unwrap_or_default();
        let pid = map.get("pid").and_then(|v| match &**v {
            zbus::zvariant::Value::I64(i) => u64::try_from(*i).ok(),
            zbus::zvariant::Value::U32(u) => Some(u64::from(*u)),
            zbus::zvariant::Value::U64(u) => Some(*u),
            _ => None,
        });
        Some(Focus { app, title, pid })
    }
}

impl Backend for KwinBackend {
    fn name(&self) -> &'static str {
        "kwin"
    }

    fn detail(&self) -> String {
        format!("D-Bus {KWIN_DEST} {KWIN_PATH} {KWIN_IFACE}.queryWindowInfo")
    }

    fn poll(&mut self) -> Result<Option<Focus>, BackendError> {
        match self
            .proxy
            .call::<_, _, HashMap<String, zbus::zvariant::OwnedValue>>("queryWindowInfo", &())
        {
            Ok(map) => Ok(Self::parse_info(&map)),
            Err(e) => {
                let msg = e.to_string();
                // KWin answers UserCancel when no window is active right now — that is
                // "no focus", not a backend failure.
                if msg.contains("UserCancel") || msg.contains("cancelled") {
                    Ok(None)
                } else {
                    Err(BackendError(format!("queryWindowInfo: {msg}")))
                }
            }
        }
    }
}

// ------------------------------------------------- wlr-foreign-toplevel (Wayland) ---

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::wl_registry,
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

#[derive(Default)]
struct WlrState {
    /// handle id -> (app_id, title, activated), committed on `done`
    toplevels: HashMap<u32, TopState>,
    /// handles seen since the last roundtrip, in arrival order
    order: Vec<u32>,
    /// pending (not yet done) values per handle
    pending: HashMap<u32, TopState>,
    last_activated: Option<u32>,
}

#[derive(Default, Clone)]
struct TopState {
    app_id: String,
    title: String,
    activated: bool,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WlrState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            let id = toplevel.id().protocol_id();
            state.order.push(id);
            state.pending.insert(id, TopState::default());
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id().protocol_id();
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                state.pending.entry(id).or_default().app_id = app_id;
            }
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                state.pending.entry(id).or_default().title = title;
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: st } => {
                let active = st.iter().any(|s| {
                    u32::from(*s) == zwlr_foreign_toplevel_handle_v1::State::Activated as u32
                });
                state.pending.entry(id).or_default().activated = active;
                if active {
                    state.last_activated = Some(id);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => {
                if let Some(p) = state.pending.remove(&id) {
                    state.toplevels.insert(id, p);
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.toplevels.remove(&id);
                state.pending.remove(&id);
                state.order.retain(|o| *o != id);
                if state.last_activated == Some(id) {
                    state.last_activated = None;
                }
            }
            _ => {}
        }
    }
}

pub struct WlrBackend {
    queue: EventQueue<WlrState>,
    state: WlrState,
    #[allow(dead_code)]
    manager: ZwlrForeignToplevelManagerV1,
}

impl WlrBackend {
    /// Probe: a Wayland connection is possible and the registry advertises the global.
    pub fn probe() -> bool {
        Self::new().is_ok()
    }

    pub fn new() -> Result<Self, String> {
        let conn = Connection::connect_to_env()
            .map_err(|e| format!("cannot connect to the Wayland display: {e}"))?;
        let (globals, queue) =
            registry_queue_init::<WlrState>(&conn).map_err(|e| format!("registry init: {e}"))?;
        let qh = queue.handle();
        let manager = globals
            .bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ())
            .map_err(|e| format!("no zwlr_foreign_toplevel_manager_v1 global: {e}"))?;
        Ok(Self {
            queue,
            state: WlrState::default(),
            manager,
        })
    }

    /// The currently activated toplevel (last one that reported `activated`).
    fn current(&self) -> Option<Focus> {
        let id = self.state.last_activated?;
        let t = self.state.toplevels.get(&id)?;
        if !t.activated {
            return None;
        }
        if t.app_id.is_empty() {
            return None;
        }
        Some(Focus {
            app: t.app_id.clone(),
            title: t.title.clone(),
            pid: None,
        })
    }
}

impl Backend for WlrBackend {
    fn name(&self) -> &'static str {
        "wlr"
    }

    fn detail(&self) -> String {
        "Wayland zwlr_foreign_toplevel_manager_v1 (wlr-foreign-toplevel)".to_string()
    }

    fn poll(&mut self) -> Result<Option<Focus>, BackendError> {
        // one roundtrip processes all pending toplevel events (blocking sync)
        self.queue
            .roundtrip(&mut self.state)
            .map_err(|e| BackendError(format!("wayland roundtrip: {e}")))?;
        Ok(self.current())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(wayland: bool, kde: bool) -> SessionInfo {
        SessionInfo {
            session_type: Some(if wayland { "wayland" } else { "x11" }.to_string()),
            wayland_display: wayland.then(|| "wayland-1".to_string()),
            display: if wayland {
                None
            } else {
                Some(":0".to_string())
            },
            kde,
        }
    }

    #[test]
    fn detect_prefers_kwin_on_kde() {
        let s = session(true, true);
        assert_eq!(detect(&s, true, true), Some(BackendArg::Kwin));
        assert_eq!(detect(&s, true, false), Some(BackendArg::Kwin));
        // non-KDE wayland with both up -> wlr
        let s = session(true, false);
        assert_eq!(detect(&s, true, true), Some(BackendArg::Wlr));
        assert_eq!(detect(&s, false, true), Some(BackendArg::Wlr));
        // KDE wayland without the bus -> falls back to wlr if present
        assert_eq!(detect(&s, false, true), Some(BackendArg::Wlr));
        // KDE X11 session with KWin on the bus -> kwin (D-Bus works on X11 too)
        let s = session(false, true);
        assert_eq!(detect(&s, true, false), Some(BackendArg::Kwin));
        // nothing usable
        assert_eq!(detect(&s, false, false), None);
        let s = session(true, false);
        assert_eq!(detect(&s, false, false), None);
    }

    #[test]
    fn kwin_map_parsing() {
        let mut map = HashMap::new();
        map.insert(
            "resourceClass".to_string(),
            zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("code")),
        );
        map.insert(
            "caption".to_string(),
            zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("PLAN.md")),
        );
        map.insert(
            "pid".to_string(),
            zbus::zvariant::OwnedValue::from(4242_i64),
        );
        let f = KwinBackend::parse_info(&map).expect("parses");
        assert_eq!(
            f,
            Focus {
                app: "code".into(),
                title: "PLAN.md".into(),
                pid: Some(4242)
            }
        );
        // alternate key spellings
        let mut map = HashMap::new();
        map.insert(
            "class".to_string(),
            zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("kitty")),
        );
        map.insert(
            "name".to_string(),
            zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("scratch")),
        );
        let f = KwinBackend::parse_info(&map).unwrap();
        assert_eq!(f.app, "kitty");
        assert_eq!(f.title, "scratch");
        assert_eq!(f.pid, None);
        // no class -> no focus
        let map = HashMap::new();
        assert!(KwinBackend::parse_info(&map).is_none());
        // empty class -> no focus
        let mut map = HashMap::new();
        map.insert(
            "class".to_string(),
            zbus::zvariant::OwnedValue::from(zbus::zvariant::Str::from("")),
        );
        assert!(KwinBackend::parse_info(&map).is_none());
    }

    #[test]
    fn select_error_messages_list_available_backends() {
        let e = SelectError::Unavailable("wlr".to_string(), vec!["kwin"]);
        let m = e.message();
        assert!(m.contains("'wlr' is not available"), "{m}");
        assert!(m.contains("available: kwin"), "{m}");
        let e = SelectError::NotBuilt("x11".to_string(), vec![]);
        let m = e.message();
        assert!(m.contains("'x11' is not built"), "{m}");
        assert!(m.contains("available here: none"), "{m}");
        let e = SelectError::Init("boom".to_string());
        assert_eq!(e.message(), "backend init failed: boom");
    }
}
