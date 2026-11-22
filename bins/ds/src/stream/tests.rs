//! Unit tests for the `ds stream` producer surface: target vocabulary (S6), rate/frame
//! limits, source loading (file/dir order, format detection) and meta shaping.

use super::{parse_target, source};
use ds_ipc::frames::limits::{MAX_ANIMATION_FRAMES, MAX_PAYLOAD_BYTES};
use std::path::PathBuf;

fn tiny_png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(&13u32.to_be_bytes());
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0]);
    v.extend_from_slice(&[0u8; 8]); // fake CRC + padding
    v
}

fn tiny_jpeg() -> Vec<u8> {
    let mut v = b"\xFF\xD8\xFF\xE0".to_vec();
    v.extend_from_slice(&[0u8; 16]);
    v.extend_from_slice(b"\xFF\xD9");
    v
}

#[test]
fn target_vocabulary_is_exactly_s6() {
    for ok in ["keys", "dial", "key:0", "key:1", "key:9", "key:14"] {
        assert_eq!(parse_target(ok), Ok(ok.to_string()), "{ok}");
    }
    for bad in [
        "panel", "screen", "PANEL", "Keys", "key:15", "key:99", "key:-1", "key:", "key:01x",
        "key:014", "keys ", " key:0", "key:0x", "", "knobs", "knob:0",
    ] {
        let e = parse_target(bad).unwrap_err();
        assert!(e.contains("invalid stream target"), "{bad}: {e}");
        assert!(e.contains("keys | key:0-14 | dial"), "{bad}: {e}");
    }
    // key:07 is a valid index 7 but not the canonical spelling — refused (schema pattern
    // allows only [0-9]|1[0-4], so "07" is not representable on the wire)
    assert!(parse_target("key:07").is_err());
}

#[test]
fn dir_plan_is_sorted_capped_and_paced() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("frames");
    std::fs::create_dir_all(&d).unwrap();
    // write out of order: filename order must win
    for (name, w) in [("0003.png", 3u32), ("0001.png", 1), ("0002.png", 2)] {
        std::fs::write(d.join(name), tiny_png(w, 72)).unwrap();
    }
    let plan = source::plan(None, Some(d.clone()), Some(30), false).unwrap();
    assert_eq!(plan.frames.len(), 3);
    // filename order wins: the frames were written 0003, 0001, 0002 with widths 3, 1, 2
    let sizes: Vec<u32> = plan.frames.iter().map(|f| f.size.unwrap().0).collect();
    assert_eq!(sizes, [1, 2, 3]);
    // each frame carries its own PNG size from the IHDR
    assert_eq!(plan.frames[0].size, Some((1, 72)));
    assert_eq!(plan.frames[2].size, Some((3, 72)));
    assert_eq!(plan.fps, Some(30));
    assert!(!plan.loop_);
    assert!(plan.frame_interval() >= std::time::Duration::from_millis(20));
    assert!(plan.frame_interval() <= std::time::Duration::from_millis(34));
    // meta: animation mode carries frames+fps, target filled by the verb
    let mut m = plan.meta(5, &plan.frames[0]);
    m.target = "keys".into();
    let v = m.to_value().unwrap();
    assert_eq!(v["frames"], serde_json::json!(3));
    assert_eq!(v["fps"], serde_json::json!(30.0));
    assert_eq!(v["seq"], serde_json::json!(5));
    assert_eq!(v["target"], serde_json::json!("keys"));
    assert_eq!(
        v["max_payload_bytes"],
        serde_json::json!(MAX_PAYLOAD_BYTES as u32)
    );
    assert!(v.get("loop").is_none());
    // loop flag rides in the meta
    let plan = source::plan(None, Some(d.clone()), Some(12), true).unwrap();
    let mut m = plan.meta(0, &plan.frames[0]);
    m.target = "dial".into();
    assert_eq!(m.to_value().unwrap()["loop"], serde_json::json!(true));
}

#[test]
fn single_file_plan_has_no_fps_and_one_frame() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("frame.png");
    std::fs::write(&f, tiny_png(480, 272)).unwrap();
    let plan = source::plan(Some(f.clone()), None, None, false).unwrap();
    assert_eq!(plan.frames.len(), 1);
    assert_eq!(plan.frames[0].size, Some((480, 272)));
    assert!(plan.fps.is_none());
    assert_eq!(plan.frame_interval(), std::time::Duration::ZERO);
    let mut m = plan.meta(0, &plan.frames[0]);
    m.target = "keys".into();
    let v = m.to_value().unwrap();
    assert!(v.get("frames").is_none(), "single frame: no frames count");
    assert!(v.get("fps").is_none());
    assert_eq!(v["format"], serde_json::json!("png"));
}

