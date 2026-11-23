//! The per-module reader thread of the real HID transport (DS-021).
//!
//! One reader thread per open module loops `hid_read_timeout` in short slices (the device lock
//! is held for at most one slice, so `send`/`query` interleave between reads — the model of
//! the pinned `mh-device`), and every input report is decoded into [`DeviceEvent`]s through the
//! same `ds_protocol::parse_input` the virtual transport uses, so both transports deliver
//! identical events for identical reports.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use hidapi::HidDevice;
use mh_device::ModuleId;

use crate::module::ModuleInfo;
use crate::report::{view_report, ReportView};
use crate::transport::DeviceEvent;

/// Input reports are 64 B on every module (`upstream/PROTOCOL.md` "Reports").
pub const INPUT_REPORT_LEN: usize = 64;
/// One read slice: how long the reader holds the device lock per `hid_read_timeout` call.
pub const READ_SLICE: Duration = Duration::from_millis(20);
/// Gap the reader leaves the device lock free between two read slices. Without it the
/// reader re-acquires the lock immediately after each slice and a waiting `send`/`query`
/// starves on the mutex (observed as a hung `hid_write` on the real deck).
pub const READ_GAP: Duration = Duration::from_millis(1);

/// The module map the reader threads share with the transport (to close it when the last
/// module goes away).
pub(crate) type Modules = Mutex<HashMap<ModuleId, Arc<ModuleHandle>>>;

/// What a module handle looks like to the reader thread.
pub(crate) struct ModuleHandle {
    pub id: ModuleId,
    pub info: ModuleInfo,
    pub device: Mutex<HidDevice>,
    /// Set when the software-mode handshake (or its reverse) was sent — see `track_mode`.
    pub software_mode: AtomicBool,
    /// Set on the first read/write error and by `close`; only the first setter reports the
    /// disconnect.
    pub closed: AtomicBool,
    pub reader: Mutex<Option<JoinHandle<()>>>,
}

/// Decode one raw input report (report id first, as `hid_read` returns it) into events.
///
/// Pure: the reader thread and the unit tests share it. Reports that are not input reports
/// (anything but id `0x01`) are ignored — the interrupt IN endpoint only carries input.
pub(crate) fn decode_input_report(
    id: &ModuleId,
    info: &ModuleInfo,
    raw: &[u8],
) -> Vec<DeviceEvent> {
    if !matches!(view_report(raw), ReportView::Input { .. }) {
        return Vec::new();
    }
    ds_protocol::parse_input(info.kind, raw)
        .into_iter()
        .map(|input| DeviceEvent::Input {
            module: id.clone(),
            input,
        })
        .collect()
}

