//! The device task: owns the `ds-device` [`Transport`] opened from the device URI
//! (`virtual:<path>` → ds-vdev; `none` → no hardware) and applies committed state changes
//! to the device (DS-095):
//!
//! - `/controls/keys/image`, `/controls/keys/<0-14>/image`, `/controls/dial/image` with a
//!   displayable visual push **image source** as chunked image reports — the panel goes out
//!   as `02 35` (0x35), key `N` as `02 36 <index N>` (0x36), the dial as `02 45` (0x45), each
//!   chunk `≤ 312 B` inside a 320 B output report, built by [`crate::frames::chunk`] — no
//!   second chunker exists here:
//!   - `{"kind":"file","path":…}` → the file's bytes, read on the device task (never under
//!     the state lock), after the `/policy/fs/roots` confinement + 16 MiB size cap check;
//!     a file outside the roots is refused with a logged error, never a panic;
//!   - `{"kind":"color","value":"#rrggbb"}` → a solid screen of the target's native size.
//!
//! Both sources are then **re-encoded to the device wire format** by [`crate::frames::encode`]
//! (baseline JPEG, standard Huffman tables, 4:4:4, q80 panel / q90 key:N / q85 dial —
//! DS-030, `docs/contracts/streaming.md` §2) and chunked by [`crate::frames::chunk`].
//! An input the encoder refuses (unsupported container, progressive or optimized-Huffman
//! JPEG, over the decode caps) fails the push with `E_FORMAT` and is dropped (counted),
//! never forwarded.
//! - `/controls/{knobs,faders,rollers}/<n>/led` color → `led_color` (feature report), paced
//!   by the LED rate cap.
//!
//! Everything else (icons, text, `kind:frame` references, brightness) is out of scope and
//! only logged. The task receives pushes over a bounded channel, so the state task never
//! blocks on device I/O; the file read and every chunk write happen on this task's thread,
//! and a failed push is logged and dropped — it never stalls the state task or the IPC.
//!
//! On connect, keys/dial get the software-mode handshake first — commands only land on
//! those modules in software mode (mh-protocol safety invariant).
//!
//! Input reports from the device (key/fader/dial/knob/roller, or injected by
//! `ds-vdevctl inject`, DS-017) are decoded by the router ([`crate::router::InputDecoder`])
//! into `deck/control/<control>/<gesture>` events published on the event bus (DS-040), and
//! connect/disconnect become `deck/device/<serial>/<event>` events. Publishing only
//! enqueues into bounded per-subscriber queues, so the device reader never blocks on a
//! slow subscriber.

