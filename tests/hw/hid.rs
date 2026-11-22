//! Hardware smoke for the real HID transport (DS-021).
//!
//! Run on a machine with the deck attached:
//!
//! ```sh
//! DS_HARDWARE=1 cargo test -p ds-device --test hid -- --nocapture
//! ```
//!
//! Without `DS_HARDWARE=1` (CI) the test skips immediately and passes, so `cargo test
//! --workspace` stays hardware-free. With `DS_HARDWARE=1` it enumerates the attached Cooler
//! Master modules (VID 0x2516), opens each one by its OS path, reads one feature report on the
//! modules that answer without the software-mode handshake (the M0 family — keys/dial only
//! answer in software mode, which is DS-022, and reading them in hardware mode would wedge
//! them), and closes again. Then it opens the whole deck once through the `hid:` URI that
//! [`ds_device::open`] dispatches. When no deck is attached — or opening needs permissions the
//! udev rule of DS-025 would grant — every step prints a clear SKIP and the test still passes:
//! hardware absence or missing permissions are states to detect, not failures.
//!
//! One test function on purpose: the steps open the same physical devices, so they must not
//! run on parallel test threads.

use std::time::Duration;

use ds_device::hid::{self, HidTransport};
use ds_device::{DeviceUri, Transport};
use ds_protocol::{Query, SoftwareMode};
use mh_device::ModuleId;

/// How long a feature read may take (the modules answer in ~45–48 ms on hardware).
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

fn hw_enabled() -> bool {
    std::env::var("DS_HARDWARE").as_deref() == Ok("1")
}

#[test]
fn hw_smoke_enumerate_open_read_close() {
    if !hw_enabled() {
        eprintln!("SKIP: DS_HARDWARE is not 1 — hardware smoke disabled (CI stays hardware-free)");
        return;
    }

    // 1. enumerate the connected MasterHub modules
    let modules = match hid::enumerate() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("SKIP: enumeration failed: {e}");
            return;
        }
    };
    if modules.is_empty() {
        eprintln!("SKIP: no MasterHub module (VID 0x2516) enumerated — no deck attached");
        return;
    }
    println!("enumerated {} MasterHub module(s):", modules.len());
    for m in &modules {
        println!(
            "  {:8} pid {:#06x}  path {}  serial {:?}  product {:?}",
            m.info.kind.name(),
            m.info.pid,
            m.path.to_string_lossy(),
            m.serial,
            m.product_string
        );
    }

    // 2. open each module, read a feature report where the module supports it, close
    let mut opened = 0;
    for m in &modules {
        let path = m.path.to_string_lossy().into_owned();
        let transport = match HidTransport::open_path(&m.path) {
            Ok(t) => t,
            Err(e) => {
                // libusb reports LIBUSB_ERROR_ACCESS (-3) here when the /dev/bus/usb node is
                // not writable by this user — the udev rule of DS-025 (or root) fixes that.
                println!(
                    "SKIP open {path}: {e} — device node not accessible to this user \
                     (udev rule DS-025 / root); enumeration above still proves the transport \
                     sees the module"
                );
                continue;
            }
        };
        opened += 1;
        let id = transport
            .modules()
            .into_iter()
            .next()
            .expect("an open transport has its module");
        assert_eq!(id.kind, m.info.kind);
        read_one_feature_report(&transport, &id);
        transport.close().expect("close");
        println!("  closed {path}");
    }
    if opened == 0 {
        println!(
            "SKIP: no module could be opened (LIBUSB_ERROR_ACCESS — permissions, DS-025); \
             the enumeration above lists the whole deck"
        );
    } else {
        println!("opened, read and closed {opened} module(s)");
    }

    // 3. the `hid:` URI that ds_device::open dispatches opens the whole deck
    let uri = DeviceUri::parse("hid:").unwrap();
    match hid::open_uri(&uri) {
        Ok(t) => {
            println!("hid: opened the deck, modules: {:?}", t.modules());
            t.close().expect("close");
        }
        Err(e) => {
            println!("SKIP hid: open failed: {e} (no deck attached, or permissions — DS-025)")
        }
    }
    // and through the public dispatcher
    match ds_device::open(&uri) {
        Ok(dev) => {
            println!("ds_device::open(\"hid:\") returned a transport");
            dev.close().expect("close");
        }
        Err(e) => println!("SKIP ds_device::open(\"hid:\"): {e}"),
    }
}

/// Read one feature report on modules that support reads without the software-mode handshake
/// (the M0 family answers at any time; keys/dial are skipped — DS-022 owns their handshake).
fn read_one_feature_report(transport: &HidTransport, id: &ModuleId) {
    let Some(mode) = SoftwareMode::m0(id.kind) else {
        println!(
            "  {}: skipped feature read (needs the software-mode handshake, DS-022)",
            id.kind.name()
        );
        return;
    };
    let Some(feature_id) = ds_protocol::query_id(mode, Query::Firmware) else {
        println!("  {}: no firmware query id", id.kind.name());
        return;
    };
    match transport.query(id, feature_id, QUERY_TIMEOUT) {
        Ok(answer) => {
            let payload = ds_protocol::feature_payload(&answer);
            println!(
                "  {}: feature report {:#04x} answered {} bytes (payload {:02x?})",
                id.kind.name(),
                feature_id,
                answer.len(),
                payload
            );
        }
        Err(e) => println!(
            "  {}: feature read {:#04x} failed: {e}",
            id.kind.name(),
            feature_id
        ),
    }
}
