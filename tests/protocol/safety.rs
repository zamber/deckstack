//! Safety tests for `ds-protocol`: forbidden HID commands must stay unrepresentable.
//!
//! - No builder can emit a forbidden report (exhaustive sweep over every builder, module kind,
//!   target and parameter) — this fails the moment anyone adds a builder for a forbidden command.
//! - The raw path (`is_forbidden`) rejects exactly the pinned forbidden (report id, command) set
//!   of the pinned upstream rev; the test fails if that list ever drifts.
//! - `Report` has no raw constructor (compile-fail doc-tests on the type in `src/lib.rs`).
//! - backlight-0 is refused on every module; LED writes never persist; reads need the
//!   software-mode token.

use ds_protocol::*;

/// The forbidden (report id, command) pairs of the pinned upstream rev
/// (mh-protocol `src/safety.rs` @ `26364e2fdcf86df6c459ea0e940f1dd5ca84cde7`). Mirrored on
/// purpose: the rev is pinned, so any drift here is a deliberate upstream change to review,
/// not something to absorb silently.
const FORBIDDEN: &[(u8, u8)] = &[
    // M0 family (base, faders, knobs, rollers), output reports
    (0x02, 0x2A), // update firmware
    (0x02, 0x2B), // set serial number
    (0x02, 0x31), // save factory data
    (0x02, 0x49), // test firmware recovery
    (0x02, 0x4A), // test factory reset
    (0x02, 0x2F), // recalibrate the base's tilt sensor
    // Keys (M4315)
    (0x02, 0x34), // update firmware
    (0x03, 0x35), // set serial number
    (0x03, 0x39), // test firmware recovery
    (0x03, 0x3C), // save factory data
    (0x03, 0x3D), // test factory reset
    (0x03, 0x3E), // test animation recovery
    (0x02, 0x38), // save screen picture to flash
    // Dial (M131E)
    (0x02, 0x44), // update firmware
    (0x03, 0x45), // set serial number
    (0x03, 0x48), // test firmware recovery
    (0x03, 0x4B), // save factory data
    (0x03, 0x4C), // test factory reset
    (0x03, 0x4D), // test animation recovery
    (0x02, 0x47), // save screen picture to flash
];

/// Every report any safe builder can produce, across all module kinds and parameters.
fn all_builder_reports() -> Vec<Report> {
    let mut all = Vec::new();
    for kind in ModuleKind::ALL {
        if let Some((r, _mode)) = enter_software_mode(kind) {
            all.push(r);
        }
        if let Some(r) = leave_software_mode(kind) {
            all.push(r);
        }
        for pct in [1u8, 50, 100, 255] {
            if let Some(r) = backlight(kind, pct) {
                all.push(r);
            }
        }
        if let Some(r) = fill_screen(kind, [1, 2, 3]) {
            all.push(r);
        }
        if let Some(r) = led_speed(kind, 50) {
            all.push(r);
        }
        for target in [
            LedTarget::All,
            LedTarget::Index(0),
            LedTarget::Index(2),
            LedTarget::Index(7),
        ] {
            if let Some(r) = led_color(kind, target, [1, 2, 3]) {
                all.push(r);
            }
            for mode in [LedMode::Off, LedMode::Static, LedMode::Rainbow] {
                if let Some(r) = led_mode(kind, target, mode) {
                    all.push(r);
                }
            }
            if let Some(r) = led_brightness(kind, target, 200) {
                all.push(r);
            }
        }
    }
    for index in [0u8, 3, 14, 15, 200] {
        if let Some(r) = fill_key(index, [9, 8, 7]) {
            all.push(r);
        }
        all.extend(key_image(index, &[0xAB; 700], 320));
    }
    all.extend(panel_image(&[0xCD; 700], 320));
    all.extend(dial_image(&[0xEF; 4], 320));
    all
}