use crate::lifecycle::{self, Shutdown};
use crate::router::{self, Bus};
use ds_device::module::module_info;
use ds_device::rate::{RateLimiter, DEFAULT_LED_RATE_PER_S};
use ds_device::transport::{DeviceEvent, Transport};
use ds_device::uri::DeviceUri;
use ds_device::vtransport::virtual_module_id;
use ds_protocol::{LedTarget, ModuleKind};
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Capacity of the device-push channel (bounded; overflow drops and is counted, never
/// blocks the state task — S16 spirit).
pub const DEVICE_CHANNEL_CAP: usize = 64;

/// How long the device loop waits on the push channel before re-checking shutdown.
const POLL: Duration = Duration::from_millis(100);

/// Short transport poll used while no push is pending, so a push that arrives while the
/// loop is inside a transport read is still handled within this bound (pushes are the hot
/// path: a committed change must reach the device promptly, e.g. before the next
/// `ds-vdevctl frames` call).
const TRANSPORT_POLL: Duration = Duration::from_millis(5);

/// Largest `kind:file` image the daemon reads and pushes: 16 MiB, the documented wire cap
/// for image payloads (`docs/contracts/streaming.md` §2/§5: `≤ 16 MB`).
pub const MAX_IMAGE_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Image-chunk write pacing: the device accepts ~2.1 MB/s (`upstream/PROTOCOL.md:265`, see
/// [`ds_device::rate`]), i.e. ≈ 6562 output reports of 320 B per second. Chunks of one
/// transfer are paced with a token bucket at that cap — the in-flight transfer is never
/// dropped mid-sequence (streaming.md §3).
pub const IMAGE_WRITE_RATE_PER_S: u32 = 6562;

/// One state change the device task should apply.
#[derive(Debug, Clone)]
pub enum DevicePush {
    /// A committed `/controls/...` change (path + new value).
    State { path: String, value: Value },
}

/// A display target in the streaming vocabulary (`docs/contracts/streaming.md` §1):
/// the keys panel, one key, or the dial screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageTarget {
    /// The whole keys panel (`02 35`, index 0).
    Panel,
    /// One key (`02 36`, index N).
    Key(u8),
    /// The dial screen (`02 45`).
    Dial,
}

impl ImageTarget {
    /// The module the image reports are written to.
    pub fn module(self) -> ModuleKind {
        match self {
            ImageTarget::Panel | ImageTarget::Key(_) => ModuleKind::Keys,
            ImageTarget::Dial => ModuleKind::Dial,
        }
    }

    /// The same target in the frame plane's vocabulary (`crate::frames::FrameTarget`):
    /// wire command, index, quality and native screen size for the encode step.
    pub fn frame_target(self) -> crate::frames::FrameTarget {
        match self {
            ImageTarget::Panel => crate::frames::FrameTarget::Panel,
            ImageTarget::Key(n) => crate::frames::FrameTarget::Key(n),
            ImageTarget::Dial => crate::frames::FrameTarget::Dial,
        }
    }
}

/// Where the bytes of an image push come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// `{"kind":"file","path":…}` — read the file (roots-confined, size-capped).
    File { path: String },
    /// `{"kind":"color","value":"#rrggbb"}` — a minimal solid-colour PNG.
    Color { rgb: [u8; 3] },
}

/// What a push translates to on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAction {
    /// Push image bytes to a display target as chunked image reports (0x35/0x36/0x45).
    Image {
        target: ImageTarget,
        source: ImageSource,
    },
    /// LED color on an M0-family module (knobs/faders/rollers).
    LedColor { kind: ModuleKind, rgb: [u8; 3] },
}

/// The per-device write pacing of the device task: one token bucket for LED/control writes
/// (`/policy/led_rate_per_s` default) and one for image chunks (the device's ~2.1 MB/s cap).
pub struct RateLimits {
    led: RateLimiter,
    image: RateLimiter,
}

impl RateLimits {
    pub fn new() -> Self {
        RateLimits {
            led: RateLimiter::new(DEFAULT_LED_RATE_PER_S),
            image: RateLimiter::new(IMAGE_WRITE_RATE_PER_S),
        }
    }
}

impl Default for RateLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse `#rrggbb` (the only color spelling `schemas/state.json` allows).
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([
        ((v >> 16) & 0xff) as u8,
        ((v >> 8) & 0xff) as u8,
        (v & 0xff) as u8,
    ])
}

fn led_color_of(value: &Value) -> Option<[u8; 3]> {
    match value.get("color")? {
        Value::String(s) => parse_hex_color(s),
        Value::Array(a) if a.len() == 3 => {
            let mut rgb = [0u8; 3];
            for (i, c) in a.iter().enumerate() {
                rgb[i] = c.as_u64().unwrap_or(0).min(255) as u8;
            }
            Some(rgb)
        }
        _ => None,
    }
}

/// Translate a visual value into an image source; `None` for the visuals this ticket does
/// not push (icon/text/frame — logged by the caller).
fn plan_image_source(value: &Value) -> Option<ImageSource> {
    match value.get("kind").and_then(Value::as_str)? {
        "file" => Some(ImageSource::File {
            path: value.get("path")?.as_str()?.to_string(),
        }),
        "color" => Some(ImageSource::Color {
            rgb: parse_hex_color(value.get("value")?.as_str()?)?,
        }),
        _ => None,
    }
}

