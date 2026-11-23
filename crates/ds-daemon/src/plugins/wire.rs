//! The plugin wire protocol (ticket DS-050; contract `docs/contracts/plugins.md` §1).
//!
//! Newline-JSON over the plugin's stdin/stdout; stderr goes to the daemon log and the
//! per-plugin ring buffer — **never to the control plane**.
//!
//! ```text
//! host→plugin   {"op":"hello","protocol":1,"plugin_token":"…","caps":[…],"roots":{"fs":[…]},"limits":{…}}
//! plugin→host   {"op":"register","id":"demo","manifest":{…}}
//! host→plugin   {"op":"event","event":"keyDown","topic":"deck/control/keys.0/down","payload":{…}}
//! host→plugin   {"op":"invoke","id":"demo.bump","args":{…},"request_id":7}
//! plugin→host   {"op":"effects","request_id":7,"effects":[{"effect":"set","path":"…","value":…}]}
//! plugin→host   {"op":"request","request_id":8,"kind":"get","path":"/controls/keys.0"}
//! plugin→host   {"op":"request","request_id":9,"kind":"net","method":"GET","url":"http://lights.lan/api/…"}
//! host→plugin   {"op":"reply","request_id":9,"ok":true,"status":200,"body":"…"}
//! plugin→host   {"op":"log","message":"…"}
//! ```
//!
//! This module owns the pure helpers: message construction/parsing, the anchored
//! one-label-per-`*` net glob, path-prefix checks, and the topic→event-name mapping
//! used for event fan-out.

use serde_json::{json, Value};

/// The wire protocol version sent in `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Build the `hello` line's object (sent right after spawn, before anything else).
pub fn hello_message(
    plugin_token: &str,
    caps: &[String],
    fs_roots: &[String],
    limits: &super::manifest::Resources,
) -> Value {
    json!({
        "op": "hello",
        "protocol": PROTOCOL_VERSION,
        "plugin_token": plugin_token,
        "caps": caps,
        "roots": {"fs": fs_roots},
        "limits": {
            "cpu_s": limits.cpu_s,
            "nproc": limits.nproc,
            "nofile": limits.nofile,
            "as_mb": limits.as_mb,
        },
    })
}

/// Build an `invoke` line: `{"op":"invoke","id":"<plugin>.<action>","args":…,"request_id":N}`.
pub fn invoke_message(request_id: u64, plugin_id: &str, action: &str, args: &Value) -> Value {
    json!({
        "op": "invoke",
        "id": format!("{plugin_id}.{action}"),
        "args": args,
        "request_id": request_id,
    })
}

/// Build an `event` line delivered to a subscribed plugin.
pub fn event_message(event: &str, topic: &str, payload: &Value) -> Value {
    json!({
        "op": "event",
        "event": event,
        "topic": topic,
        "payload": payload,
    })
}

/// Build a `reply` line (host answering a plugin `request`).
pub fn reply_message(request_id: u64, ok: bool, extra: Value) -> Value {
    let mut v = json!({"op": "reply", "request_id": request_id, "ok": ok});
    if let (Value::Object(m), Value::Object(extra)) = (&mut v, extra) {
        for (k, val) in extra {
            m.insert(k, val);
        }
    }
    v
}

/// Parse one stdout line into a JSON object with a string `op`. `None` = malformed
/// (a protocol violation: counted + logged by the caller, never sent to clients).
pub fn parse_line(line: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("op").and_then(Value::as_str).is_some() && v.is_object() {
        Some(v)
    } else {
        None
    }
}

