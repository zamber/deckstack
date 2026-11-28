//! DS-112 permutation harness: the virtual model of the configuration space the owner wants
//! tested — (module kind × connector × base orientation). Every SALON-001 connector slot
//! (`docs/discovery/salon-001/connector-map-*.csv`, 27 module port paths + the base MCU's
//! `1-4-4`) is combined with every module kind and both base orientations; for each
//! permutation the real `VirtualTransport` connects to a real `VdevServer`, handshakes, and
//! must resolve the module's identity key to exactly `(slot, orientation)` with the
//! connector position derived from the port path. Control addressing stays logical: a frame
//! push to `key:0` reassembles with an intact sha256 no matter where the keys module sits.
//!
//! A second case proves the move/replug scenario at the transport level: move the keys
//! module to a different slot, replug, reconnect — the slot read changes, the identity key
//! follows the position, and a re-pushed frame keeps its sha256.

use std::path::PathBuf;
use std::time::Duration;

use ds_device::handshake::HandshakeConfig;
use ds_device::hotplug;
use ds_device::slot::{DeviceKey, Slot, BASE_MCU_SLOT};
use ds_device::transport::Transport;
use ds_device::vtransport::virtual_module_id;
use ds_device::DeviceUri;
use ds_protocol::{ModuleKind, Orientation};
use ds_vdev::{sha256_hex, Hub, HubConfig, Target, VdevServer};

/// Every connector port path observed in the SALON-001 sweeps, plus the base MCU slot.
const CONNECTORS: [&str; 28] = [
    "1-1-1", "1-1-2", "1-1-3", "1-1-4", "1-2-1", "1-2-2", "1-2-3", "1-2-4", "1-3-1", "1-3-2",
    "1-3-3", "1-3-4", "1-4-1", "1-4-2", "1-4-3", "1-4-4", "2-1", "2-2", "2-3", "2-4", "3-1", "3-2",
    "3-3", "3-4", "4-1", "4-2", "4-3", "4-4",
];

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-vdev-ds112-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A fast-but-real handshake config: the ≥600 ms settle of the default is proven by the
/// handshake unit tests; the permutation matrix exercises identity, not wall-clock waits.
fn fast_cfg() -> HandshakeConfig {
    HandshakeConfig {
        settle: Duration::from_millis(1),
        re_settle: Duration::from_millis(1),
        retry_gap: Duration::from_millis(1),
        attempts: 2,
        query_timeout: Duration::from_millis(200),
    }
}

/// A deterministic 72x72 "JPEG" payload for the key transfer (the reassembler only needs a
/// stable byte stream; the sha oracle is over these bytes).
fn solid_jpeg_key0(rgb: [u8; 3]) -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8, 0xFF];
    for i in 0..700u32 {
        v.push(rgb[(i as usize) % 3] ^ (i % 251) as u8);
    }
    v.push(0xFF);
    v.push(0xD9);
    v
}

#[test]
fn permutation_matrix_kind_x_connector_x_orientation() {
    let dir = test_dir("matrix");
    let sock = dir.join("vdev.sock");
    let server = VdevServer::start(&sock, HubConfig::default()).unwrap();
    let hub: &Hub = &server.hub();
    let uri = DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap();

    let mut cases = 0u32;
    for kind in ModuleKind::ALL {
        for slot_s in CONNECTORS {
            let slot = Slot::parse(slot_s).unwrap();
            for orientation in [Orientation::Horizontal, Orientation::Vertical] {
                // configure the virtual deck: module moved to this connector, base rotated
                if kind == ModuleKind::Base && slot_s != BASE_MCU_SLOT {
                    continue; // the base MCU sits at its own slot
                }
                hub.set_slot(kind, slot.clone()).unwrap();
                hub.set_orientation(orientation);

                // a fresh connection per permutation (a replug): handshake + slot read
                let t = ds_device::open(&uri).unwrap();
                let id = virtual_module_id(kind);
                hotplug::handshake_module(&*t, &id, fast_cfg()).unwrap();
                let got_slot = t.module_slot(kind).expect("slot read");
                assert_eq!(got_slot, slot, "{kind:?} at {slot_s}");
                assert_eq!(got_slot.position(), slot.position());

                // the identity key is exactly (slot, orientation) in addition to the serial
                let keys = hotplug::identity_keys(&*t, orientation);
                let key = keys
                    .iter()
                    .find(|(mid, _)| mid.kind == kind)
                    .map(|(_, k)| k.clone())
                    .expect("identity key");
                assert_eq!(key, DeviceKey::new(slot.clone(), orientation));
                // a placement rescan right after connect sees no change (idempotent)
                assert!(hotplug::placement_changes(&*t, orientation, &keys).is_empty());

                // control addressing stays logical: any module in any connector is addressed
                // by kind (the frame sweep below proves the sha oracle per connector)
                t.close().unwrap();
                cases += 1;
            }
        }
    }
    // 5 module kinds x 28 slots x 2 orientations + the base at its own slot x 2 = 282
    assert_eq!(
        cases,
        5 * 28 * 2 + 2,
        "expected the full matrix, got {cases}"
    );
    let _ = server.hub().orientation();
}

