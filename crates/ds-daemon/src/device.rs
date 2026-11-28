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
//!   - `{"kind":"color","value":"#rrggbb"}` → a solid screen of the target's native size;
//!   - `{"kind":"colors","value":[15 × "#rrggbb"]}` (panel only, DS-111) → the daemon
//!     composes the 480x272 panel from the 15 key colours and pushes it as one `02 35`.
//!
//! Both sources are then **re-encoded to the device wire format** by [`crate::frames::encode`]
//! (baseline JPEG, standard Huffman tables, 4:4:4, q80 panel / q90 key:N / q85 dial —
//! DS-030, `docs/contracts/streaming.md` §2) and chunked by [`crate::frames::chunk`].
//! An input the encoder refuses (unsupported container, progressive or optimized-Huffman
//! JPEG, over the decode caps) fails the push with `E_FORMAT` and is dropped (counted),
//! never forwarded.
//! - `/controls/{knobs,faders}/<n>/led` → mode + colour + brightness feature reports
//!   (`02 2C/2D/2E`, target all), paced by the LED rate cap; `/controls/rollers/<n>/led` has
//!   no LED command and is a warned no-op.
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
//!
//! **Hot-plug survival (DS-120).** A module/device disconnect (a `Disconnected` event or a
//! gone-class transport error) never stops the task or the daemon: the device is marked
//! unhealthy (`disconnected` status + `deck/device/<serial>/disconnected`), the transport is
//! closed, and the loop re-opens, re-handshakes and re-pushes the cached latest targets
//! within the ≤ 5 s replug budget (`docs/90-verification.md`) — the daemon PID is unchanged.
//! The latest-frame-wins re-push cache uses the same DS-111 panel/key exclusion rule as the
//! resilient device task ([`crate::device_task`]).

use crate::lifecycle::{self, Shutdown};
use crate::router::{self, Bus};
use ds_device::hotplug::{self, IoFault};
use ds_device::module::module_info;
use ds_device::rate::{RateLimiter, DEFAULT_LED_RATE_PER_S};
use ds_device::transport::{module_id_for_send, DeviceEvent, Transport};
use ds_device::uri::DeviceUri;
use ds_device::vtransport::virtual_module_id;
use ds_protocol::{LedMode, LedTarget, ModuleKind};
use mh_device::ModuleId;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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

/// Delay before the first re-open after a disconnect (DS-120): the replug budget is
/// ≤ 5 s end to end (`docs/90-verification.md`), so the fast rescan starts immediately.
const RECONNECT_DELAY: Duration = Duration::from_millis(200);

/// Cap of the backoff after repeated failed re-opens (the device is still down).
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(2);

/// Pause after a non-fatal transport error, so a persistent error cannot busy-loop the task.
const ERROR_PAUSE: Duration = Duration::from_millis(50);

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
    /// `{"kind":"colors","value":[15 × "#rrggbb"]}` — the panel-only bulk visual (DS-111):
    /// the daemon composes the 480x272 panel from the 15 key colours server-side.
    Colors([[u8; 3]; 15]),
}

/// What a push translates to on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAction {
    /// Push image bytes to a display target as chunked image reports (0x35/0x36/0x45).
    Image {
        target: ImageTarget,
        source: ImageSource,
    },
    /// LED on an M0-family module (knobs/faders): mode + colour + brightness (`02 2C/2D/2E`).
    LedColor {
        kind: ModuleKind,
        mode: LedMode,
        rgb: [u8; 3],
        brightness: u8,
    },
    /// Screen backlight on keys or dial (`03 34/44 <pct>`).
    Backlight { kind: ModuleKind, percent: u8 },
    /// Base ring LED: mode + colour + brightness (`02 2C/2D/2E`, target 0).
    BaseLed {
        mode: LedMode,
        rgb: [u8; 3],
        brightness: u8,
    },
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

fn led_mode_of(s: &str) -> Option<LedMode> {
    Some(match s {
        "off" => LedMode::Off,
        "static" => LedMode::Static,
        "breathing" => LedMode::Breathing,
        "strobe" => LedMode::Strobe,
        "spring" => LedMode::Spring,
        "cycling" => LedMode::Cycling,
        "random" => LedMode::Random,
        "wave" => LedMode::Wave,
        "water" => LedMode::Water,
        "rainbow" => LedMode::Rainbow,
        _ => return None,
    })
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

/// Parse the `value` of a `kind:"colors"` visual: exactly 15 `#rrggbb` strings
/// (row-major key 0..14). `None` for any other arity or spelling — the schema rejects
/// those before the transaction, this is the daemon-side backstop.
pub fn parse_colors_visual(value: &Value) -> Option<[[u8; 3]; 15]> {
    let arr = value.get("value")?.as_array()?;
    if arr.len() != 15 {
        return None;
    }
    let mut colors = [[0u8; 3]; 15];
    for (i, item) in arr.iter().enumerate() {
        colors[i] = parse_hex_color(item.as_str()?)?;
    }
    Some(colors)
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
        "colors" => Some(ImageSource::Colors(parse_colors_visual(value)?)),
        _ => None,
    }
}

/// One-shot warning that `/controls/rollers/<n>/led` is accepted in state but has no device
/// command (see the `rollers` arm of [`plan_push`]). Logged once per daemon run so a 20 Hz
/// roller feed does not spam the log.
fn warn_rollers_led_unsupported_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        lifecycle::log(
            "warn",
            "ds-daemon::device",
            "/controls/rollers/*/led is valid state but the M032LE has no LED command in \
             mh-protocol (M0 SDK LED targets are base/faders/knobs only); the write is a no-op",
        );
    });
}