#[test]
fn raw_path_rejects_exactly_the_pinned_forbidden_set() {
    let mut found = Vec::new();
    for id in 0..=u8::MAX {
        for command in 0..=u8::MAX {
            if is_forbidden(&[id, command, 0x00, 0x00]) {
                found.push((id, command));
            }
        }
    }
    let mut expected = FORBIDDEN.to_vec();
    found.sort_unstable();
    expected.sort_unstable();
    assert_eq!(
        found, expected,
        "forbidden set drifted from the pinned upstream list"
    );
    // sanity: the list is not empty and covers the headline dangers
    for pair in [
        (0x02, 0x2A),
        (0x02, 0x2B),
        (0x03, 0x35),
        (0x02, 0x4A),
        (0x03, 0x4C),
    ] {
        assert!(FORBIDDEN.contains(&pair), "missing {pair:02x?}");
    }
    // short or safe raw buffers are simply not forbidden (nothing to match)
    assert!(!is_forbidden(&[]));
    assert!(!is_forbidden(&[0x02]));
    assert!(!is_forbidden(&[0x02, 0x36, 1, 2, 3]));
}

#[test]
fn no_builder_can_emit_a_forbidden_report() {
    let all = all_builder_reports();
    assert!(
        all.len() > 50,
        "sweep must cover many builders, got {}",
        all.len()
    );
    for r in &all {
        assert!(
            !r.is_forbidden(),
            "builder produced a forbidden report: {:02x?}",
            &r.bytes()[..2]
        );
        assert!(!is_forbidden(r.bytes()));
    }
}

#[test]
fn backlight_zero_is_refused_on_every_module() {
    for kind in ModuleKind::ALL {
        assert!(
            backlight(kind, 0).is_none(),
            "{kind:?} accepted backlight 0"
        );
    }
    // real values still work and clamp at 100
    let r = backlight(ModuleKind::Dial, 100).unwrap();
    assert_eq!(r.bytes(), &[0x03, 0x44, 100]);
    assert_eq!(r.kind(), ReportKind::Feature);
    let r = backlight(ModuleKind::Keys, 255).unwrap();
    assert_eq!(r.bytes(), &[0x03, 0x34, 100]);
    // modules without a screen have no backlight at all
    assert!(backlight(ModuleKind::Base, 50).is_none());
}

#[test]
fn led_writes_never_persist() {
    for kind in ModuleKind::ALL {
        for target in [LedTarget::All, LedTarget::Index(3)] {
            if let Some(r) = led_color(kind, target, [1, 2, 3]) {
                assert_eq!(
                    *r.bytes().last().unwrap(),
                    0,
                    "{kind:?} {target:?} persisted a color"
                );
            }
            if let Some(r) = led_mode(kind, target, LedMode::Static) {
                assert_eq!(
                    *r.bytes().last().unwrap(),
                    0,
                    "{kind:?} {target:?} persisted a mode"
                );
            }
        }
    }
}

#[test]
fn reads_require_the_software_mode_token() {
    // keys/dial cannot mint the token without the handshake report
    assert!(SoftwareMode::m0(ModuleKind::Keys).is_none());
    assert!(SoftwareMode::m0(ModuleKind::Dial).is_none());
    let (handshake, mode) = enter_software_mode(ModuleKind::Keys).unwrap();
    assert_eq!(handshake.bytes(), &[0x03, 0x3A, 0x01]);
    assert_eq!(handshake.kind(), ReportKind::Feature);
    assert_eq!(mode.kind(), ModuleKind::Keys);
    assert_eq!(query_id(mode, Query::Backlight), Some(0x3C));
    assert_eq!(query_id(mode, Query::DeviceInfo), Some(0x3B));
    assert!(!diagnostic_reads(mode).is_empty());
    // handback to firmware
    assert_eq!(
        leave_software_mode(ModuleKind::Dial).unwrap().bytes(),
        &[0x03, 0x49, 0x00]
    );
    // the M0 family mints the token without any report and reads at any time
    let base = SoftwareMode::m0(ModuleKind::Base).unwrap();
    assert_eq!(query_id(base, Query::Firmware), Some(0x2E));
    assert_eq!(query_id(base, Query::Orientation), Some(0x35));
    // ...and has no handshake report of its own
    assert!(enter_software_mode(ModuleKind::Faders).is_none());
    assert!(leave_software_mode(ModuleKind::Base).is_none());
}

#[test]
fn wire_version_is_pinned_to_one() {
    assert_eq!(PROTOCOL_VERSION, 1);
}