/// Serialize one message to a single NDJSON line (no embedded newline possible:
/// serde_json escapes control characters).
pub fn to_line(msg: &Value) -> String {
    let mut s = serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// Anchored DNS-glob match (M5): `*` matches exactly one DNS label; the pattern and
/// the host must have the same number of labels; matching is case-insensitive.
/// `lights.lan` matches only `lights.lan`; `*.lan` matches `lights.lan` but not `lan`,
/// `a.b.lan` or `lights.lan.evil.com`.
pub fn glob_matches_host(pattern: &str, host: &str) -> bool {
    let pat: Vec<&str> = pattern.split('.').collect();
    let hst: Vec<&str> = host.split('.').collect();
    if pat.len() != hst.len() || hst.iter().any(|l| l.is_empty()) {
        return false;
    }
    pat.iter()
        .zip(hst.iter())
        .all(|(p, h)| *p == "*" || p.eq_ignore_ascii_case(h))
}

/// Is `path` equal to or beneath `root` (both absolute, `/`-segment-wise, after
/// lexical normalization of `.` and duplicate slashes — no symlink resolution, the
/// Landlock layer is what enforces the real filesystem view)?
pub fn path_under_root(path: &str, root: &str) -> bool {
    fn norm(p: &str) -> Vec<&str> {
        p.split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect()
    }
    let p = norm(path);
    let r = norm(root);
    if r.is_empty() {
        // root "/" covers everything
        return path.starts_with('/');
    }
    p.len() >= r.len() && p[..r.len()] == r[..]
}

/// Map a bus topic to the plugin event name of `schemas/plugin-manifest.json`
/// (`keyDown`/`keyUp`/`dialRotate`/`slide`/`focus`/…). `None` = not a plugin event.
pub fn topic_to_event(topic: &str) -> Option<&'static str> {
    if let Some(rest) = topic.strip_prefix("deck/control/keys.") {
        if let Some(idx) = rest.find('/') {
            match &rest[idx + 1..] {
                "down" => return Some("keyDown"),
                "up" => return Some("keyUp"),
                _ => {}
            }
        }
        return None;
    }
    if topic.ends_with("/rotate") {
        return Some("dialRotate");
    }
    if topic.ends_with("/slide") {
        return Some("slide");
    }
    if topic == "app/focus/changed" {
        return Some("focus");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_is_anchored_one_label_per_star() {
        assert!(glob_matches_host("lights.lan", "lights.lan"));
        assert!(glob_matches_host("*.lan", "lights.lan"));
        assert!(!glob_matches_host("*.lan", "lan"));
        assert!(!glob_matches_host("*.lan", "a.b.lan"));
        assert!(!glob_matches_host("*.lan", "lights.lan.evil.com"));
        assert!(!glob_matches_host("lights.lan", "lights.lan.evil.com"));
        assert!(glob_matches_host("*", "lights"));
        assert!(!glob_matches_host("*", "lights.lan"));
        assert!(glob_matches_host("LIGHTS.lan", "lights.LAN"));
        assert!(!glob_matches_host("lights.lan", "lights.lan."));
        assert!(glob_matches_host("a.*.c", "a.b.c"));
        assert!(!glob_matches_host("a.*.c", "a.b.d.c"));
    }

    #[test]
    fn path_under_root_is_segment_wise() {
        assert!(path_under_root("/home/u/media/x.png", "/home/u/media"));
        assert!(path_under_root("/home/u/media", "/home/u/media"));
        assert!(!path_under_root("/home/u/mediax", "/home/u/media"));
        assert!(!path_under_root("/home/u", "/home/u/media"));
        assert!(path_under_root("/anything", "/"));
        assert!(!path_under_root("relative", "/"));
        assert!(path_under_root("/a/./b//c", "/a/b"));
    }

    #[test]
    fn topic_to_event_mapping() {
        assert_eq!(topic_to_event("deck/control/keys.0/down"), Some("keyDown"));
        assert_eq!(topic_to_event("deck/control/keys.14/up"), Some("keyUp"));
        assert_eq!(
            topic_to_event("deck/control/dial/rotate"),
            Some("dialRotate")
        );
        assert_eq!(
            topic_to_event("deck/control/knobs.2/rotate"),
            Some("dialRotate")
        );
        assert_eq!(topic_to_event("deck/control/faders.1/slide"), Some("slide"));
        assert_eq!(topic_to_event("app/focus/changed"), Some("focus"));
        assert_eq!(topic_to_event("deck/control/keys.0/long"), None);
        assert_eq!(topic_to_event("daemon/start"), None);
        assert_eq!(topic_to_event("app/demo/x"), None);
    }

    #[test]
    fn lines_round_trip_and_malformed_lines_are_none() {
        let msg = hello_message(
            "tok",
            &["state.write".to_string()],
            &["/media".to_string()],
            &super::super::manifest::Resources::default(),
        );
        let line = to_line(&msg);
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
        let back = parse_line(line.trim_end()).unwrap();
        assert_eq!(back["op"], json!("hello"));
        assert_eq!(back["protocol"], json!(1));
        assert_eq!(back["limits"]["cpu_s"], json!(10));
        assert_eq!(parse_line("not json"), None);
        assert_eq!(parse_line("{\"no\":\"op\"}"), None);
        assert_eq!(parse_line("[1,2]"), None);
        let inv = invoke_message(7, "demo", "bump", &json!({"by": 1}));
        assert_eq!(inv["id"], json!("demo.bump"));
        assert_eq!(inv["request_id"], json!(7));
    }
}
