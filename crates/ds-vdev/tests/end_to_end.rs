//! End-to-end proof that ds-vdev speaks exactly what ds-device's VirtualTransport speaks:
//! a real `VirtualTransport` connects to a `VdevServer` over the Unix socket, runs the
//! software-mode handshake, pushes chunked images, runs feature queries, receives injected
//! input, and the reassembled frame sha matches — all in-repo, no hardware, no daemon
//! (the `ds daemon` smoke is deferred to DS-018/DS-020, which land DS-015/DS-016 first).

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ds_device::frame::{kind, read_frame, write_frame, Frame};
use ds_device::transport::{DeviceEvent, Transport};
use ds_device::vtransport::{virtual_module_id, VirtualTransport};
use ds_device::DeviceUri;
use ds_protocol::{self, Input, ModuleKind};
use ds_vdev::{ctl, sha256_hex, HubConfig, InjectEvent, Throttle, VdevServer};

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-vdev-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn start(dir: &std::path::Path, name: &str, config: HubConfig) -> VdevServer {
    VdevServer::start(dir.join(name), config).unwrap()
}

#[test]
fn virtual_transport_end_to_end_against_vdev() {
    let dir = test_dir("e2e");
    let server = start(&dir, "vdev.sock", HubConfig::default());
    let path = server.socket_path().to_path_buf();

    // open through the same URI path the daemon uses
    let uri = DeviceUri::parse(&format!("virtual:{}", path.display())).unwrap();
    let dev = ds_device::open(&uri).unwrap();
    let keys = virtual_module_id(ModuleKind::Keys);
    let dial = virtual_module_id(ModuleKind::Dial);

    // software-mode handshake: FEATURE_SET 03 3A 01 padded to 32 B, acked with 32
    let (handshake, _mode) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
    assert_eq!(dev.send(&keys, &handshake).unwrap(), 32);

    // a chunked panel image: every chunk acked with 320, reassembled per target with sha
    let jpeg: Vec<u8> = (0..2000u32).map(|i| (i % 256) as u8).collect();
    let reports = ds_protocol::panel_image(&jpeg, 320);
    assert!(reports.len() > 1);
    for r in &reports {
        assert_eq!(dev.send(&keys, r).unwrap(), 320);
    }
    // the golden-frame oracle: ds-vdevctl frames panel == sha256(jpeg)
    let lines = ctl::request(&path, "frames panel").unwrap();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with(&format!("panel {} {}B", sha256_hex(&jpeg), jpeg.len())),
        "{}",
        lines[0]
    );

    // key and dial images land on their own targets (multiple targets at once)
    let key_img = vec![0xABu8; 700];
    for r in ds_protocol::key_image(0, &key_img, 320) {
        assert_eq!(dev.send(&keys, &r).unwrap(), 320);
    }
    let dial_img = vec![0xCDu8; 500];
    let (dial_hs, _) = ds_protocol::enter_software_mode(ModuleKind::Dial).unwrap();
    assert_eq!(dev.send(&dial, &dial_hs).unwrap(), 32);
    for r in ds_protocol::dial_image(&dial_img, 320) {
        assert_eq!(dev.send(&dial, &r).unwrap(), 320);
    }
    let lines = ctl::request(&path, "frames").unwrap();
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert!(lines
        .iter()
        .any(|l| l.starts_with(&format!("key:0 {} ", sha256_hex(&key_img)))));
    assert!(lines
        .iter()
        .any(|l| l.starts_with(&format!("dial {} ", sha256_hex(&dial_img)))));

    // feature queries round-trip: firmware, serial, screen info, backlight
    let fw = dev.query(&keys, 0x38, Duration::from_secs(2)).unwrap();
    assert_eq!(fw[0], 0x03);
    assert_eq!(fw[1], 0x38);
    assert_eq!(
        ds_protocol::parse_firmware(&fw[1..]).as_deref(),
        Some(ds_vdev::hub::VIRTUAL_FIRMWARE)
    );
    let sn = dev.query(&keys, 0x3A, Duration::from_secs(2)).unwrap();
    assert_eq!(
        ds_protocol::parse_serial_number(&sn[1..]).as_deref(),
        Some("VDEV-KEYS-01FE")
    );
    let info = dev.query(&keys, 0x3B, Duration::from_secs(2)).unwrap();
    assert_eq!(ds_protocol::parse_screen_size(&info[1..]), Some((480, 272)));
    let bl = dev.query(&dial, 0x4C, Duration::from_secs(2)).unwrap();
    assert_eq!(
        ds_protocol::parse_percent(&bl[1..]),
        Some(ds_vdev::hub::DEFAULT_BACKLIGHT)
    );

    // reports + writes over the control socket
    let reports = ctl::request(&path, "reports").unwrap();
    assert_eq!(reports.len(), 6);
    assert!(reports
        .iter()
        .any(|r| r.contains("keys pid=0x01fe") && r.contains("software_mode=true")));
    let writes = ctl::request(&path, "writes 5").unwrap();
    assert_eq!(writes.len(), 5);
    assert!(writes
        .iter()
        .all(|w| (w.contains("kind=0x01") || w.contains("kind=0x02")) && !w.contains("refused")));

    // inject: a scripted key event reaches the host as a decoded input event
    let n = server
        .hub()
        .inject(&InjectEvent {
            module: "keys".into(),
            at_ms: 0,
            keys: Some(vec![3]),
            faders: None,
            knob: None,
            knob_delta: None,
            knob_press: None,
            pressed: None,
            roller: None,
            roller_delta: None,
            dial_delta: None,
            dial_pressed: None,
            orientation: None,
        })
        .unwrap();
    assert_eq!(n, 1, "one connected host");
    let ev = dev.recv(Duration::from_secs(2)).unwrap();
    let DeviceEvent::Input { module, input } = ev else {
        panic!("expected input event, got {ev:?}")
    };
    assert_eq!(module.kind, ModuleKind::Keys);
    assert!(matches!(input, Input::Keys(k) if k[3] && !k[0]));

    dev.close().unwrap();
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn forbidden_write_gets_an_error_frame_not_an_ack() {
    let dir = test_dir("forbidden");
    let server = start(&dir, "vdev.sock", HubConfig::default());
    let mut stream = UnixStream::connect(server.socket_path()).unwrap();
    // 02 34 … on keys is a forbidden firmware-update command
    let mut payload = vec![0x02u8, 0x34, 0x00, 0x00];
    payload.resize(320, 0);
    write_frame(
        &mut stream,
        &Frame::new(kind::WRITE, 0x01FE, payload).unwrap(),
    )
    .unwrap();
    let reply = read_frame(&mut stream).unwrap();
    assert_eq!(reply.kind, kind::ERROR);
    assert_eq!(reply.pid, 0x01FE);
    assert!(String::from_utf8_lossy(&reply.payload).contains("forbidden"));
    // the write log records the refusal
    let writes = ctl::request(server.socket_path(), "writes").unwrap();
    assert!(writes.iter().any(|w| w.contains("refused")));
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn throttle_kbps_delays_the_acks() {
    let dir = test_dir("throttle");
    // 64 kbit/s = 8000 B/s: a 320 B report takes 40 ms on the device
    let server = start(
        &dir,
        "vdev.sock",
        HubConfig {
            throttle: Throttle {
                kbps: Some(64),
                drop_after: None,
            },
            ..HubConfig::default()
        },
    );
    let t = VirtualTransport::connect(server.socket_path())
        .unwrap()
        .with_write_timeout(Duration::from_secs(10));
    let keys = virtual_module_id(ModuleKind::Keys);
    let jpeg = vec![0x5Au8; 700]; // 3 chunks of 312/312/76 bytes
    let reports = ds_protocol::panel_image(&jpeg, 320);
    assert_eq!(reports.len(), 3);
    let start = Instant::now();
    for r in &reports {
        assert_eq!(t.send(&keys, r).unwrap(), 320);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(100),
        "3 x 320 B at 64 kbit/s must take >= 120 ms, took {elapsed:?}"
    );
    // and the runtime knob turns it off again
    let lines = ctl::request(server.socket_path(), "throttle off off").unwrap();
    assert!(lines[0].contains("kbps=off"));
    let start = Instant::now();
    assert_eq!(t.send(&keys, &reports[0]).unwrap(), 320);
    assert!(
        start.elapsed() < Duration::from_millis(100),
        "unthrottled again"
    );
    t.close().unwrap();
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn drop_after_simulates_hot_unplug() {
    let dir = test_dir("drop");
    let server = start(
        &dir,
        "vdev.sock",
        HubConfig {
            throttle: Throttle {
                kbps: None,
                drop_after: Some(2),
            },
            ..HubConfig::default()
        },
    );
    let t = VirtualTransport::connect(server.socket_path())
        .unwrap()
        .with_write_timeout(Duration::from_secs(2));
    let keys = virtual_module_id(ModuleKind::Keys);
    let (handshake, _) = ds_protocol::enter_software_mode(ModuleKind::Keys).unwrap();
    assert!(t.send(&keys, &handshake).is_ok());
    assert!(t.send(&keys, &handshake).is_ok());
    // third write is never acked: the device dropped the connection
    assert!(t.send(&keys, &handshake).is_err());
    assert!(matches!(
        t.recv(Duration::from_secs(2)),
        Ok(DeviceEvent::Disconnected) | Err(_)
    ));
    server.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
}
