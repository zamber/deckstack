//! Behaviour tests for the re-exported mh-protocol builders: the wire format stays pinned to
//! the hardware-verified upstream facts (`upstream/PROTOCOL.md` at the pinned rev).

use ds_protocol::*;

#[test]
fn handshake_and_handback_bytes() {
    let (hs, mode) = enter_software_mode(ModuleKind::Keys).unwrap();
    assert_eq!(hs.bytes(), &[0x03, 0x3A, 0x01]);
    assert_eq!(mode.kind(), ModuleKind::Keys);
    let (hs, mode) = enter_software_mode(ModuleKind::Dial).unwrap();
    assert_eq!(hs.bytes(), &[0x03, 0x49, 0x01]);
    assert_eq!(mode.kind(), ModuleKind::Dial);
    assert_eq!(
        leave_software_mode(ModuleKind::Keys).unwrap().bytes(),
        &[0x03, 0x3A, 0x00]
    );
}

#[test]
fn led_report_bytes() {
    let r = led_color(ModuleKind::Faders, LedTarget::Index(2), [1, 2, 3]).unwrap();
    assert_eq!(r.bytes(), &[0x02, 0x2D, 1, 2, 1, 2, 3, 0]);
    assert_eq!(r.kind(), ReportKind::Output);
    // the base has a single edge LED: the index is forced to 0
    let r = led_mode(ModuleKind::Base, LedTarget::Index(7), LedMode::Static).unwrap();
    assert_eq!(r.bytes(), &[0x02, 0x2C, 0, 0, 1, 0]);
    let r = led_brightness(ModuleKind::Knobs, LedTarget::All, 42).unwrap();
    assert_eq!(r.bytes(), &[0x02, 0x2E, 2, 0xFF, 42]);
    // brightness clamps at 100
    let r = led_brightness(ModuleKind::Base, LedTarget::All, 200).unwrap();
    assert_eq!(r.bytes(), &[0x02, 0x2E, 0, 0, 100]);
    // rollers have no LEDs; keys/dial LED speed does not exist
    assert!(led_color(ModuleKind::Rollers, LedTarget::All, [0; 3]).is_none());
    let r = led_speed(ModuleKind::Base, 30).unwrap();
    assert_eq!(r.bytes(), &[0x02, 0x30, 30]);
    assert!(led_speed(ModuleKind::Keys, 30).is_none());
}

#[test]
fn screen_fill_bytes() {
    assert_eq!(
        fill_key(3, [9, 8, 7]).unwrap().bytes(),
        &[0x03, 0x37, 3, 9, 8, 7]
    );
    assert!(fill_key(15, [0; 3]).is_none());
    assert_eq!(
        fill_screen(ModuleKind::Keys, [1, 2, 3]).unwrap().bytes(),
        &[0x03, 0x36, 1, 2, 3]
    );
    assert_eq!(
        fill_screen(ModuleKind::Dial, [4, 5, 6]).unwrap().bytes(),
        &[0x03, 0x46, 4, 5, 6]
    );
    assert!(fill_screen(ModuleKind::Base, [0; 3]).is_none());
}

#[test]
fn image_chunks_match_the_wire_format() {
    let jpeg: Vec<u8> = (0..700u32).map(|i| i as u8).collect();
    let chunks = key_image(4, &jpeg, 320);
    assert_eq!(chunks.len(), 3);
    assert_eq!(
        &chunks[0].bytes()[..8],
        &[0x02, 0x36, 0, 0, 0x38, 0x01, 4, 0]
    );
    assert_eq!(chunks[0].bytes().len(), 320);
    assert_eq!(&chunks[2].bytes()[..8], &[0x02, 0x36, 2, 0, 76, 0, 4, 1]);
    let joined: Vec<u8> = chunks
        .iter()
        .flat_map(|c| c.bytes()[8..].to_vec())
        .collect();
    assert_eq!(joined, jpeg);
    assert!(chunks
        .iter()
        .all(|c| c.kind() == ReportKind::Output && !c.is_forbidden()));

    let panel = panel_image(&[1, 2, 3], 320);
    assert_eq!(&panel[0].bytes()[..8], &[0x02, 0x35, 0, 0, 3, 0, 0, 1]);
    let dial = dial_image(&[0xFF, 0xD8, 0xFF, 0xD9], 320);
    assert_eq!(dial.len(), 1);
    assert_eq!(&dial[0].bytes()[..8], &[0x02, 0x45, 0, 0, 4, 0, 0, 1]);
    assert!(key_image(15, &jpeg, 320).is_empty());
}

