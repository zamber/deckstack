//! The real HID transport on Linux (DS-021): the [`Transport`] trait over actual hardware.
//!
//! Enumeration finds the Cooler Master modules by VID `0x2516` + the per-kind PIDs
//! ([`crate::module`]), each open module gets one [`hidapi::HidDevice`] behind a mutex and one
//! reader thread (see [`reader`]), and reports travel exactly like on the virtual transport:
//! every write is checked against the forbidden-command list and padded to the module's report
//! length before it leaves the host ([`prepare_write`]), and input reports are decoded into
//! [`DeviceEvent`]s. URI scheme `hid:` (open every attached module) and `hid:<os-path>` (one
//! module) are dispatched from [`crate::open`].
//!
//! Scope of this ticket: open, read (input reports, feature reports where the module answers
//! without a handshake — the M0 family) and close. The software-mode handshake and module init
//! are DS-022; no image or LED writes are sent from here. Reads on keys/dial are refused while
//! the module is in hardware mode (a read there wedges the firmware until power-cycled — the
//! pinned `mh-device` refuses the same way), which is why the transport tracks the handshake
//! reports sent through [`Transport::send`].
//!
//! Backend note: `hidapi` 2.6 requires exactly one Linux backend and `mh-device` (pinned git
//! dep, ADR-010) already selects `linux-static-libusb`, so this transport uses the same libusb
//! backend (it opens the `/dev/bus/usb/…` nodes through libusb and addresses devices by their
//! sysfs path `<bus>-<ports…>:<config>.<interface>`; it links the system libusb-1.0 via
//! pkg-config — no extra crates). The vendor-defined modules do get hidraw nodes on recent
//! kernels, but enabling the hidraw backend as well would break the hidapi build, so libusb it
//! is.

mod enumerate;
mod reader;

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ds_protocol::{ModuleKind, Report};
use hidapi::HidApi;
use mh_device::ModuleId;

use crate::error::{Error, Result};
use crate::module::{module_info, ModuleInfo, VENDOR_ID};
use crate::report::{ensure_allowed, pad_report};
use crate::transport::{DeviceEvent, Transport};
use crate::uri::DeviceUri;

pub(crate) use enumerate::enumerate_with;
pub use enumerate::{enumerate, hid_error, module_info_for_vid_pid, HidModuleInfo};
pub(crate) use reader::{reader_loop, ModuleHandle, Modules};

/// Open a device by URI: `hid:` opens every attached module, `hid:<os-path>` exactly one.
pub fn open_uri(uri: &DeviceUri) -> Result<HidTransport> {
    let rest = uri.rest();
    if rest.is_empty() {
        HidTransport::open_all()
    } else {
        let path = CString::new(rest).map_err(|_| Error::BadUri(uri.to_string()))?;
        HidTransport::open_path(&path)
    }
}

/// The event inbox all reader threads share (same shape as the virtual transport's).
pub(crate) struct Shared {
    inbox: Mutex<Inbox>,
    cond: Condvar,
}

struct Inbox {
    queue: VecDeque<DeviceEvent>,
    closed: bool,
    disconnect_reported: bool,
}

impl Shared {
    fn new() -> Self {
        Shared {
            inbox: Mutex::new(Inbox {
                queue: VecDeque::new(),
                closed: false,
                disconnect_reported: false,
            }),
            cond: Condvar::new(),
        }
    }

    fn push_event(&self, ev: DeviceEvent) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.queue.push_back(ev);
        drop(inbox);
        self.cond.notify_all();
    }

    fn close(&self) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.closed = true;
        drop(inbox);
        self.cond.notify_all();
    }

    /// A module went away: report the disconnect once; when the last module is gone the whole
    /// transport is closed.
    fn module_gone(&self, modules: &Modules, _why: String) {
        self.push_event(DeviceEvent::Disconnected);
        let all_dead = modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .all(|h| h.closed.load(Ordering::SeqCst));
        if all_dead {
            self.close();
        }
    }
}

/// The real HID transport: every attached (or one addressed) MasterHub module behind the same
/// [`Transport`] trait the virtual transport implements.
pub struct HidTransport {
    shared: Arc<Shared>,
    modules: Arc<Modules>,
    /// The id of each open module, by kind — filled as modules open, so [`Transport::module_id`]
    /// resolves lazily at call time (enumeration may happen after construction).
    ids: Mutex<HashMap<ModuleKind, ModuleId>>,
}

impl HidTransport {
    fn new() -> Self {
        HidTransport {
            shared: Arc::new(Shared::new()),
            modules: Arc::new(Mutex::new(HashMap::new())),
            ids: Mutex::new(HashMap::new()),
        }
    }