/// Translate one state change into a device action; `None` when out of scope (logged by
/// the caller). Display-target `image` visuals (file and colour) become chunked image
/// pushes; LED colors stay feature reports.
pub fn plan_push(path: &str, value: &Value) -> Option<DeviceAction> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segs.as_slice() {
        ["controls", "keys", "image"] => {
            plan_image_source(value).map(|source| DeviceAction::Image {
                target: ImageTarget::Panel,
                source,
            })
        }
        ["controls", "keys", n, "image"] => {
            let index: u8 = n.parse().ok()?;
            if index >= 15 {
                return None;
            }
            plan_image_source(value).map(|source| DeviceAction::Image {
                target: ImageTarget::Key(index),
                source,
            })
        }
        ["controls", "dial", "image"] => {
            plan_image_source(value).map(|source| DeviceAction::Image {
                target: ImageTarget::Dial,
                source,
            })
        }
        ["controls", kind, _n, "led"] => {
            let kind = match *kind {
                "knobs" => ModuleKind::Knobs,
                "faders" => ModuleKind::Faders,
                "rollers" => ModuleKind::Rollers,
                _ => return None,
            };
            let rgb = led_color_of(value)?;
            Some(DeviceAction::LedColor { kind, rgb })
        }
        _ => None,
    }
}

/// The daemon's persisted state document, located from this process's own `--data-dir`
/// argument (else the default data-dir resolution). The state task forwards only
/// `/controls/*` pushes to the device task, so the fs policy is read from the persisted
/// document — ds-state persists synchronously inside `commit`, so it is current whenever a
/// push arrives.
fn state_doc_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    for (i, arg) in args.iter().enumerate() {
        if arg == "--data-dir" {
            if let Some(v) = args.get(i + 1) {
                return PathBuf::from(v).join("state.json");
            }
        } else if let Some(v) = arg.strip_prefix("--data-dir=") {
            return PathBuf::from(v).join("state.json");
        }
    }
    crate::lifecycle::resolve_data_dir(None).join("state.json")
}

/// Roots a `kind:file` visual may be read from: `/policy/fs/roots` of the persisted state
/// document plus the system temp dir (`/tmp`, honoring `TMPDIR`) — `docs/contracts/state.md`
/// §3 / `docs/contracts/cli.md`: file paths are confined to the roots + /tmp.
pub fn fs_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp"), std::env::temp_dir()];
    if let Ok(text) = fs::read_to_string(state_doc_path()) {
        if let Ok(doc) = serde_json::from_str::<Value>(&text) {
            if let Some(arr) = doc.pointer("/policy/fs/roots").and_then(Value::as_array) {
                for root in arr.iter().filter_map(Value::as_str) {
                    let p = PathBuf::from(root);
                    if !roots.contains(&p) {
                        roots.push(p);
                    }
                }
            }
        }
    }
    roots
}

/// Whether `canon` (an already-canonicalized path) is inside one of `roots`.
fn inside_roots(canon: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| {
        fs::canonicalize(root)
            .map(|c| canon.starts_with(&c))
            .unwrap_or(false)
    })
}