#[test]
fn keys_frame_sha_intact_across_every_connector_and_orientation() {
    let dir = test_dir("frames");
    let sock = dir.join("vdev.sock");
    let server = VdevServer::start(&sock, HubConfig::default()).unwrap();
    let hub = server.hub();
    let uri = DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap();

    // one representative sweep: every connector slot, both orientations, keys module moved
    // around — the key:0 frame sha must be identical at every seat (logical addressing).
    let want = sha256_hex(&solid_jpeg_key0([0x3c, 0xc8, 0x6e]));
    for slot_s in CONNECTORS {
        if slot_s == BASE_MCU_SLOT {
            continue;
        }
        for orientation in [Orientation::Horizontal, Orientation::Vertical] {
            hub.set_slot(ModuleKind::Keys, Slot::parse(slot_s).unwrap())
                .unwrap();
            hub.set_orientation(orientation);
            let t = ds_device::open(&uri).unwrap();
            let keys = virtual_module_id(ModuleKind::Keys);
            hotplug::handshake_module(&*t, &keys, fast_cfg()).unwrap();
            assert_eq!(t.module_slot(ModuleKind::Keys).unwrap().as_str(), slot_s);
            let img = solid_jpeg_key0([0x3c, 0xc8, 0x6e]);
            for r in ds_protocol::key_image(0, &img, 320) {
                t.send(&keys, &r).unwrap();
            }
            let entry = hub.frame(Target::Key(0)).expect("frame stored");
            assert_eq!(entry.sha256, want, "slot {slot_s} {orientation:?}");
            t.close().unwrap();
        }
    }
}

#[test]
fn move_then_replug_keeps_position_identity_and_frame_sha() {
    let dir = test_dir("move-replug");
    let sock = dir.join("vdev.sock");
    let server = VdevServer::start(&sock, HubConfig::default()).unwrap();
    let hub = server.hub();
    let uri = DeviceUri::parse(&format!("virtual:{}", sock.display())).unwrap();

    // initial seating: keys at its rot0 anchor 1-3-4, base flat
    let t = ds_device::open(&uri).unwrap();
    let keys = virtual_module_id(ModuleKind::Keys);
    hotplug::handshake_module(&*t, &keys, fast_cfg()).unwrap();
    assert_eq!(t.module_slot(ModuleKind::Keys).unwrap().as_str(), "1-3-4");
    let known = hotplug::identity_keys(&*t, Orientation::Horizontal);
    assert!(hotplug::placement_changes(&*t, Orientation::Horizontal, &known).is_empty());

    // push key:0 and record the sha
    let img = solid_jpeg_key0([0xff, 0x88, 0x00]);
    for r in ds_protocol::key_image(0, &img, 320) {
        t.send(&keys, &r).unwrap();
    }
    let sha_before = hub.frame(Target::Key(0)).unwrap().sha256;
    assert_eq!(sha_before, sha256_hex(&img));

    // MOVE the keys module to another connector (same serial/pid): the identity key changes
    // with the position, and a rescan of the still-open transport sees it
    hub.set_slot(ModuleKind::Keys, Slot::parse("3-1").unwrap())
        .unwrap();
    let changes = hotplug::placement_changes(&*t, Orientation::Horizontal, &known);
    assert_eq!(changes.len(), 1, "the move is a placement change");
    assert_eq!(
        changes[0].1,
        DeviceKey::new(Slot::parse("3-1").unwrap(), Orientation::Horizontal)
    );
    assert_eq!(t.module_slot(ModuleKind::Keys).unwrap().position(), 24);
    t.close().unwrap();

    // REPLUG: connections drop; a fresh connect lands on the same hub at the new slot
    hub.replug();
    let t2 = ds_device::open(&uri).unwrap();
    hotplug::handshake_module(&*t2, &keys, fast_cfg()).unwrap();
    assert_eq!(t2.module_slot(ModuleKind::Keys).unwrap().as_str(), "3-1");
    // re-push the same frame after the replug: sha intact
    for r in ds_protocol::key_image(0, &img, 320) {
        t2.send(&keys, &r).unwrap();
    }
    let entry = hub.frame(Target::Key(0)).unwrap();
    assert_eq!(
        entry.sha256, sha_before,
        "frame sha intact after move+replug+re-push"
    );

    // base rotation re-keys every module without any USB event
    hub.set_orientation(Orientation::Vertical);
    let t3 = ds_device::open(&uri).unwrap();
    hotplug::handshake_module(&*t3, &keys, fast_cfg()).unwrap();
    let keyed = hotplug::identity_keys(&*t3, Orientation::Vertical);
    assert!(keyed
        .iter()
        .any(|(_, k)| k == &DeviceKey::new(Slot::parse("3-1").unwrap(), Orientation::Vertical)));
    t3.close().unwrap();
    t2.close().unwrap();
}