/// The latest-wins coalescing key of an LED-plane path (the paths [`plan_push`] maps to
/// `LedColor`/`BaseLed`/`Backlight`), or `None` for every other push (images etc.), which
/// keep flowing through the bounded channel. Per-index paths that target the same
/// module-wide LED share one key, so they collapse into a single send.
pub fn led_lane_key(path: &str) -> Option<String> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segs.as_slice() {
        ["controls", "base", "led"] => Some("base-led".to_string()),
        ["controls", kind, _n, "led"] => match *kind {
            "knobs" => Some("led:knobs".to_string()),
            "faders" => Some("led:faders".to_string()),
            "rollers" => Some("led:rollers".to_string()),
            _ => None,
        },
        ["controls", "keys", _n, "brightness"] => Some("backlight:keys".to_string()),
        ["controls", "dial", "brightness"] => Some("backlight:dial".to_string()),
        _ => None,
    }
}

/// Translate one state change into a device action; `None` when out of scope (logged by
/// the caller). Display-target `image` visuals (file and colour) become chunked image
/// pushes; LED colors stay feature reports.
pub fn plan_push(path: &str, value: &Value) -> Option<DeviceAction> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segs.as_slice() {
        ["controls", "panel", "image"] => {
            plan_image_source(value).map(|source| DeviceAction::Image {
                target: ImageTarget::Panel,
                source,
            })
        }
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
                // The M032LE rollers have no LED command in mh-protocol: the M0 SDK accepts
                // LED target 0 = base, 1 = faders, 2 = knobs only (`m0_led_address`), so
                // `led_color(Rollers, …)` is unrepresentable. The state path stays valid
                // (schemas/state.json), but the write is a one-shot-warned no-op — no
                // command is invented.
                "rollers" => {
                    warn_rollers_led_unsupported_once();
                    return None;
                }
                _ => return None,
            };
            let mode = led_mode_of(value.get("mode")?.as_str()?)?;
            let rgb = led_color_of(value)?;
            let brightness = value.get("brightness")?.as_u64()?.min(100) as u8;
            Some(DeviceAction::LedColor {
                kind,
                mode,
                rgb,
                brightness,
            })
        }
        ["controls", "base", "led"] => {
            let mode = led_mode_of(value.get("mode")?.as_str()?)?;
            let rgb = led_color_of(value)?;
            let brightness = value.get("brightness")?.as_u64()?.min(100) as u8;
            Some(DeviceAction::BaseLed {
                mode,
                rgb,
                brightness,
            })
        }
        ["controls", "keys", _n, "brightness"] => {
            let percent = value.as_u64()?.clamp(1, 100) as u8;
            Some(DeviceAction::Backlight {
                kind: ModuleKind::Keys,
                percent,
            })
        }
        ["controls", "dial", "brightness"] => {
            let percent = value.as_u64()?.clamp(1, 100) as u8;
            Some(DeviceAction::Backlight {
                kind: ModuleKind::Dial,
                percent,
            })
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
        ImageSource::Colors(colors) => match target {
            // the bulk colours visual composes the whole panel (DS-111); it is valid only
            // on /controls/panel/image — the schema rejects it on keys/dial, and here is
            // the daemon-side backstop
            ImageTarget::Panel => Ok(crate::frames::encode::encode_panel_colors(colors)),
            other => Err(format!(
                "the kind:\"colors\" visual is panel-only, not {other:?}"
            )),
        },
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
        DeviceAction::Backlight { kind, percent } => {
            let report = ds_protocol::backlight(*kind, *percent).ok_or("no backlight report")?;
            let module = module_id_for_send(t, *kind).map_err(|e| e.to_string())?;
            limits.led.acquire();
            t.send(&module, &report)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        DeviceAction::LedColor {
            kind,
            mode,
            rgb,
            brightness,
        } => {
            // The transport owns module identity: ask it for the id of this module kind
            // (hid: -> the enumerated serial, virtual: -> the virtual id).
            let module = module_id_for_send(t, *kind).map_err(|e| e.to_string())?;
            for report in [
                ds_protocol::led_mode(*kind, LedTarget::All, *mode),
                ds_protocol::led_color(*kind, LedTarget::All, *rgb),
                ds_protocol::led_brightness(*kind, LedTarget::All, *brightness),
            ]
            .into_iter()
            .flatten()
            {
                limits.led.acquire();
                t.send(&module, &report).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        DeviceAction::BaseLed {
            mode,
            rgb,
            brightness,
        } => {
            let module = module_id_for_send(t, ModuleKind::Base).map_err(|e| e.to_string())?;
            for report in [
                ds_protocol::led_mode(ModuleKind::Base, LedTarget::All, *mode),
                ds_protocol::led_color(ModuleKind::Base, LedTarget::All, *rgb),
                ds_protocol::led_brightness(ModuleKind::Base, LedTarget::All, *brightness),
            ]
            .into_iter()
            .flatten()
            {
                limits.led.acquire();
                t.send(&module, &report).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        DeviceAction::Image { target, source } => {
            let jpeg = encode_image(*target, source)?;
            let module = target.module();
            let out_len = module_info(module).out_len;
            let reports = crate::frames::chunk::reports(target.frame_target(), &jpeg, out_len);
            if reports.is_empty() {
                return Err(format!("image chunker produced no reports for {target:?}"));
            }
            let module_id = module_id_for_send(t, module)
                .map_err(|e| format!("image push for {target:?}: {e}"))?;
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
///
/// Non-LED pushes go through the bounded channel ([`DeviceHandle::push`], drop+count when
/// full); LED-plane pushes go through the latest-wins [`LedLane`] ([`DeviceHandle::led`]),
/// where the newest value always wins and is never dropped.
#[derive(Clone)]
pub struct DeviceHandle {
    tx: SyncSender<DevicePush>,
    /// Number of pushes dropped because the channel was full.
    pub dropped: Arc<AtomicU64>,
    /// The latest-wins LED lane (dedicated writer thread with the per-module LED caps).
    pub led: crate::led::LedLane,
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

    /// A handle over a raw channel plus a lane nobody writes to (unit tests).
    #[cfg(test)]
    pub fn new_for_tests(tx: SyncSender<DevicePush>, led: crate::led::LedLane) -> Self {
        DeviceHandle {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
            led,
        }
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
    frames: Option<crate::frames::service::FrameService>,
) -> (DeviceHandle, JoinHandle<()>, DeviceStatus) {
    spawn_with(device, shutdown, bus, frames, None)
}

/// Testable entry point: `open` overrides the transport factory (unit tests inject fakes
/// that disappear, like [`crate::device_task::spawn_with`]).
pub(crate) fn spawn_with(
    device: Option<DeviceUri>,
    shutdown: Shutdown,
    bus: Bus,
    frames: Option<crate::frames::service::FrameService>,
    open: Option<crate::device_task::Opener>,
) -> (DeviceHandle, JoinHandle<()>, DeviceStatus) {
    let (tx, rx) = std::sync::mpsc::sync_channel(DEVICE_CHANNEL_CAP);
    let dropped = Arc::new(AtomicU64::new(0));
    let led = crate::led::LedLane::new(shutdown.clone());
    // The dedicated LED writer thread: it owns the per-module LED rate buckets and is the
    // only place that sleeps on LED pacing, so the device task below never blocks on it.
    let _led_writer = led.clone().spawn_writer();
    let handle = DeviceHandle {
        tx,
        dropped: Arc::clone(&dropped),
        led: led.clone(),
    };
    let status: DeviceStatus = Arc::new(Mutex::new(None));
    let status2 = Arc::clone(&status);
    let thread = std::thread::Builder::new()
        .name("ds-device".into())
        .spawn(move || {
            let opener: crate::device_task::Opener = open.unwrap_or_else(|| {
                let uri = device.clone();
                Box::new(move || match &uri {
                    Some(uri) => ds_device::open(uri),
                    None => Err(ds_device::Error::Device("no device URI".into())),
                })
            });
            Loop {
                device,
                rx,
                shutdown,
                status: status2,
                bus,
                led,
                frames,
                opener,
                limits: RateLimits::new(),
                decoder: router::InputDecoder::new(),
                latest: HashMap::new(),
                failures: 0,
            }
            .run()
        })
        .expect("spawn ds-device thread");
    (handle, thread, status)
}

/// The device loop's shared state, bundled so the loop entry point stays readable (and
/// under clippy's argument limit). DS-120: this loop never exits on a device error —
/// it reconnects instead.
struct Loop {
    device: Option<DeviceUri>,
    rx: Receiver<DevicePush>,
    shutdown: Shutdown,
    status: DeviceStatus,
    bus: Bus,
    led: crate::led::LedLane,
    frames: Option<crate::frames::service::FrameService>,
    opener: crate::device_task::Opener,
    limits: RateLimits,
    decoder: router::InputDecoder,
    /// DS-120: latest-frame-wins re-push cache — what a reconnect re-sends. Same DS-111
    /// panel/key exclusion rule as the resilient device task ([`crate::device_task`]).
    latest: HashMap<String, DevicePush>,
    /// Consecutive failed (re-)opens, for the reconnect backoff.
    failures: u32,
}

impl Loop {
    fn run(mut self) {
        'reconnect: loop {
            let Some(uri) = self.device.clone() else {
                // --device none: idle, caching pushes (dropped on the floor, counted by the
                // state plane), until shutdown — the control plane stays fully alive.
                *self.status.lock().unwrap() = Some("none".to_string());
                lifecycle::log("info", "ds-daemon::device", "no device (--device none)");
                while !self.shutdown.get() {
                    match self.rx.recv_timeout(POLL) {
                        Ok(push) => self.cache_push(&push),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'reconnect,
                    }
                }
                break;
            };
            if self.shutdown.get() {
                break;
            }
            // Open + handshake. A device that is not there (yet) is retried with a backoff —
            // DS-120: a hot-unplug must never take the daemon down, the loop just reconnects.
            let t: Arc<dyn Transport> = match (self.opener)() {
                Ok(t) => Arc::from(t),
                Err(e) => {
                    self.failures = self.failures.saturating_add(1);
                    let delay = RECONNECT_DELAY
                        .checked_mul(1u32.checked_shl(self.failures.min(8)).unwrap_or(u32::MAX))
                        .unwrap_or(RECONNECT_MAX_BACKOFF)
                        .min(RECONNECT_MAX_BACKOFF);
                    *self.status.lock().unwrap() = Some(format!("reconnecting: {e}"));
                    lifecycle::log(
                        "warn",
                        "ds-daemon::device",
                        &format!(
                            "cannot open device {uri}: {e}; retrying in {delay:?} (daemon stays up)"
                        ),
                    );
                    if !self.drain_while_down(delay) {
                        break 'reconnect; // push channel closed: the daemon is shutting down
                    }
                    continue 'reconnect;
                }
            };
            // Software-mode handshake before anything else on keys/dial (invariant: no
            // reads/commands on those modules in hardware mode). The transport owns module
            // identity: the on-report goes to the id of the connected module of that kind
            // (hid: -> the enumerated serial, virtual: -> the virtual id); the M0 family needs
            // no report and is ready at any time.
            let mut handshaken: Vec<ModuleId> = Vec::new();
            let mut failed: Vec<String> = Vec::new();
            for kind in ModuleKind::ALL {
                match t.module_id(kind) {
                    None => continue, // not connected on this transport
                    Some(id) => {
                        if let Some((report, _mode)) = ds_protocol::enter_software_mode(kind) {
                            match t.send(&id, &report) {
                                Ok(_) => handshaken.push(id),
                                Err(e) => {
                                    failed.push(format!("{} ({}): {e}", kind.name(), id.serial))
                                }
                            }
                        } else {
                            handshaken.push(id); // M0: no software mode, ready now
                        }
                    }
                }
            }
            handshaken.sort();
            let fw: Vec<String> = handshaken
                .iter()
                .map(|id| format!("{} fw=?", id.kind.name()))
                .collect();
            lifecycle::log(
                "info",
                "ds-daemon::device",
                &format!(
                    "connected to {uri}: {} module(s) handshaken [{}], {} unhealthy",
                    handshaken.len(),
                    fw.join(", "),
                    failed.len()
                ),
            );
            for f in &failed {
                lifecycle::log(
                    "error",
                    "ds-daemon::device",
                    &format!("software-mode handshake failed for {f}"),
                );
            }
            self.led.set_transport(Some(t.clone()));
            // a fresh connection must receive the current LED values again (dedup reset)
            self.led.invalidate();
            *self.status.lock().unwrap() = Some(format!("connected {uri}"));
            // The connect is a bus event: deck/device/<serial>/connected (DS-040).
            let serial = t
                .module_id(ModuleKind::Keys)
                .map(|m| m.serial)
                .unwrap_or_else(|| virtual_module_id(ModuleKind::Keys).serial);
            self.bus.publish(router::device_event(
                &serial,
                "connected",
                serde_json::json!({"device": uri.to_string()}),
            ));
            // The frame plane may send only after the software-mode handshake, so the
            // stream writers get the transport here and not at service spawn.
            if let Some(frames) = &self.frames {
                frames.set_transport(Some(t.clone()));
            }
            self.failures = 0;
            // Re-push the cached current targets after the (re)connect (≤ 5 s replug budget).
            self.repush(t.as_ref());

            // ---- connected service loop: pushes (hot path) + device events ----
            loop {
                if self.shutdown.get() {
                    break 'reconnect;
                }
                // Pushes are the hot path: block on the channel so a committed change is
                // applied immediately, then drain whatever else is queued.
                match self.rx.recv_timeout(POLL) {
                    Ok(push) => {
                        self.handle_push(Some(t.as_ref()), &push);
                        while let Ok(push) = self.rx.try_recv() {
                            self.handle_push(Some(t.as_ref()), &push);
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'reconnect,
                }
                // Drain every queued input event in this iteration: handling only one
                // event per pass throttled roller/knob input to ~1/POLL events per
                // second (the observed uniform 100 ms cadence on fast flicks).
                match t.recv(TRANSPORT_POLL) {
                    Ok(DeviceEvent::Input { module, input }) => {
                        // Decode the input report into deck/control/* (or deck/device/*) events
                        // and publish them on the bus (DS-040). Publish never blocks.
                        let events = self.decoder.decode(&module, &input);
                        let n = events.len();
                        for ev in events {
                            self.bus.publish(ev);
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
                        self.mark_gone(&t, "device reported disconnect");
                        if !self.drain_while_down(RECONNECT_DELAY) {
                            break 'reconnect;
                        }
                        continue 'reconnect;
                    }
                    Err(ds_device::Error::Timeout(_)) => {}
                    Err(e) => match hotplug::classify(&e) {
                        IoFault::Gone => {
                            // DS-120: a gone transport is a per-device error — mark unhealthy,
                            // publish disconnected, reconnect — never stop the task/daemon.
                            self.mark_gone(&t, &format!("transport error: {e}"));
                            if !self.drain_while_down(RECONNECT_DELAY) {
                                break 'reconnect;
                            }
                            continue 'reconnect;
                        }
                        _ => {
                            // non-fatal: logged and counted, the loop keeps the daemon alive
                            lifecycle::log(
                                "warn",
                                "ds-daemon::device",
                                &format!("device error: {e}"),
                            );
                            std::thread::sleep(ERROR_PAUSE);
                        }
                    },
                }
            }
        }
        self.led.set_transport(None);
        if let Some(frames) = &self.frames {
            frames.set_transport(None);
        }
        lifecycle::log("info", "ds-daemon::device", "device task stopped");
    }

    /// The transport is gone (DS-120): mark the device unhealthy, publish
    /// `deck/device/<serial>/disconnected`, detach the LED/frame planes and close the
    /// transport. The caller then re-opens + re-handshakes + re-pushes — the daemon
    /// process stays alive.
    fn mark_gone(&self, t: &Arc<dyn Transport>, reason: &str) {
        lifecycle::log(
            "warn",
            "ds-daemon::device",
            &format!("device gone: {reason}; marking unhealthy and reconnecting (DS-120)"),
        );
        *self.status.lock().unwrap() = Some(format!("disconnected ({reason})"));
        let serial = t
            .module_id(ModuleKind::Keys)
            .map(|m| m.serial)
            .unwrap_or_else(|| virtual_module_id(ModuleKind::Keys).serial);
        self.bus.publish(router::device_event(
            &serial,
            "disconnected",
            serde_json::json!({"reason": reason}),
        ));
        self.led.set_transport(None);
        if let Some(frames) = &self.frames {
            frames.set_transport(None);
        }
        let _ = t.close();
    }

    /// While the device is down, drain the push channel into the re-push cache for at most
    /// `delay`. Returns false when the channel is closed (the daemon is shutting down).
    fn drain_while_down(&mut self, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        loop {
            if self.shutdown.get() {
                return true;
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return true;
            }
            match self.rx.recv_timeout(wait.min(POLL)) {
                Ok(push) => self.cache_push(&push),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    /// Cache one push as the current value of its target (latest-frame-wins per target;
    /// DS-111 panel/key images are mutually exclusive — shared rule with device_task).
    fn cache_push(&mut self, push: &DevicePush) {
        let DevicePush::State { path, value } = push;
        let Some(action) = plan_push(path, value) else {
            return;
        };
        crate::device_task::drop_excluded_images(&mut self.latest, &action);
        let key = crate::device_task::cache_key(&action);
        self.latest.insert(key, push.clone());
    }

    /// Re-push every cached target after a (re)connect. Pure transport operation — no state
    /// read, no `rev` bump (S1). LED-plane pushes go back through the latest-wins lane.
    fn repush(&self, t: &dyn Transport) {
        if self.latest.is_empty() {
            return;
        }
        let mut keys: Vec<&String> = self.latest.keys().collect();
        keys.sort();
        lifecycle::log(
            "info",
            "ds-daemon::device",
            &format!(
                "re-pushing {} cached target(s) after (re)connect",
                keys.len()
            ),
        );
        for key in keys {
            let push = &self.latest[key];
            let DevicePush::State { path, value } = push;
            if led_lane_key(path).is_some() {
                self.led.submit(key.clone(), push.clone());
                continue;
            }
            if let Some(action) = plan_push(path, value) {
                if let Err(e) = send_action(t, &action, &self.limits) {
                    lifecycle::log(
                        "error",
                        "ds-daemon::device",
                        &format!("re-push {path} failed: {e}"),
                    );
                }
            }
        }
    }

    fn handle_push(&mut self, transport: Option<&dyn Transport>, push: &DevicePush) {
        let DevicePush::State { path, value } = push;
        // cache first: this push is the current value of its target and is what a reconnect
        // re-pushes (DS-120)
        self.cache_push(push);
        // LED-plane pushes never queue behind anything: hand them to the latest-wins lane (the
        // state task routes them there directly; this is the backstop for channel pushes).
        if let Some(key) = led_lane_key(path) {
            self.led.submit(key, push.clone());
            return;
        }
        match plan_push(path, value) {
            Some(action) => match transport {
                Some(t) => {
                    // File reads and chunk writes happen here, on the device task's thread —
                    // never under the state lock; a failure is logged and dropped.
                    if let Err(e) = send_action(t, &action, &self.limits) {
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
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            // the mock stands in for a whole deck: every kind resolves to its virtual id
            Some(virtual_module_id(kind))
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
    fn colors_visual_parsing_requires_exactly_15_hex_strings() {
        let hexes: Vec<String> = (0..15).map(|i| format!("#{i:02x}{i:02x}{i:02x}")).collect();
        let good = json!({"kind": "colors", "value": hexes});
        let parsed = parse_colors_visual(&good).unwrap();
        for (i, c) in parsed.iter().enumerate() {
            assert_eq!(*c, [i as u8, i as u8, i as u8]);
        }
        // 14 / 16 entries, non-string entries, bad hex: None
        let mut v = good.clone();
        v["value"] = json!(hexes[..14]);
        assert_eq!(parse_colors_visual(&v), None);
        let mut h16 = hexes.clone();
        h16.push("#000000".to_string());
        v["value"] = json!(h16);
        assert_eq!(parse_colors_visual(&v), None);
        v["value"] = json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
        assert_eq!(parse_colors_visual(&v), None);
        let mut hbad = hexes.clone();
        hbad[3] = "red".to_string();
        v["value"] = json!(hbad);
        assert_eq!(parse_colors_visual(&v), None);
        v["value"] = json!("#ff0000");
        assert_eq!(parse_colors_visual(&v), None);
        assert_eq!(parse_colors_visual(&json!({"kind": "colors"})), None);
    }

    #[test]
    fn plan_push_maps_a_colors_visual_on_the_panel_to_one_image_action() {
        let hexes: Vec<String> = (0..15).map(|i| format!("#{i:02x}0000")).collect();
        let visual = json!({"kind": "colors", "value": hexes});
        let a = plan_push("/controls/panel/image", &visual);
        let Some(DeviceAction::Image { target, source }) = a else {
            panic!("colors on the panel must plan an image action, got {a:?}");
        };
        assert_eq!(target, ImageTarget::Panel);
        let ImageSource::Colors(colors) = source else {
            panic!("source must be Colors, got {source:?}");
        };
        assert_eq!(colors[0], [0x00, 0x00, 0x00]);
        assert_eq!(colors[14], [0x0e, 0x00, 0x00]);
        // the same visual on a key or the dial still plans (the schema rejects it before
        // the transaction; encode_image refuses it as the daemon-side backstop)
        assert!(matches!(
            plan_push("/controls/keys/2/image", &visual),
            Some(DeviceAction::Image {
                target: ImageTarget::Key(2),
                ..
            })
        ));
        // malformed colors values plan nothing
        let bad = json!({"kind": "colors", "value": ["#ff0000"]});
        assert_eq!(plan_push("/controls/panel/image", &bad), None);
    }

    #[test]
    fn encode_image_routes_colors_to_the_panel_composer_only() {
        let colors = [[0x10u8, 0x20, 0x30]; 15];
        let source = ImageSource::Colors(colors);
        // panel: exactly the composer output
        let jpeg = encode_image(ImageTarget::Panel, &source).unwrap();
        assert_eq!(jpeg, crate::frames::encode::encode_panel_colors(&colors));
        // key/dial: refused, never a per-key solid of one of the colours
        for t in [ImageTarget::Key(0), ImageTarget::Dial] {
            let err = encode_image(t, &source).unwrap_err();
            assert!(err.contains("panel-only"), "{err}");
        }
    }

    #[test]
    fn send_action_pushes_a_colors_visual_as_one_0x35_panel_transfer() {
        let mock = MockTransport {
            sent: StdMutex::new(Vec::new()),
        };
        let limits = RateLimits::new();
        let mut colors = [[0u8; 3]; 15];
        for (i, c) in colors.iter_mut().enumerate() {
            *c = [i as u8, 0x80, 0xff - i as u8];
        }
        send_action(
            &mock,
            &DeviceAction::Image {
                target: ImageTarget::Panel,
                source: ImageSource::Colors(colors),
            },
            &limits,
        )
        .unwrap();
        let sent = mock.sent.lock().unwrap();
        let chunks: Vec<&(ModuleId, Vec<u8>)> = sent
            .iter()
            .filter(|(m, raw)| m.kind == ModuleKind::Keys && raw[1] == 0x35)
            .collect();
        let want = crate::frames::encode::encode_panel_colors(&colors);
        assert_eq!(
            chunks.len(),
            want.len().div_ceil(312),
            "the composed panel JPEG is chunked into ceil(len/312) 0x35 reports"
        );
        assert!(!chunks.is_empty());
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
        assert_eq!(reassembled, want, "one panel transfer, no 0x36 key reports");
        assert!(
            sent.iter().all(|(_, raw)| raw[1] != 0x36),
            "a colors push sends no per-key 0x36 reports"
        );
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
        // led (string form): mode + colour + brightness are all planned
        let a = plan_push(
            "/controls/knobs/1/led",
            &json!({"mode": "static", "color": "#00ff00", "brightness": 40}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::LedColor {
                kind: ModuleKind::Knobs,
                mode: LedMode::Static,
                rgb: [0, 255, 0],
                brightness: 40,
            })
        );
        // led (array form); brightness above 100 clamps to 100 like the base arm
        let a = plan_push(
            "/controls/faders/2/led",
            &json!({"mode": "breathing", "color": [1, 2, 3], "brightness": 250}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::LedColor {
                kind: ModuleKind::Faders,
                mode: LedMode::Breathing,
                rgb: [1, 2, 3],
                brightness: 100,
            })
        );
        // a led object missing mode or brightness is out of scope (the schema requires
        // all three fields on every led object; this is the daemon-side backstop)
        assert_eq!(
            plan_push("/controls/faders/2/led", &json!({"color": [1, 2, 3]})),
            None
        );
        assert_eq!(
            plan_push(
                "/controls/knobs/0/led",
                &json!({"mode": "disco", "color": "#010203", "brightness": 10})
            ),
            None
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
        assert_eq!(
            plan_push("/controls/keys/0/brightness", &json!(80)),
            Some(DeviceAction::Backlight {
                kind: ModuleKind::Keys,
                percent: 80
            })
        );
        assert_eq!(
            plan_push("/controls/dial/brightness", &json!(55)),
            Some(DeviceAction::Backlight {
                kind: ModuleKind::Dial,
                percent: 55
            })
        );
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
        // a module LED is the mode + colour + brightness feature reports on its module
        send_action(
            &mock,
            &DeviceAction::LedColor {
                kind: ModuleKind::Faders,
                mode: LedMode::Static,
                rgb: [4, 5, 6],
                brightness: 80,
            },
            &limits,
        )
        .unwrap();
        {
            let sent = mock.sent.lock().unwrap();
            let led: Vec<&Vec<u8>> = sent
                .iter()
                .filter(|(m, _)| m.kind == ModuleKind::Faders)
                .map(|(_, b)| b)
                .collect();
            assert_eq!(led.len(), 3, "mode + colour + brightness: {led:?}");
            assert_eq!(
                &led[0][..],
                &[0x02, 0x2C, 1, 0xFF, 0x01, 0x00],
                "02 2C mode"
            );
            assert_eq!(
                &led[1][..],
                &[0x02, 0x2D, 1, 0xFF, 4, 5, 6, 0],
                "02 2D colour"
            );
            assert_eq!(&led[2][..], &[0x02, 0x2E, 1, 0xFF, 80], "02 2E brightness");
        }
    }

    #[test]
    fn send_action_without_a_connected_module_is_a_typed_error_not_a_panic() {
        // a transport that has no module of the asked kind (the trait default): the send
        // path must produce a clear typed error, never panic
        struct NoModuleTransport;
        impl Transport for NoModuleTransport {
            fn send(&self, _module: &ModuleId, _report: &Report) -> ds_device::Result<usize> {
                Ok(0)
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
        let limits = RateLimits::new();
        let err = send_action(
            &NoModuleTransport,
            &DeviceAction::LedColor {
                kind: ModuleKind::Knobs,
                mode: LedMode::Static,
                rgb: [1, 2, 3],
                brightness: 100,
            },
            &limits,
        )
        .unwrap_err();
        assert!(
            err.contains("module knobs not connected on this transport"),
            "{err}"
        );
        let err = send_action(
            &NoModuleTransport,
            &DeviceAction::Image {
                target: ImageTarget::Panel,
                source: ImageSource::Color { rgb: [1, 2, 3] },
            },
            &limits,
        )
        .unwrap_err();
        assert!(
            err.contains("module keys not connected on this transport"),
            "{err}"
        );
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
        let (handle, thread, status) = spawn(None, shutdown.clone(), Bus::new(), None);
        assert!(handle.push(DevicePush::State {
            path: "/controls/keys/0/image".into(),
            value: json!({"kind": "color", "value": "#ffffff"}),
        }));
        shutdown.trigger();
        thread.join().unwrap();
        assert_eq!(status.lock().unwrap().as_deref(), Some("none"));
    }

    // ------------------------------------------------------------ DS-120 hot-plug survival

    use std::collections::VecDeque;
    use std::sync::mpsc::channel as std_channel;

    fn wait_until(deadline: Duration, mut f: impl FnMut() -> bool) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        f()
    }

    fn push_color(handle: &DeviceHandle, path: &str, hex: &str) -> bool {
        handle.push(DevicePush::State {
            path: path.into(),
            value: json!({"kind": "color", "value": hex}),
        })
    }

    /// A fake transport that can be told to report a disconnect (hot-unplug).
    struct FakeTransport {
        sent: StdMutex<Vec<(ModuleId, Vec<u8>)>>,
        events: StdMutex<VecDeque<ds_device::Result<DeviceEvent>>>,
        closed: StdMutex<bool>,
    }

    impl FakeTransport {
        fn new() -> Arc<Self> {
            Arc::new(FakeTransport {
                sent: StdMutex::new(Vec::new()),
                events: StdMutex::new(VecDeque::new()),
                closed: StdMutex::new(false),
            })
        }

        fn disconnect(&self) {
            self.events
                .lock()
                .unwrap()
                .push_back(Ok(DeviceEvent::Disconnected));
        }

        /// The image reports (0x02 0x36 …) sent to the keys module, in order.
        fn key_image_reports(&self) -> Vec<Vec<u8>> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, b)| {
                    m.kind == ModuleKind::Keys && b.len() > 2 && b[0] == 0x02 && b[1] == 0x36
                })
                .map(|(_, b)| b.clone())
                .collect()
        }
    }

    impl Transport for FakeTransport {
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
            let mut payload = vec![0x29, 0x63, 0x1C, 0x5D];
            payload.extend_from_slice(b"01.00.00.00");
            let mut ans = vec![0x03, 0x38, payload.len() as u8];
            ans.extend_from_slice(&payload);
            Ok(ans)
        }
        fn recv(&self, _timeout: Duration) -> ds_device::Result<DeviceEvent> {
            match self.events.lock().unwrap().pop_front() {
                Some(e) => e,
                None => Err(ds_device::Error::Timeout(Duration::from_millis(1))),
            }
        }
        fn close(&self) -> ds_device::Result<()> {
            *self.closed.lock().unwrap() = true;
            Ok(())
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            Some(virtual_module_id(kind))
        }
    }

    /// Adapter so an `Arc<FakeTransport>` can be boxed as a `Transport`.
    struct FakeTransportArc(Arc<FakeTransport>);

    impl Transport for FakeTransportArc {
        fn send(&self, module: &ModuleId, report: &Report) -> ds_device::Result<usize> {
            self.0.send(module, report)
        }
        fn query(&self, m: &ModuleId, f: u8, t: Duration) -> ds_device::Result<Vec<u8>> {
            self.0.query(m, f, t)
        }
        fn recv(&self, t: Duration) -> ds_device::Result<DeviceEvent> {
            self.0.recv(t)
        }
        fn close(&self) -> ds_device::Result<()> {
            self.0.close()
        }
        fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
            self.0.module_id(kind)
        }
    }

    fn expected_key_image_reports(rgb: [u8; 3]) -> Vec<Vec<u8>> {
        let ft = ImageTarget::Key(0).frame_target();
        let jpeg = crate::frames::encode::encode_solid(ft, rgb);
        crate::frames::chunk::reports(ft, &jpeg, module_info(ModuleKind::Keys).out_len)
            .iter()
            .map(|r| r.bytes().to_vec())
            .collect()
    }

    #[test]
    fn device_loop_survives_a_disconnect_and_repushes_on_reconnect() {
        let fake1 = FakeTransport::new();
        let fake2 = FakeTransport::new();
        let opens = Arc::new(AtomicU64::new(0));
        let (f1, f2, op) = (Arc::clone(&fake1), Arc::clone(&fake2), Arc::clone(&opens));
        let opener: crate::device_task::Opener = Box::new(move || {
            let n = op.fetch_add(1, Ordering::SeqCst);
            Ok(match n {
                0 => Box::new(FakeTransportArc(f1.clone())),
                _ => Box::new(FakeTransportArc(f2.clone())),
            })
        });
        let bus = Bus::new();
        let (sink_tx, sink_rx) = std_channel();
        bus.subscribe(
            router::Pattern::Topic(router::TopicPattern::parse("deck/*").unwrap()),
            None,
            "deck/*".into(),
            Arc::new(router::FnSink(Arc::new(move |msg: &Value| {
                let _ = sink_tx.send(msg.clone());
            }))),
        );
        let shutdown = Shutdown::manual();
        let uri = DeviceUri::parse("virtual:/tmp/ds120-fake.sock").unwrap();
        let (handle, thread, status) =
            spawn_with(Some(uri), shutdown.clone(), bus, None, Some(opener));

        // first connect + handshake
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        // push a key colour; the image chunks land on fake1
        assert!(push_color(&handle, "/controls/keys/0/image", "#3cc86e"));
        assert!(wait_until(Duration::from_secs(5), || {
            !fake1.key_image_reports().is_empty()
        }));
        assert_eq!(
            fake1.key_image_reports(),
            expected_key_image_reports([0x3c, 0xc8, 0x6e])
        );

        // DS-120: hot-unplug — the task must NOT exit; it reconnects (fake2) and re-pushes
        fake1.disconnect();
        assert!(wait_until(Duration::from_secs(5), || {
            opens.load(Ordering::SeqCst) >= 2
                && status
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|s| s.starts_with("connected"))
        }));
        assert!(
            wait_until(Duration::from_secs(5), || {
                fake2.key_image_reports() == expected_key_image_reports([0x3c, 0xc8, 0x6e])
            }),
            "the reconnect re-pushed the cached current frame"
        );
        assert!(*fake1.closed.lock().unwrap(), "old transport closed");
        assert!(
            !thread.is_finished(),
            "the device task survived the disconnect (DS-120)"
        );

        // the bus saw disconnected then connected again
        let topics: Vec<String> = sink_rx
            .try_iter()
            .filter_map(|m| m.get("topic").and_then(Value::as_str).map(String::from))
            .collect();
        let disc = topics
            .iter()
            .filter(|t| t.ends_with("/disconnected"))
            .count();
        let conn = topics.iter().filter(|t| t.ends_with("/connected")).count();
        assert!(disc >= 1, "disconnected event published: {topics:?}");
        assert!(
            conn >= 2,
            "connected event before and after replug: {topics:?}"
        );

        shutdown.trigger();
        thread.join().unwrap();
    }

    #[test]
    fn vdev_hot_unplug_keeps_the_device_task_alive_and_repushes_within_five_seconds() {
        use ds_vdev::{HubConfig, Target as VdevTarget, VdevServer};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("vdev.sock");
        let vdev_a = VdevServer::start(&sock, HubConfig::default()).unwrap();
        let shutdown = Shutdown::manual();
        let uri = DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap();
        let (handle, thread, status) = spawn(Some(uri), shutdown.clone(), Bus::new(), None);

        // connect + handshake against the real virtual device
        assert!(wait_until(Duration::from_secs(10), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));

        // push colour A, then the latest colour B; the vdev reassembles key:0
        assert!(push_color(&handle, "/controls/keys/0/image", "#3cc86e"));
        assert!(wait_until(Duration::from_secs(10), || {
            vdev_a.hub().frame(VdevTarget::Key(0)).is_some()
        }));
        assert!(push_color(&handle, "/controls/keys/0/image", "#ff8800"));
        assert!(wait_until(Duration::from_secs(10), || {
            let all = vdev_a.hub().frame(VdevTarget::Key(0)).map(|e| e.sha256);
            all.is_some_and(|sha| {
                sha == ds_vdev::sha256_hex(&crate::frames::encode::encode_solid(
                    ImageTarget::Key(0).frame_target(),
                    [0xff, 0x88, 0x00],
                ))
            })
        }));

        // simulate the hot-unplug (the ticket scenario: `throttle off 1` + a write): the
        // write past the budget kills the connection, and the server is then replaced on
        // the same socket path (fresh deck, empty frame table) = the replug
        let replug_start = std::time::Instant::now();
        vdev_a.hub().set_throttle(ds_vdev::Throttle {
            kbps: None,
            drop_after: Some(1),
        });
        assert!(push_color(&handle, "/controls/keys/0/image", "#00aabb"));
        vdev_a.shutdown();
        let vdev_b = VdevServer::start(&sock, HubConfig::default()).unwrap();

        // DS-120: the task reconnects to vdev_b and re-pushes the LATEST frame within 5 s,
        // and the task thread is still alive (no daemon restart)
        let got = wait_until(Duration::from_secs(5), || {
            vdev_b.hub().frame(VdevTarget::Key(0)).is_some()
        });
        let elapsed = replug_start.elapsed();
        assert!(got, "no re-pushed frame on the new vdev within 5 s");
        assert!(
            elapsed <= Duration::from_secs(5),
            "replug took {elapsed:?}, over the 5 s budget"
        );
        let entry = vdev_b.hub().frame(VdevTarget::Key(0)).unwrap();
        let want_sha = ds_vdev::sha256_hex(&crate::frames::encode::encode_solid(
            ImageTarget::Key(0).frame_target(),
            [0x00, 0xaa, 0xbb],
        ));
        assert_eq!(entry.sha256, want_sha, "re-push delivered the latest frame");
        assert!(
            !thread.is_finished(),
            "the device task survived the hot-unplug (DS-120: same daemon, no restart)"
        );
        assert!(wait_until(Duration::from_secs(5), || {
            status
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|s| s.starts_with("connected"))
        }));

        shutdown.trigger();
        thread.join().unwrap();
        drop(vdev_b);
    }
}
