//! The device task: owns the `ds-device` [`Transport`] opened from the device URI
//! (`virtual:<path>` → ds-vdev; `none` → no hardware) and applies the state image/color
//! pushes of this ticket's scope to the device:
//!
//! - `/controls/keys/<0-14>/image` = `{"kind":"color","value":"#rrggbb"}` → `fill_key`
//! - `/controls/dial/image` = color → `fill_screen` on the dial
//! - `/controls/{knobs,faders,rollers}/<n>/led` color → `led_color`
//!
//! Everything else (icons, text, files, frames) is out of scope for DS-015 and only
//! logged. The task receives pushes over a bounded channel, so the state task never
//! blocks on device I/O; input reports from the device are logged (device-driven state
//! writes are the router's job, later ticket).
//!
//! On connect, keys/dial get the software-mode handshake first — commands only land on
//! those modules in software mode (mh-protocol safety invariant).

use crate::lifecycle::{self, Shutdown};
use ds_device::transport::{DeviceEvent, Transport};
use ds_device::uri::DeviceUri;
use ds_device::vtransport::virtual_module_id;
use ds_protocol::{LedTarget, ModuleKind};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Capacity of the device-push channel (bounded; overflow drops and is counted, never
/// blocks the state task — S16 spirit).
pub const DEVICE_CHANNEL_CAP: usize = 64;

/// How long the device loop waits on the transport before re-checking shutdown/pushes.
const POLL: Duration = Duration::from_millis(100);

/// One state change the device task should apply.
#[derive(Debug, Clone)]
pub enum DevicePush {
    /// A committed `/controls/...` change (path + new value).
    State { path: String, value: Value },
}

/// What a push translates to on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceAction {
    /// Solid color on one key (`03 37 <index> r g b` feature report to the keys module).
    FillKey { index: u8, rgb: [u8; 3] },
    /// Solid color on the dial screen.
    FillDial { rgb: [u8; 3] },
    /// LED color on an M0-family module (knobs/faders/rollers).
    LedColor { kind: ModuleKind, rgb: [u8; 3] },
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

/// Translate one state change into a device action; `None` when out of scope (logged by
/// the caller). Only color/image pushes of the ticket scope are planned here.
pub fn plan_push(path: &str, value: &Value) -> Option<DeviceAction> {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let is_color = value.get("kind").and_then(Value::as_str) == Some("color");
    match segs.as_slice() {
        ["controls", "keys", n, "image"] if is_color => {
            let rgb = parse_hex_color(value.get("value")?.as_str()?)?;
            let index: u8 = n.parse().ok()?;
            (index < 15).then_some(DeviceAction::FillKey { index, rgb })
        }
        ["controls", "dial", "image"] if is_color => {
            let rgb = parse_hex_color(value.get("value")?.as_str()?)?;
            Some(DeviceAction::FillDial { rgb })
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

/// Send one planned action to the device through the transport.
pub fn send_action(t: &dyn Transport, action: &DeviceAction) -> Result<(), String> {
    match action {
        DeviceAction::FillKey { index, rgb } => {
            let report = ds_protocol::fill_key(*index, *rgb).ok_or("no fill_key report")?;
            t.send(&virtual_module_id(ModuleKind::Keys), &report)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        DeviceAction::FillDial { rgb } => {
            let report = ds_protocol::fill_screen(ModuleKind::Dial, *rgb).ok_or("no report")?;
            t.send(&virtual_module_id(ModuleKind::Dial), &report)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        DeviceAction::LedColor { kind, rgb } => {
            let report =
                ds_protocol::led_color(*kind, LedTarget::All, *rgb).ok_or("no led report")?;
            t.send(&virtual_module_id(*kind), &report)
                .map(|_| ())
                .map_err(|e| e.to_string())
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
/// status says what happened.
pub fn spawn(
    device: Option<DeviceUri>,
    shutdown: Shutdown,
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
        .spawn(move || run_loop(device, rx, shutdown, status2))
        .expect("spawn ds-device thread");
    (handle, thread, status)
}

fn run_loop(
    device: Option<DeviceUri>,
    rx: Receiver<DevicePush>,
    shutdown: Shutdown,
    status: DeviceStatus,
) {
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
        while let Ok(push) = rx.try_recv() {
            handle_push(transport.as_deref(), &push);
        }
        match &transport {
            Some(t) => match t.recv(POLL) {
                Ok(DeviceEvent::Input { module, input }) => {
                    lifecycle::log(
                        "info",
                        "ds-daemon::device",
                        &format!("input from {:?}: {:?}", module.kind, input),
                    );
                }
                Ok(DeviceEvent::Disconnected) => {
                    lifecycle::log("warn", "ds-daemon::device", "device disconnected");
                    *status.lock().unwrap() = Some("disconnected".to_string());
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

fn handle_push(transport: Option<&dyn Transport>, push: &DevicePush) {
    let DevicePush::State { path, value } = push;
    match plan_push(path, value) {
        Some(action) => match transport {
            Some(t) => {
                if let Err(e) = send_action(t, &action) {
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
                &format!("out of scope for DS-015, ignored: {path} = {value}"),
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
    fn plan_push_covers_the_ticket_scope() {
        // key color
        let a = plan_push(
            "/controls/keys/3/image",
            &json!({"kind": "color", "value": "#ff8800"}),
        );
        assert_eq!(
            a,
            Some(DeviceAction::FillKey {
                index: 3,
                rgb: [0xff, 0x88, 0x00]
            })
        );
        // dial color
        let a = plan_push(
            "/controls/dial/image",
            &json!({"kind": "color", "value": "#000000"}),
        );
        assert_eq!(a, Some(DeviceAction::FillDial { rgb: [0, 0, 0] }));
        // led color (string form)
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
        // out of scope: non-color image, brightness, other paths
        assert_eq!(
            plan_push(
                "/controls/keys/0/image",
                &json!({"kind": "icon", "name": "tabler:bolt"})
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
    }

    #[test]
    fn send_action_writes_the_expected_reports() {
        let mock = MockTransport {
            sent: StdMutex::new(Vec::new()),
        };
        send_action(
            &mock,
            &DeviceAction::FillKey {
                index: 3,
                rgb: [0xff, 0x88, 0x00],
            },
        )
        .unwrap();
        send_action(&mock, &DeviceAction::FillDial { rgb: [1, 2, 3] }).unwrap();
        send_action(
            &mock,
            &DeviceAction::LedColor {
                kind: ModuleKind::Faders,
                rgb: [4, 5, 6],
            },
        )
        .unwrap();
        let sent = mock.sent.lock().unwrap();
        assert_eq!(sent.len(), 3);
        // fill_key: feature report 03 37 <index> r g b to the keys module
        assert_eq!(sent[0].0.kind, ModuleKind::Keys);
        assert_eq!(&sent[0].1[..6], &[0x03, 0x37, 3, 0xff, 0x88, 0x00]);
        // fill_screen on the dial module
        assert_eq!(sent[1].0.kind, ModuleKind::Dial);
        // led_color on the faders module
        assert_eq!(sent[2].0.kind, ModuleKind::Faders);
    }

    #[test]
    fn device_task_with_no_device_accepts_and_drops_pushes() {
        let shutdown = Shutdown::manual();
        let (handle, thread, status) = spawn(None, shutdown.clone());
        assert!(handle.push(DevicePush::State {
            path: "/controls/keys/0/image".into(),
            value: json!({"kind": "color", "value": "#ffffff"}),
        }));
        shutdown.trigger();
        thread.join().unwrap();
        assert_eq!(status.lock().unwrap().as_deref(), Some("none"));
    }
}