    /// Enumerate the attached modules and open every one that can be opened. Errors only when
    /// no module is attached at all or none could be opened (e.g. missing udev permissions).
    pub fn open_all() -> Result<Self> {
        let mut api = HidApi::new().map_err(hid_error)?;
        let infos = enumerate_with(&mut api)?;
        if infos.is_empty() {
            return Err(Error::Device(format!(
                "no MasterHub module (VID {VENDOR_ID:#06x}) is attached"
            )));
        }
        let transport = Self::new();
        let mut opened = 0;
        let mut last_err: Option<Error> = None;
        for info in &infos {
            match transport.open_module(&api, info) {
                Ok(()) => opened += 1,
                Err(e) => last_err = Some(e),
            }
        }
        if opened == 0 {
            return Err(last_err.unwrap_or_else(|| {
                Error::Device("no MasterHub module could be opened".to_string())
            }));
        }
        Ok(transport)
    }

    /// Open exactly one module by its OS device path (`hid:<path>`).
    pub fn open_path(path: &CStr) -> Result<Self> {
        let api = HidApi::new().map_err(hid_error)?;
        let device = api.open_path(path).map_err(hid_error)?;
        let dinfo = device.get_device_info().map_err(hid_error)?;
        let Some(info) = module_info_for_vid_pid(dinfo.vendor_id(), dinfo.product_id()) else {
            if dinfo.vendor_id() != VENDOR_ID {
                return Err(Error::Device(format!(
                    "{} is not a Cooler Master device (VID {:#06x})",
                    path.to_string_lossy(),
                    dinfo.vendor_id()
                )));
            }
            return Err(Error::UnknownPid(dinfo.product_id()));
        };
        let info = HidModuleInfo {
            info,
            path: path.to_owned(),
            serial: dinfo.serial_number().map(str::to_string),
            product_string: dinfo.product_string().map(str::to_string),
            module_id: enumerate::module_id_for(&info, dinfo.serial_number(), path),
        };
        let transport = Self::new();
        transport.open_module(&api, &info)?;
        Ok(transport)
    }

    /// Open one enumerated module on this transport and start its reader thread.
    fn open_module(&self, api: &HidApi, info: &HidModuleInfo) -> Result<()> {
        let device = api.open_path(&info.path).map_err(hid_error)?;
        let handle = Arc::new(ModuleHandle {
            id: info.module_id.clone(),
            info: info.info,
            device: Mutex::new(device),
            software_mode: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            reader: Mutex::new(None),
        });
        let reader_shared = Arc::clone(&self.shared);
        let reader_modules = Arc::clone(&self.modules);
        let reader_handle = Arc::clone(&handle);
        let reader = std::thread::Builder::new()
            .name(format!("ds-hid-read-{}", info.info.kind.name()))
            .spawn(move || reader_loop(reader_handle, reader_shared, reader_modules))
            .map_err(Error::Io)?;
        *handle.reader.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader);
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(info.module_id.clone(), handle);
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(info.info.kind, info.module_id.clone());
        Ok(())
    }

    /// The modules this transport is connected to.
    pub fn modules(&self) -> Vec<ModuleId> {
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    fn lookup(&self, module: &ModuleId) -> Option<Arc<ModuleHandle>> {
        self.modules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(module)
            .cloned()
    }

    fn is_closed(&self) -> bool {
        self.shared
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .closed
    }
}

impl Drop for HidTransport {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// Guard + pad before any write: refuse forbidden reports (the ds-protocol list, not a copy)
/// and pad to the module's report length exactly like the HID layer does.
fn prepare_write(info: &ModuleInfo, report: &Report) -> Result<Vec<u8>> {
    ensure_allowed(report.bytes())?;
    Ok(pad_report(info, report))
}

/// Remember software-mode handshakes sent through `send`, so `query` can refuse reads on
/// keys/dial while they are still in hardware mode (a read there wedges them until power
/// cycle — `upstream/PROTOCOL.md`; the pinned mh-device tracks the same reports).
fn track_mode(kind: ModuleKind, flag: &AtomicBool, report: &Report) {
    if let Some((on, _mode)) = ds_protocol::enter_software_mode(kind) {
        if on.bytes() == report.bytes() {
            flag.store(true, Ordering::SeqCst);
            return;
        }
    }
    if let Some(off) = ds_protocol::leave_software_mode(kind) {
        if off.bytes() == report.bytes() {
            flag.store(false, Ordering::SeqCst);
        }
    }
}

impl Transport for HidTransport {
    fn send(&self, module: &ModuleId, report: &Report) -> Result<usize> {
        // Refuse forbidden reports before any byte leaves the host, then pad like the HID
        // layer does. HID writes are synchronous control/interrupt transfers: the byte count
        // `hid_write` returns is the ack the virtual transport waits for.
        let padded = prepare_write(&module_info(module.kind), report)?;
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let Some(handle) = self.lookup(module) else {
            return Err(Error::Device(format!("module {module} is not connected")));
        };
        if handle.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        track_mode(handle.info.kind, &handle.software_mode, report);
        let device = handle.device.lock().unwrap_or_else(|e| e.into_inner());
        match device.write(&padded) {
            Ok(n) => Ok(n),
            Err(e) => {
                if !handle.closed.swap(true, Ordering::SeqCst) {
                    self.shared
                        .module_gone(&self.modules, format!("write error: {e}"));
                }
                Err(Error::Device(format!("hid write to {module}: {e}")))
            }
        }
    }