#[test]
fn query_ids_and_diagnostic_reads() {
    let (_hs, keys) = enter_software_mode(ModuleKind::Keys).unwrap();
    assert_eq!(query_id(keys, Query::Firmware), Some(0x38));
    assert_eq!(query_id(keys, Query::SerialNumber), Some(0x3A));
    assert_eq!(query_id(keys, Query::FaderValues), None);
    let reads = diagnostic_reads(keys);
    assert_eq!(
        reads.iter().map(|r| r.1).collect::<Vec<_>>(),
        [0x38, 0x3B, 0x3C]
    );
    // a module check never reads the serial number
    assert!(reads
        .iter()
        .all(|(_, id)| Some(*id) != query_id(keys, Query::SerialNumber)));

    let faders = SoftwareMode::m0(ModuleKind::Faders).unwrap();
    assert_eq!(query_id(faders, Query::FaderValues), Some(0x33));
    assert_eq!(query_id(faders, Query::Backlight), None);
}

#[test]
fn module_facts_and_constants() {
    assert_eq!(VENDOR_ID, 0x2516);
    assert_eq!(FEATURE_LEN, 0x20);
    assert_eq!(CHUNK_HEADER_LEN, 8);
    for kind in ModuleKind::ALL {
        assert_eq!(ModuleKind::from_product_id(kind.product_id()), Some(kind));
        assert_eq!(ModuleKind::from_name(kind.name()), Some(kind));
    }
    assert_eq!(ModuleKind::Keys.screen_size(), Some((480, 272)));
    assert_eq!(ModuleKind::Dial.screen_size(), Some((240, 240)));
    assert!(ModuleKind::Keys.needs_software_mode());
    assert!(!ModuleKind::Base.needs_software_mode());
}

fn raw_input(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut r = vec![0x01, tag, payload.len() as u8, 0];
    r.extend_from_slice(payload);
    r.resize(64, 0);
    r
}

#[test]
fn input_decoding_and_answer_parsing_still_work() {
    let mut p = vec![0x02, 0x0f];
    p.extend_from_slice(&[0u8; 15]);
    p[2 + 9] = 1;
    let events = parse_input(ModuleKind::Keys, &raw_input(0x34, &p));
    match &events[0] {
        Input::Keys(k) => assert!(k[9] && !k[0]),
        other => panic!("unexpected event {other:?}"),
    }
    assert_eq!(
        parse_input(ModuleKind::Dial, &raw_input(0x44, &[0x03, 0x01, 0x00])),
        vec![Input::Dial { delta: 1 }]
    );

    // answers captured on hardware (upstream report.rs tests)
    let fw = [
        0x38, 0x0f, 0x29, 0x63, 0x1c, 0x5d, b'0', b'2', b'.', b'0', b'0', b'.', b'0', b'1', b'.',
        b'0', b'0', 0, 0,
    ];
    assert_eq!(parse_firmware(&fw).as_deref(), Some("02.00.01.00"));
    let keys_info = [
        0x3b, 0x0b, 0x14, 0x03, 0x05, 0x0f, 0xe0, 0x01, 0x10, 0x01, 0x01, 0x48, 0x48,
    ];
    assert_eq!(parse_screen_size(&keys_info), Some((480, 272)));
    assert_eq!(parse_percent(&[0x4c, 0x01, 64]), Some(64));
    assert_eq!(
        feature_payload(&[0x33, 0x05, 30, 42, 41, 49, 84]),
        &[30, 42, 41, 49, 84]
    );
    assert_eq!(
        parse_fader_values(&[0x33, 0x05, 30, 42, 41, 49, 84]),
        Some([30, 42, 41, 49, 84])
    );
}