/// The reader thread body: read slices until the module is closed or the read fails. On a
/// read error the module is marked closed and the disconnect is reported once.
pub(crate) fn reader_loop(
    handle: Arc<ModuleHandle>,
    shared: Arc<super::Shared>,
    modules: Arc<Modules>,
) {
    let mut buf = [0u8; INPUT_REPORT_LEN];
    loop {
        if handle.closed.load(Ordering::SeqCst) {
            return;
        }
        let res = {
            let device = handle.device.lock().unwrap_or_else(|e| e.into_inner());
            device.read_timeout(&mut buf, READ_SLICE.as_millis() as i32)
        };
        // The lock is released here; leave it free for READ_GAP so a send/query waiting on
        // it can acquire it before the next slice re-locks (mutex barging would otherwise
        // starve the writer indefinitely).
        std::thread::sleep(READ_GAP);
        match res {
            Ok(0) => {} // slice timed out, no report
            Ok(n) => {
                for ev in decode_input_report(&handle.id, &handle.info, &buf[..n]) {
                    shared.push_event(ev);
                }
            }
            Err(e) => {
                // read error ⇒ the module is gone (unplugged / wedged): report the
                // disconnect once and stop this reader
                if !handle.closed.swap(true, Ordering::SeqCst) {
                    shared.module_gone(&modules, format!("read error: {e}"));
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::module_info;
    use ds_protocol::{Input, ModuleKind};

    fn id(kind: ModuleKind) -> ModuleId {
        ModuleId {
            kind,
            serial: "TEST".into(),
        }
    }

    fn raw_input(tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut r = vec![0x01, tag, payload.len() as u8, 0x00];
        r.extend_from_slice(payload);
        r.resize(INPUT_REPORT_LEN, 0);
        r
    }

    #[test]
    fn keys_report_decodes_to_a_key_state_event() {
        let info = module_info(ModuleKind::Keys);
        let mut payload = vec![0x02, 0x0F];
        payload.extend_from_slice(&[0u8; 15]);
        payload[2 + 9] = 1;
        let events = decode_input_report(&id(ModuleKind::Keys), &info, &raw_input(0x34, &payload));
        assert_eq!(events.len(), 1);
        let DeviceEvent::Input { module, input } = &events[0] else {
            panic!("not an input event")
        };
        assert_eq!(module.kind, ModuleKind::Keys);
        let Input::Keys(keys) = input else {
            panic!("not keys: {input:?}")
        };
        assert!(keys[9] && !keys[0] && !keys[14]);
    }

    #[test]
    fn fader_knob_roller_dial_and_orientation_reports_decode() {
        let faders = module_info(ModuleKind::Faders);
        let ev = decode_input_report(
            &id(ModuleKind::Faders),
            &faders,
            &raw_input(0x2A, &[0x02, 0x05, 0x33, 0x00, 0x64, 0x19, 0x4B]),
        );
        assert_eq!(
            ev,
            vec![DeviceEvent::Input {
                module: id(ModuleKind::Faders),
                input: Input::Faders([0x33, 0x00, 0x64, 0x19, 0x4B]),
            }]
        );

        let knobs = module_info(ModuleKind::Knobs);
        let ev = decode_input_report(
            &id(ModuleKind::Knobs),
            &knobs,
            &raw_input(0x2A, &[0x00, 0x03, 0x01, 0xFF, 0xFF]),
        );
        assert_eq!(
            ev,
            vec![DeviceEvent::Input {
                module: id(ModuleKind::Knobs),
                input: Input::Knob { index: 0, delta: 1 },
            }]
        );

        let rollers = module_info(ModuleKind::Rollers);
        let ev = decode_input_report(
            &id(ModuleKind::Rollers),
            &rollers,
            &raw_input(0x2A, &[0x01, 0x02, 0x00, 0xFF]),
        );
        assert_eq!(
            ev,
            vec![DeviceEvent::Input {
                module: id(ModuleKind::Rollers),
                input: Input::Roller { index: 0, delta: 1 },
            }]
        );

        let dial = module_info(ModuleKind::Dial);
        let ev = decode_input_report(
            &id(ModuleKind::Dial),
            &dial,
            &raw_input(0x44, &[0x03, 0x01, 0x00]),
        );
        assert_eq!(
            ev,
            vec![DeviceEvent::Input {
                module: id(ModuleKind::Dial),
                input: Input::Dial { delta: 1 },
            }]
        );

        let base = module_info(ModuleKind::Base);
        let ev = decode_input_report(&id(ModuleKind::Base), &base, &raw_input(0x2B, &[0x01]));
        assert_eq!(
            ev,
            vec![DeviceEvent::Input {
                module: id(ModuleKind::Base),
                input: Input::Orientation(ds_protocol::Orientation::Horizontal),
            }]
        );
    }

    #[test]
    fn unknown_tags_still_surface_as_unknown_input() {
        let info = module_info(ModuleKind::Knobs);
        let ev = decode_input_report(&id(ModuleKind::Knobs), &info, &raw_input(0x77, &[1, 2, 3]));
        assert_eq!(ev.len(), 1);
        assert!(matches!(
            ev[0],
            DeviceEvent::Input {
                input: Input::Unknown { tag: 0x77, .. },
                ..
            }
        ));
    }

    #[test]
    fn non_input_reports_are_ignored() {
        let info = module_info(ModuleKind::Keys);
        assert!(decode_input_report(&id(ModuleKind::Keys), &info, &[]).is_empty());
        assert!(decode_input_report(&id(ModuleKind::Keys), &info, &[0x02, 0x2D, 0x00]).is_empty());
        assert!(decode_input_report(&id(ModuleKind::Keys), &info, &[0x03, 0x3A, 0x01]).is_empty());
    }
}