/// Read a `kind:file` image: canonicalize, confine to the fs roots, enforce the size cap,
/// then read the bytes. Every failure is an `Err` (logged by the caller) — never a panic.
pub fn read_image_file(path: &str) -> Result<Vec<u8>, String> {
    let canon = fs::canonicalize(path)
        .map_err(|e| format!("cannot canonicalize image file {path}: {e}"))?;
    let roots = fs_roots();
    if !inside_roots(&canon, &roots) {
        return Err(format!(
            "image file {path} is outside /policy/fs/roots {roots:?} — refusing to push"
        ));
    }
    let meta = fs::metadata(&canon).map_err(|e| format!("cannot stat image file {path}: {e}"))?;
    if meta.len() as usize > MAX_IMAGE_FILE_BYTES {
        return Err(format!(
            "image file {path} is {} bytes, over the {MAX_IMAGE_FILE_BYTES}-byte cap",
            meta.len()
        ));
    }
    let file = fs::File::open(&canon).map_err(|e| format!("cannot open image file {path}: {e}"))?;
    let mut buf = Vec::new();
    file.take((MAX_IMAGE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read image file {path}: {e}"))?;
    if buf.len() > MAX_IMAGE_FILE_BYTES {
        return Err(format!(
            "image file {path} exceeds the {MAX_IMAGE_FILE_BYTES}-byte cap"
        ));
    }
    Ok(buf)
}

/// Encode one image source to the device wire format (baseline JPEG, standard Huffman,
/// 4:4:4, target quality — DS-030) for `target`. Runs on the device task's thread — never
/// under the state lock; an `E_FORMAT` refusal is an `Err` (logged and dropped by the
/// caller, never forwarded).
fn encode_image(target: ImageTarget, source: &ImageSource) -> Result<Vec<u8>, String> {
    let ft = target.frame_target();
    match source {
        ImageSource::Color { rgb } => Ok(crate::frames::encode::encode_solid(ft, *rgb)),
        ImageSource::File { path } => {
            let wire = read_image_file(path)?;
            crate::frames::encode::encode_frame(ft, &wire).map_err(|e| format!("{}: {e}", e.code()))
        }
    }
}

/// Send one planned action to the device through the transport, paced by `limits`.
///
/// An image push resolves its source (file read or solid colour), re-encodes it to the
/// device wire format, chunks it with the frame plane's chunker for the target's module
/// and sends every chunk in order — a failed chunk aborts the push with an `Err` (logged
/// by the caller; the state task is never involved).
pub fn send_action(
    t: &dyn Transport,
    action: &DeviceAction,
    limits: &RateLimits,
) -> Result<(), String> {
    match action {
        DeviceAction::LedColor { kind, rgb } => {
            let report =
                ds_protocol::led_color(*kind, LedTarget::All, *rgb).ok_or("no led report")?;
            limits.led.acquire();
            t.send(&virtual_module_id(*kind), &report)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        DeviceAction::Image { target, source } => {
            let jpeg = encode_image(*target, source)?;
            let module = target.module();
            let out_len = module_info(module).out_len;
            let reports = crate::frames::chunk::reports(target.frame_target(), &jpeg, out_len);
            if reports.is_empty() {
                return Err(format!("image chunker produced no reports for {target:?}"));
            }
            let module_id = virtual_module_id(module);
            for report in &reports {
                limits.image.acquire();
                t.send(&module_id, report)
                    .map(|_| ())
                    .map_err(|e| format!("image chunk send failed for {target:?}: {e}"))?;
            }
            Ok(())
        }
    }
}

/// Handle of the device task for producers (the state task): bounded, non-blocking.
pub struct DeviceHandle {
    tx: SyncSender<DevicePush>,
    /// Number of pushes dropped because the channel was full.
    pub dropped: Arc<AtomicU64>,
}

impl DeviceHandle {
    /// Queue one state change for the device. Returns false when the bounded channel was
    /// full (the push is dropped and counted — never blocks).
    pub fn push(&self, push: DevicePush) -> bool {
        match self.tx.try_send(push) {
            Ok(()) => true,
            Err(_) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// A clone of the raw sender (the state task holds one).
    pub fn sender(&self) -> SyncSender<DevicePush> {
        self.tx.clone()
    }
}

/// Human-readable device status for `daemon status` (`connected …` / `none` / `error: …`).
pub type DeviceStatus = Arc<Mutex<Option<String>>>;

/// Open the transport for a device URI (when given) and run the device loop on a new
/// thread.
///
/// `device` is `None` for `--device none`. A URI that fails to open is logged loudly and
/// the daemon keeps running without hardware (hot-plug/retry is DS-023); the returned
/// status says what happened. `bus` is the event bus (DS-040): input reports and
/// connect/disconnect are published on it.
pub fn spawn(
    device: Option<DeviceUri>,
    shutdown: Shutdown,
    bus: Bus,
) -> (DeviceHandle, JoinHandle<()>, DeviceStatus) {
    let (tx, rx) = std::sync::mpsc::sync_channel(DEVICE_CHANNEL_CAP);
    let dropped = Arc::new(AtomicU64::new(0));
    let handle = DeviceHandle {
        tx,
        dropped: Arc::clone(&dropped),
    };
    let status: DeviceStatus = Arc::new(Mutex::new(None));
    let status2 = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("ds-device".into())
        .spawn(move || run_loop(device, rx, shutdown, status2, bus))
        .expect("spawn ds-device thread");
    (handle, thread, status)
}

fn run_loop(
    device: Option<DeviceUri>,
    rx: Receiver<DevicePush>,
    shutdown: Shutdown,
    status: DeviceStatus,
    bus: Bus,
) {
    let limits = RateLimits::new();
    let mut decoder = router::InputDecoder::new();
    let transport: Option<Box<dyn Transport>> = match &device {
        None => {
            *status.lock().unwrap() = Some("none".to_string());
            lifecycle::log("info", "ds-daemon::device", "no device (--device none)");
            None
        }
        Some(uri) => match ds_device::open(uri) {
            Ok(t) => {
                *status.lock().unwrap() = Some(format!("connected {uri}"));
                lifecycle::log(
                    "info",
                    "ds-daemon::device",
                    &format!("connected to device {uri}"),
                );
                // Software-mode handshake before anything else on keys/dial (invariant:
                // no reads/commands on those modules in hardware mode).
                for kind in [ModuleKind::Keys, ModuleKind::Dial] {
                    if let Some((report, _mode)) = ds_protocol::enter_software_mode(kind) {
                        let module = virtual_module_id(kind);
                        if let Err(e) = t.send(&module, &report) {
                            lifecycle::log(
                                "error",
                                "ds-daemon::device",
                                &format!("software-mode handshake for {kind:?} failed: {e}"),
                            );
                        }
                    }
                }
                // The connect is a bus event: deck/device/<serial>/connected (DS-040).
                let serial = virtual_module_id(ModuleKind::Keys).serial;
                bus.publish(router::device_event(
                    &serial,
                    "connected",
                    serde_json::json!({"device": uri.to_string()}),
                ));
                Some(t)
            }
            Err(e) => {
                *status.lock().unwrap() = Some(format!("error: {e}"));
                lifecycle::log(
                    "error",
                    "ds-daemon::device",
                    &format!("cannot open device {uri}: {e} (running without hardware)"),
                );
                None
            }
        },
    };

    loop {
        if shutdown.get() {
            break;
        }
        // Pushes are the hot path: block on the channel so a committed change is applied
        // immediately (the state task enqueues the push during commit, before the set
        // reply), then drain whatever else is queued.
        match rx.recv_timeout(POLL) {
            Ok(push) => {
                handle_push(transport.as_deref(), &push, &limits);
                while let Ok(push) = rx.try_recv() {
                    handle_push(transport.as_deref(), &push, &limits);
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        match &transport {
            Some(t) => match t.recv(TRANSPORT_POLL) {
                Ok(DeviceEvent::Input { module, input }) => {
                    // Decode the input report into deck/control/* (or deck/device/*) events
                    // and publish them on the bus (DS-040). Publish never blocks.
                    let events = decoder.decode(&module, &input);
                    let n = events.len();
                    for ev in events {
                        bus.publish(ev);
                    }
                    lifecycle::log(
                        "info",
                        "ds-daemon::device",
                        &format!(
                            "input from {:?} ({}): {:?} -> {n} bus event(s)",
                            module.kind, module.serial, input
                        ),
                    );
                }
                Ok(DeviceEvent::Disconnected) => {
                    lifecycle::log("warn", "ds-daemon::device", "device disconnected");
                    *status.lock().unwrap() = Some("disconnected".to_string());
                    let serial = virtual_module_id(ModuleKind::Keys).serial;
                    bus.publish(router::device_event(
                        &serial,
                        "disconnected",
                        serde_json::json!({}),
                    ));
                    break;
                }
                Err(ds_device::Error::Timeout(_)) => {}
                Err(e) => {
                    lifecycle::log("warn", "ds-daemon::device", &format!("device error: {e}"));
                    break;
                }
            },
            None => std::thread::sleep(POLL),
        }
    }
    if let Some(t) = &transport {
        let _ = t.close();
    }
    lifecycle::log("info", "ds-daemon::device", "device task stopped");
}

fn handle_push(transport: Option<&dyn Transport>, push: &DevicePush, limits: &RateLimits) {
    let DevicePush::State { path, value } = push;
    match plan_push(path, value) {
        Some(action) => match transport {
            Some(t) => {
                // File reads and chunk writes happen here, on the device task's thread —
                // never under the state lock; a failure is logged and dropped.
                if let Err(e) = send_action(t, &action, limits) {
                    lifecycle::log(
                        "error",
                        "ds-daemon::device",
                        &format!("push {path} failed: {e}"),
                    );
                } else {
                    lifecycle::log(
                        "debug",
                        "ds-daemon::device",
                        &format!("pushed {action:?} for {path}"),
                    );
                }
            }
            None => {
                lifecycle::log(
                    "debug",
                    "ds-daemon::device",
                    &format!("no device; dropped push {action:?} for {path}"),
                );
            }
        },
        None => {
            lifecycle::log(
                "debug",
                "ds-daemon::device",
                &format!("out of scope, ignored: {path} = {value}"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_protocol::Report;
    use mh_device::ModuleId;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    /// A mock transport recording every sent report.
    struct MockTransport {
        sent: StdMutex<Vec<(ModuleId, Vec<u8>)>>,
    }

    impl Transport for MockTransport {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            let bytes = report.bytes().to_vec();
            self.sent
                .lock()
                .unwrap()
                .push((module.clone(), bytes.clone()));
            Ok(bytes.len())
        }
        fn query(
            &self,
            _module: &ModuleId,
            _feature_id: u8,
            _timeout: Duration,
        ) -> ds_device::Result<Vec<u8>> {
            Ok(vec![])
        }
        fn recv(&self, _timeout: Duration) -> ds_device::Result<DeviceEvent> {
            Err(ds_device::Error::Timeout(Duration::from_millis(1)))
        }
        fn close(&self) -> ds_device::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn hex_color_parsing() {
        assert_eq!(parse_hex_color("#3cc86e"), Some([0x3c, 0xc8, 0x6e]));
        assert_eq!(parse_hex_color("#FFFFFF"), Some([255, 255, 255]));
        for bad in ["3cc86e", "#fff", "#12345", "#gggggg", "", "#1234567"] {
            assert_eq!(parse_hex_color(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn plan_push_maps_image_visuals_to_chunked_pushes() {
        // key colour -> 0x36 image push for key N
        let a = plan_push(
            "/controls/keys/3/image",
            &json!({"kind": "color", "value": "#ff8800"}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::Image {
                target: ImageTarget::Key(3),
                source: ImageSource::Color {
                    rgb: [0xff, 0x88, 0x00]
                }
            })
        );
        // key file -> 0x36 image push carrying the path
        let a = plan_push(
            "/controls/keys/0/image",
            &json!({"kind": "file", "path": "/tmp/pic.png"}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::Image {
                target: ImageTarget::Key(0),
                source: ImageSource::File {
                    path: "/tmp/pic.png".to_string()
                }
            })
        );
        // panel image -> 0x35
        let a = plan_push(
            "/controls/keys/image",
            &json!({"kind": "file", "path": "/tmp/panel.png"}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::Image {
                target: ImageTarget::Panel,
                source: ImageSource::File {
                    path: "/tmp/panel.png".to_string()
                }
            })
        );
        // dial colour -> 0x45
        let a = plan_push(
            "/controls/dial/image",
            &json!({"kind": "color", "value": "#000000"}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::Image {
                target: ImageTarget::Dial,
                source: ImageSource::Color { rgb: [0, 0, 0] }
            })
        );
        // led color (string form) is unchanged
        let a = plan_push(
            "/controls/knobs/1/led",
            &json!({"mode": "static", "color": "#00ff00", "brightness": 40}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::LedColor {
                kind: ModuleKind::Knobs,
                rgb: [0, 255, 0]
            })
        );
        // led color (array form)
        let a = plan_push("/controls/faders/2/led", &json!({"color": [1, 2, 3]}));
        assert_eq!(
            a,
            Some(DeviceAction::LedColor {
                kind: ModuleKind::Faders,
                rgb: [1, 2, 3]
            })
        );
        // out of scope: icon/text/frame visuals, brightness, bad paths/values
        assert_eq!(
            plan_push(
                "/controls/keys/0/image",
                &json!({"kind": "icon", "name": "tabler:bolt"})
            ),
            None
        );
        assert_eq!(
            plan_push(
                "/controls/keys/0/image",
                &json!({"kind": "text", "value": "hi"})
            ),
            None
        );
        assert_eq!(
            plan_push(
                "/controls/dial/image",
                &json!({"kind": "frame", "target": "dial", "seq": 1, "sha256": "0".repeat(64)})
            ),
            None
        );
        assert_eq!(plan_push("/controls/keys/0/brightness", &json!(80)), None);
        assert_eq!(plan_push("/settings/log/level", &json!("debug")), None);
        assert_eq!(
            plan_push(
                "/controls/keys/99/image",
                &json!({"kind": "color", "value": "#ffffff"})
            ),
            None
        );
        assert_eq!(
            plan_push(
                "/controls/keys/1/image",
                &json!({"kind": "color", "value": "red"})
            ),
            None
        );
        assert_eq!(
            plan_push("/controls/keys/2/image", &json!({"kind": "file"})),
            None
        );
    }

    #[test]
    fn send_action_pushes_encoded_jpeg_chunks_for_the_target() {
        let mock = MockTransport {
            sent: StdMutex::new(Vec::new()),
        };
        let limits = RateLimits::new();
        // a colour on key 3: one chunk report `02 36 00 00 <len> 03 01 <jpeg…>` carrying the
        // encoded baseline JPEG (solid 72x72 screen at q90), not a PNG placeholder
        send_action(
            &mock,
            &DeviceAction::Image {
                target: ImageTarget::Key(3),
                source: ImageSource::Color {
                    rgb: [0xff, 0x88, 0x00],
                },
            },
            &limits,
        )
        .unwrap();
        {
            let want = crate::frames::encode::encode_solid(
                ImageTarget::Key(3).frame_target(),
                [0xff, 0x88, 0x00],
            );
            let sent = mock.sent.lock().unwrap();
            assert_eq!(
                sent.len(),
                want.len().div_ceil(312),
                "the solid key JPEG is chunked into ceil(len/312) reports"
            );
            assert_eq!(sent[0].0.kind, ModuleKind::Keys);
            let raw = &sent[0].1;
            assert_eq!(&raw[..2], &[0x02, 0x36]);
            assert_eq!(u16::from_le_bytes([raw[2], raw[3]]), 0, "chunk 0");
            let len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
            assert_eq!(raw[6], 3, "key index");
            assert_eq!(
                raw[7],
                u8::from(sent.len() == 1),
                "last flag only on the final chunk"
            );
            assert_eq!(len, raw.len() - 8);
            assert_eq!(
                &raw[8..10],
                &[0xFF, 0xD8],
                "payload is the encoded JPEG (SOI)"
            );
            // ... and the reassembled payload is exactly what the frame plane encodes
            let mut reassembled = Vec::new();
            for (_, r) in sent.iter() {
                let l = u16::from_le_bytes([r[4], r[5]]) as usize;
                reassembled.extend_from_slice(&r[8..8 + l]);
            }
            assert_eq!(reassembled, want);
        }
        // a file on the panel: a real image file is re-encoded to the panel JPEG (q80) and
        // chunked as 0x35 with index 0 — the chunks no longer carry the file bytes raw
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("panel.jpg");
        let source_jpeg = crate::frames::encode::encode_solid(
            crate::frames::FrameTarget::Key(0),
            [0x10, 0x20, 0x30],
        );
        std::fs::write(&file, &source_jpeg).unwrap();
        send_action(
            &mock,
            &DeviceAction::Image {
                target: ImageTarget::Panel,
                source: ImageSource::File {
                    path: file.display().to_string(),
                },
            },
            &limits,
        )
        .unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            let chunks: Vec<&(ModuleId, Vec<u8>)> = sent
                .iter()
                .filter(|(m, raw)| m.kind == ModuleKind::Keys && raw[1] == 0x35)
                .collect();
            assert!(
                chunks.len() >= 1,
                "the encoded panel JPEG is chunked as 0x35"
            );
            let mut reassembled = Vec::new();
            for (i, (_, raw)) in chunks.iter().enumerate() {
                assert_eq!(
                    u16::from_le_bytes([raw[2], raw[3]]),
                    i as u16,
                    "chunk order"
                );
                assert_eq!(raw[6], 0, "panel index");
                let len = u16::from_le_bytes([raw[4], raw[5]]) as usize;
                reassembled.extend_from_slice(&raw[8..8 + len]);
            }
            let want = crate::frames::encode::encode_frame(
                crate::frames::FrameTarget::Panel,
                &source_jpeg,
            )
            .unwrap();
            assert_eq!(
                reassembled, want,
                "the chunks carry the re-encoded device JPEG, not the file bytes"
            );
            assert_eq!(chunks.last().unwrap().1[7], 1, "last chunk flagged");
        }
        // a non-image file is refused with E_FORMAT and nothing is sent
        let junk = dir.path().join("junk.bin");
        std::fs::write(&junk, b"this is not an image").unwrap();
        let before = mock.sent.lock().unwrap().len();
        let err = send_action(
            &mock,
            &DeviceAction::Image {
                target: ImageTarget::Panel,
                source: ImageSource::File {
                    path: junk.display().to_string(),
                },
            },
            &limits,
        )
        .unwrap_err();
        assert!(err.starts_with("E_FORMAT"), "{err}");
        assert_eq!(mock.sent.lock().unwrap().len(), before, "nothing forwarded");
        // a dial colour goes to the dial module as 0x45
        send_action(
            &mock,
            &DeviceAction::Image {
                target: ImageTarget::Dial,
                source: ImageSource::Color { rgb: [1, 2, 3] },
            },
            &limits,
        )
        .unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            let dial: Vec<&(ModuleId, Vec<u8>)> = sent
                .iter()
                .filter(|(m, raw)| m.kind == ModuleKind::Dial && raw[1] == 0x45)
                .collect();
            let want =
                crate::frames::encode::encode_solid(ImageTarget::Dial.frame_target(), [1, 2, 3]);
            assert_eq!(dial.len(), want.len().div_ceil(312));
            assert_eq!(&dial[0].1[8..10], &[0xFF, 0xD8]);
        }
        // an LED colour is still a feature report on its module
        send_action(
            &mock,
            &DeviceAction::LedColor {
                kind: ModuleKind::Faders,
                rgb: [4, 5, 6],
            },
            &limits,
        )
        .unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            let led = sent
                .iter()
                .find(|(m, _)| m.kind == ModuleKind::Faders)
                .unwrap();
            assert_eq!(&led.1[..8], &[0x02, 0x2D, 1, 0xFF, 4, 5, 6, 0]);
        }
    }

    #[test]
    fn read_image_file_confines_to_roots_and_caps_the_size() {
        // /tmp is an implicit root: a temp file reads back unchanged
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("key.png");
        let data: Vec<u8> = (0..500u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        let path = file.display().to_string();
        assert_eq!(read_image_file(&path).unwrap(), data);
        // outside every root -> Err, not a panic
        let err = read_image_file("/etc/hostname").unwrap_err();
        assert!(err.contains("outside /policy/fs/roots"), "{err}");
        // missing file -> Err
        assert!(read_image_file("/tmp/definitely-missing-ds095.png").is_err());
        // over the cap -> Err (write cap+1 bytes)
        let big = dir.path().join("big.bin");
        let chunk = vec![0xAB; 65536];
        let mut f = std::fs::File::create(&big).unwrap();
        use std::io::Write;
        for _ in 0..(MAX_IMAGE_FILE_BYTES / 65536 + 1) {
            f.write_all(&chunk).unwrap();
        }
        drop(f);
        let err = read_image_file(&big.display().to_string()).unwrap_err();
        assert!(err.contains("cap"), "{err}");
    }

    #[test]
    fn device_task_with_no_device_accepts_and_drops_pushes() {
        let shutdown = Shutdown::manual();
        let (handle, thread, status) = spawn(None, shutdown.clone(), Bus::new());
        assert!(handle.push(DevicePush::State {
            path: "/controls/keys/0/image".into(),
            value: json!({"kind": "color", "value": "#ffffff"}),
        }));
        shutdown.trigger();
        thread.join().unwrap();
        assert_eq!(status.lock().unwrap().as_deref(), Some("none"));
    }
}