    fn query(&self, module: &ModuleId, feature_id: u8, timeout: Duration) -> Result<Vec<u8>> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let Some(handle) = self.lookup(module) else {
            return Err(Error::Device(format!("module {module} is not connected")));
        };
        if handle.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        // Keys/dial only answer reads in software mode; reading them in hardware mode wedges
        // them until power-cycled. The handshake itself is DS-022 — until it is sent, refuse.
        if handle.info.kind.needs_software_mode() && !handle.software_mode.load(Ordering::SeqCst) {
            return Err(Error::Device(format!(
                "{} is in hardware mode; a feature read now would wedge it — send the \
                 software-mode handshake first (DS-022)",
                handle.info.kind.name()
            )));
        }
        // `hid_get_feature_report` has no per-call timeout (hidapi bounds the control
        // transfer internally), so run it on a helper thread and enforce the trait's timeout
        // on the wait — same observable semantics as the virtual transport's answer wait.
        let info = handle.info;
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let handle = Arc::clone(&handle);
            std::thread::Builder::new()
                .name(format!("ds-hid-query-{}", info.kind.name()))
                .spawn(move || {
                    let mut buf = vec![0u8; info.feature_len + 1];
                    buf[0] = feature_id;
                    let res = {
                        let device = handle.device.lock().unwrap_or_else(|e| e.into_inner());
                        device
                            .get_feature_report(&mut buf)
                            .map(|n| buf[..n].to_vec())
                    };
                    let _ = tx.send(res);
                })
                .map_err(Error::Io)?;
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(answer)) if !answer.is_empty() => Ok(answer),
            Ok(Ok(_)) => Err(Error::Timeout(timeout)), // no answer within the transfer bound
            Ok(Err(e)) => Err(Error::Device(format!("feature read on {module}: {e}"))),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(Error::Timeout(timeout)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }

    fn recv(&self, timeout: Duration) -> Result<DeviceEvent> {
        let mut inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(ev) = inbox.queue.pop_front() {
                return Ok(ev);
            }
            if inbox.closed {
                if !inbox.disconnect_reported {
                    inbox.disconnect_reported = true;
                    return Ok(DeviceEvent::Disconnected);
                }
                return Err(Error::Closed);
            }
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(Error::Timeout(timeout))?;
            let (guard, _) = self
                .shared
                .cond
                .wait_timeout(inbox, remaining)
                .unwrap_or_else(|e| e.into_inner());
            inbox = guard;
        }
    }

    fn close(&self) -> Result<()> {
        {
            let inbox = self.shared.inbox.lock().unwrap_or_else(|e| e.into_inner());
            if inbox.closed {
                return Ok(());
            }
        }
        // Stop the readers and drop the handles. No handback report is sent here — that is
        // DS-024; this ticket only opens, reads and closes.
        let handles: Vec<Arc<ModuleHandle>> = {
            let mut modules = self.modules.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *modules).into_values().collect()
        };
        for h in &handles {
            h.closed.store(true, Ordering::SeqCst);
        }
        for h in &handles {
            if let Some(reader) = h.reader.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = reader.join();
            }
        }
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.shared.close();
        Ok(())
    }

    fn module_id(&self, kind: ModuleKind) -> Option<ModuleId> {
        // Lazy lookup in the id map filled at open time: the enumerated serial of the module
        // of this kind that this transport actually has open (None when it is not connected).
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&kind)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::module_info;
    use ds_protocol::ModuleKind;

    #[test]
    fn prepare_write_pads_like_the_hid_layer() {
        let keys = module_info(ModuleKind::Keys);
        let faders = module_info(ModuleKind::Faders);
        let (handshake, _mode) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
        assert_eq!(
            prepare_write(&keys, &handshake).unwrap().len(),
            keys.feature_len
        );
        let led =
            ds_protocol::led_color(ModuleKind::Faders, ds_protocol::LedTarget::All, [1, 2, 3])
                .unwrap();
        let padded = prepare_write(&faders, &led).unwrap();
        assert_eq!(padded.len(), faders.out_len);
        assert_eq!(&padded[..led.bytes().len()], led.bytes());
        let img = ds_protocol::panel_image(&[0xFF; 10], keys.out_len);
        assert_eq!(prepare_write(&keys, &img[0]).unwrap().len(), keys.out_len);
    }

    #[test]
    fn prepare_write_refuses_forbidden_bytes() {
        // The guard is ds-protocol's list (report::ensure_allowed → ds_protocol::is_forbidden);
        // builder output never trips it, raw forbidden bytes do.
        assert!(matches!(
            crate::report::ensure_allowed(&[0x02, 0x2A, 0x00, 0x00]),
            Err(Error::Forbidden)
        ));
        assert!(matches!(
            crate::report::ensure_allowed(&[0x03, 0x35, b'M', b'H']),
            Err(Error::Forbidden)
        ));
    }

    #[test]
    fn track_mode_follows_the_handshake_reports() {
        let flag = AtomicBool::new(false);
        let (on, _mode) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
        track_mode(ModuleKind::Keys, &flag, &on);
        assert!(flag.load(Ordering::SeqCst));
        let off = ds_protocol::leave_software_mode(ModuleKind::Keys).unwrap();
        track_mode(ModuleKind::Keys, &flag, &off);
        assert!(!flag.load(Ordering::SeqCst));
        // an unrelated report does not change the flag
        let fill = ds_protocol::fill_screen(ModuleKind::Keys, [9, 9, 9]).unwrap();
        track_mode(ModuleKind::Keys, &flag, &fill);
        assert!(!flag.load(Ordering::SeqCst));
        // the M0 family has no handshake
        assert!(ds_protocol::enter_software_mode(ModuleKind::Base).is_none());
    }

    #[test]
    fn hid_uri_parsing_dispatches_by_rest() {
        let all = DeviceUri::parse("hid:").unwrap();
        assert_eq!(all.scheme(), "hid");
        assert!(all.rest().is_empty());
        let one = DeviceUri::parse("hid:/dev/bus/usb/003/012").unwrap();
        assert_eq!(one.rest(), "/dev/bus/usb/003/012");
        // an interior nul cannot be an OS path — refused before any hardware is touched
        let bad = DeviceUri::parse("hid:a\u{0}b").unwrap();
        assert!(matches!(open_uri(&bad), Err(Error::BadUri(_))));
    }

    #[test]
    fn module_info_for_pid_covers_the_transport_lookup() {
        for kind in ModuleKind::ALL {
            let info = module_info(kind);
            assert_eq!(
                crate::module::module_info_for_pid(info.pid).unwrap().kind,
                kind
            );
        }
    }

    /// The serials of the five modules of the DS-097 smoke deck (kind -> USB serial).
    fn deck_serials() -> Vec<(ModuleKind, &'static str)> {
        vec![
            (ModuleKind::Base, "MHBS01AA221252301000"),
            (ModuleKind::Keys, "MHKB03AA221252301767"),
            (ModuleKind::Knobs, "MHKD15AA221252301042"),
            (ModuleKind::Rollers, "MHRR02AA221252301533"),
            (ModuleKind::Dial, "MHDD01AA221252301288"),
        ]
    }

    #[test]
    fn hid_transport_resolves_each_kind_to_its_own_enumerated_id() {
        // build a transport the way open_all does, minus the hardware: register the
        // enumerated module ids exactly as open_module does
        let t = HidTransport::new();
        for (kind, serial) in deck_serials() {
            t.ids.lock().unwrap().insert(
                kind,
                ModuleId {
                    kind,
                    serial: serial.to_string(),
                },
            );
        }
        for (kind, serial) in deck_serials() {
            let id = crate::transport::module_id_for_send(&t, kind).unwrap();
            assert_eq!(id.kind, kind);
            assert_eq!(id.serial, serial, "{kind:?} must resolve to its own serial");
        }
        // the keys panel lives on the keys module
        assert_eq!(
            t.module_id(ModuleKind::Keys).unwrap().serial,
            "MHKB03AA221252301767"
        );
        // a kind that is not connected resolves to None and errors typed, never panics
        assert!(t.module_id(ModuleKind::Faders).is_none());
        let err = crate::transport::module_id_for_send(&t, ModuleKind::Faders).unwrap_err();
        assert!(
            matches!(&err, Error::Device(m) if m == "module faders not connected on this transport"),
            "{err}"
        );
        // after close the ids are gone too
        t.close().unwrap();
        assert!(t.module_id(ModuleKind::Keys).is_none());
    }
}