#[test]
fn plan_rejects_bad_sources_and_limits() {
    let tmp = tempfile::tempdir().unwrap();
    // no source
    assert!(source::plan(None, None, None, false)
        .unwrap_err()
        .message()
        .contains("needs a source"));
    // both
    let f = tmp.path().join("f.png");
    std::fs::write(&f, tiny_png(72, 72)).unwrap();
    assert!(
        source::plan(Some(f.clone()), Some(tmp.path().join("d")), Some(30), false)
            .unwrap_err()
            .message()
            .contains("not both")
    );
    // --dir without --fps
    let d = tmp.path().join("d");
    std::fs::create_dir_all(&d).unwrap();
    assert!(source::plan(None, Some(d.clone()), None, false)
        .unwrap_err()
        .message()
        .contains("--fps"));
    // empty dir
    assert!(source::plan(None, Some(d.clone()), Some(30), false)
        .unwrap_err()
        .message()
        .contains("no files"));
    // fps out of contract range
    for bad in [0u32, 31, 100] {
        assert!(source::plan(None, Some(d.clone()), Some(bad), false)
            .unwrap_err()
            .message()
            .contains("1..=30"));
    }
    // non-image file in a dir
    std::fs::write(d.join("0001.bin"), b"not an image").unwrap();
    let e = source::plan(None, Some(d.clone()), Some(30), false)
        .unwrap_err()
        .message();
    assert!(e.contains("not a PNG/JPEG/GIF"), "{e}");
    assert!(e.contains("0001.bin"), "{e}");
    // unreadable file
    assert!(
        source::plan(Some(tmp.path().join("missing.png")), None, None, false)
            .unwrap_err()
            .message()
            .contains("cannot read")
    );
    // > 300 frames in a dir (write 301 tiny files — cheap)
    let big = tmp.path().join("big");
    std::fs::create_dir_all(&big).unwrap();
    for i in 0..=MAX_ANIMATION_FRAMES {
        std::fs::write(big.join(format!("{i:04}.jpg")), tiny_jpeg()).unwrap();
    }
    let e = source::plan(None, Some(big), Some(30), false)
        .unwrap_err()
        .message();
    assert!(e.contains("300 frames"), "{e}");
    // exactly 300 is allowed
    let ok = tmp.path().join("ok300");
    std::fs::create_dir_all(&ok).unwrap();
    for i in 0..300u32 {
        std::fs::write(ok.join(format!("{i:04}.jpg")), tiny_jpeg()).unwrap();
    }
    let plan = source::plan(None, Some(ok), Some(30), false).unwrap();
    assert_eq!(plan.frames.len(), 300);
}

#[test]
fn non_image_file_triggers_ffmpeg_path_with_clear_error_when_missing() {
    // a video-looking file (mp4 magic) with ffmpeg absent from PATH → clear usage error
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("v.mp4");
    std::fs::write(&f, b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom").unwrap();
    let old = std::env::var_os("PATH");
    std::env::set_var("PATH", "/nonexistent-ds-test-path");
    let r = source::plan(Some(f), None, Some(24), false);
    match old {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    let e = r.unwrap_err().message();
    assert!(e.contains("ffmpeg"), "{e}");
    assert!(e.contains("not found on PATH"), "{e}");
    assert!(e.contains("--dir"), "{e}");
}

#[test]
fn ffmpeg_argv_construction_is_argv_only_and_stable() {
    let argv = source::ffmpeg_argv(
        std::path::Path::new("/tmp/in.mp4"),
        std::path::Path::new("/tmp/out/frames/%04d.png"),
    );
    assert_eq!(argv[0], "ffmpeg");
    assert_eq!(argv[1], "-y");
    assert_eq!(argv[2], "-i");
    assert_eq!(argv[3], "/tmp/in.mp4");
    assert_eq!(argv[4], "/tmp/out/frames/%04d.png");
    assert_eq!(argv.len(), 5, "no extra shell-ish tokens");
    // a filename with spaces stays one argv entry (no shell splitting)
    let argv = source::ffmpeg_argv(
        std::path::Path::new("/tmp/my video.mp4"),
        std::path::Path::new("/tmp/o/%04d.png"),
    );
    assert_eq!(argv[3], "/tmp/my video.mp4");
    assert_eq!(argv.len(), 5);
}

#[test]
fn stream_sock_path_is_the_control_socket_sibling() {
    let g = crate::cli::Global {
        socket: Some(PathBuf::from("/run/x/daemon.sock")),
        ..Default::default()
    };
    assert_eq!(
        super::stream_sock_path(&g),
        PathBuf::from("/run/x/stream.sock")
    );
}
