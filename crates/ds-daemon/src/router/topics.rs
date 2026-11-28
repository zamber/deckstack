//! Topic vocabulary, syntax validation and wildcard matching for the event bus
//! (`docs/contracts/actions.md` §1, `docs/components/event-router.md`).
//!
//! ## Topic grammar (actions.md §1)
//!
//! ```text
//! deck/control/<control>/<gesture>   control: keys.0 | dial | knobs.0 | faders.2 | rollers.1
//!                                     gesture: down | up | long | double | rotate | slide | value
//! deck/device/<serial>/<event>        connected | disconnected | orientation | faders
//! deck/system/<event>                 start | stop | rev | binding-error | exec | crash-recovered
//! daemon/<event>                      daemon lifecycle (start | stop | …) — the `daemon/*`
//!                                     namespace of the router component doc
//! app/<sender>/<topic…>               any external program (`ds emit app/focus/changed …`)
//! plugin/<id>/<event>                 reserved for the plugin host (phase 5, DS-050+)
//! ```
//!
//! Events are `{topic, ts_ms, payload, rev}` — `rev` is present only on state events
//! (a committed revision); device/app/daemon events carry `rev: null`.
//!
//! ## Wildcard rules (subscription / binding lookup)
//!
//! In topics **both `/` and `.` are segment separators** for matching, so a control
//! address like `keys.0` is the two segments `keys` + `0`. A pattern matches a topic when
//! the pattern is a *segment-wise prefix* of the topic and every non-`*` pattern segment
//! equals the corresponding topic segment; `*` matches **exactly one** segment. Hence:
//!
//! - `deck/control/keys.0/down` matches only itself (exact);
//! - `deck/control/keys/*` matches `deck/control/keys.0/down` (`*` eats the `0`; the
//!   pattern may be shorter than the topic, exactly like state-path patterns);
//! - `deck/control/*` matches every control event; `deck/*` every `deck/...` topic;
//! - `*` alone matches every topic.
//!
//! The same rules apply to state-path subscription patterns (`/controls/keys/*`), which
//! keep their existing `ds-state` semantics (`/`-separated only, prefix match, `*` = one
//! segment) — see [`super::Pattern`].

/// Topic roots the router knows. `plugin` is reserved (phase 5); `daemon` is the
/// daemon-lifecycle namespace; `deck`/`app` are the externally emittable roots (the IPC
/// schema restricts the `emit` op to `^(deck|app)/`).
pub const TOPIC_ROOTS: [&str; 4] = ["deck", "app", "daemon", "plugin"];

/// Gestures of `deck/control/<control>/<gesture>` (actions.md §1 + `value`).
pub const GESTURES: [&str; 7] = ["down", "up", "long", "double", "rotate", "slide", "value"];

/// Events of `deck/device/<serial>/<event>` (`moved` = DS-112 placement change: the module's
/// (slot, orientation) identity key changed, e.g. a reseat in another connector).
pub const DEVICE_EVENTS: [&str; 5] = [
    "connected",
    "disconnected",
    "orientation",
    "faders",
    "moved",
];

/// Events of `deck/system/<event>` (actions.md §1).
pub const SYSTEM_EVENTS: [&str; 6] = [
    "start",
    "stop",
    "rev",
    "binding-error",
    "exec",
    "crash-recovered",
];

/// Events of `daemon/<event>` (the router's own lifecycle namespace).
pub const DAEMON_EVENTS: [&str; 3] = ["start", "stop", "reload"];

/// Maximum length of a whole topic string.
pub const MAX_TOPIC_LEN: usize = 200;

/// Maximum number of segments (`.`- and `/`-separated) in a topic or pattern.
pub const MAX_SEGMENTS: usize = 16;

/// Whether `seg` is a topic root (`deck`, `app`, `daemon`, `plugin`).
pub fn is_topic_root(seg: &str) -> bool {
    TOPIC_ROOTS.contains(&seg)
}

/// Characters allowed in a raw topic/pattern segment (the schema's topic pattern allows
/// the same set plus `.` after the root; `.` also separates matching segments).
fn valid_segment_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '*' | '.')
}

/// Split a topic (or pattern) into its matching segments: `/` and `.` both separate.
pub fn segments(s: &str) -> Vec<String> {
    s.split(['/', '.'])
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// A subscription pattern over topics: segment-wise prefix match, `*` = one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicPattern {
    raw: String,
    segs: Vec<String>,
}

impl TopicPattern {
    /// Parse and syntax-check a topic pattern (no leading `/`; wildcards allowed in any
    /// segment). This is *syntax only* — the full grammar is checked by [`validate_topic`]
    /// for published events.
    pub fn parse(raw: &str) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("topic pattern is empty".to_string());
        }
        if raw.len() > MAX_TOPIC_LEN {
            return Err(format!(
                "topic pattern longer than {MAX_TOPIC_LEN} chars: {raw:?}"
            ));
        }
        let mut segs = Vec::new();
        for seg in raw.split('/') {
            if seg.is_empty() {
                return Err(format!("empty topic segment in {raw:?}"));
            }
            if !seg.chars().all(valid_segment_char) {
                return Err(format!("illegal character in topic segment {seg:?}"));
            }
            // `.` also separates segments for matching.
            for part in seg.split('.') {
                if part.is_empty() {
                    return Err(format!("empty topic sub-segment in {raw:?}"));
                }
                if !part.chars().all(valid_segment_char) {
                    return Err(format!("illegal character in topic segment {part:?}"));
                }
                segs.push(part.to_string());
            }
        }
        if segs.len() > MAX_SEGMENTS {
            return Err(format!("more than {MAX_SEGMENTS} segments in {raw:?}"));
        }
        Ok(Self {
            raw: raw.to_string(),
            segs,
        })
    }

    /// The original string (no leading `/`).
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether any segment is the `*` wildcard.
    pub fn is_wildcard(&self) -> bool {
        self.segs.iter().any(|s| s == "*")
    }

    /// Segment-wise prefix match with `*` = exactly one segment (see module docs).
    pub fn matches(&self, topic: &str) -> bool {
        let t = segments(topic);
        if self.segs.len() > t.len() {
            return false;
        }
        self.segs
            .iter()
            .zip(t.iter())
            .all(|(p, e)| p == "*" || p == e)
    }
}

/// One parsed `deck/control/<control>/<gesture>` triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlGesture {
    /// The control spelling, e.g. `keys.0`, `dial`, `knobs.2`.
    pub control: String,
    /// The gesture, e.g. `down`.
    pub gesture: String,
}

/// Validate the control part of `deck/control/...`: `keys.N` (0-14), `dial`, `knobs.N`
/// (0-2), `faders.N` (0-4), `rollers.N` (0-1) — actions.md §1, S8 ranges.
pub fn validate_control(control: &str) -> Result<(), String> {
    let (kind, index) = match control.split_once('.') {
        Some((k, i)) => (k, Some(i)),
        None => (control, None),
    };
    let max = match kind {
        "keys" => Some(14),
        "knobs" => Some(2),
        "faders" => Some(4),
        "rollers" => Some(1),
        "dial" => None,
        other => return Err(format!("unknown control kind '{other}' in {control:?}")),
    };
    match (kind, index) {
        ("dial", None) => Ok(()),
        (_, Some(i)) => match (i.parse::<u32>(), max) {
            (Ok(n), Some(m)) if n <= m => Ok(()),
            (Ok(n), Some(m)) => Err(format!(
                "control index {n} out of range 0-{m} in {control:?} (S8)"
            )),
            _ => Err(format!("bad control index in {control:?}")),
        },
        (_, None) => Err(format!("control {control:?} needs an index (e.g. keys.0)")),
    }
}

/// Full grammar validation of a topic to be *published* (emit op, device/system/daemon
/// sources). Wildcards are not allowed in published topics.
pub fn validate_topic(topic: &str) -> Result<(), String> {
    if topic.len() > MAX_TOPIC_LEN {
        return Err(format!(
            "topic longer than {MAX_TOPIC_LEN} chars: {topic:?}"
        ));
    }
    if topic.contains('*') {
        return Err(format!(
            "wildcard '*' is not allowed in a published topic: {topic:?}"
        ));
    }
    let segs = segments(topic);
    if segs.is_empty() || segs.len() > MAX_SEGMENTS {
        return Err(format!("bad topic segment count in {topic:?}"));
    }
    if !segs.iter().all(|s| s.chars().all(valid_segment_char)) {
        return Err(format!("illegal character in topic {topic:?}"));
    }
    match segs[0].as_str() {
        "deck" => match segs.get(1).map(String::as_str) {
            Some("control") => {
                if segs.len() < 4 {
                    return Err(format!("deck/control needs <control>/<gesture>: {topic:?}"));
                }
                // `deck/control/<control>/<gesture>` splits into 5 segments for an indexed
                // control (`keys.0` -> keys + 0 + gesture) and 4 for `dial`.
                validate_control_gesture(&segs)
            }
            Some("device") => {
                if segs.len() != 4 || segs[2].is_empty() {
                    return Err(format!("deck/device needs <serial>/<event>: {topic:?}"));
                }
                if !DEVICE_EVENTS.contains(&segs[3].as_str()) {
                    return Err(format!("unknown deck/device event {:?}", segs[3]));
                }
                Ok(())
            }
            Some("system") => {
                if segs.len() != 3 {
                    return Err(format!("deck/system needs one <event>: {topic:?}"));
                }
                if !SYSTEM_EVENTS.contains(&segs[2].as_str()) {
                    return Err(format!("unknown deck/system event {:?}", segs[2]));
                }
                Ok(())
            }
            other => Err(format!("unknown deck topic '{other:?}' in {topic:?}")),
        },
        "app" => {
            if segs.len() < 3 || segs[1].is_empty() {
                return Err(format!(
                    "app topics need <sender>/<topic…> (at least 3 segments): {topic:?}"
                ));
            }
            Ok(())
        }
        "daemon" => {
            if segs.len() != 2 {
                return Err(format!("daemon topics need one <event>: {topic:?}"));
            }
            if !DAEMON_EVENTS.contains(&segs[1].as_str()) {
                return Err(format!("unknown daemon event {:?}", segs[1]));
            }
            Ok(())
        }
        "plugin" => {
            if segs.len() < 3 {
                return Err(format!("plugin topics need <id>/<event>: {topic:?}"));
            }
            Ok(())
        }
        other => Err(format!("unknown topic root '{other}' in {topic:?}")),
    }
}

/// Validate `deck/control/...` given the already-split segments: `[deck, control, <control
/// segments…>, <gesture>]` where an indexed control contributes two segments
/// (`keys` + `0`) and `dial` one.
fn validate_control_gesture(segs: &[String]) -> Result<(), String> {
    // segs[0] == "deck", segs[1] == "control"; the rest is <control…>/<gesture>.
    let rest = &segs[2..];
    if rest.len() < 2 {
        return Err("deck/control needs <control>/<gesture>".to_string());
    }
    let (gesture, control_parts) = rest.split_last().unwrap();
    if !GESTURES.contains(&gesture.as_str()) {
        return Err(format!("unknown gesture {gesture:?}"));
    }
    let control = control_parts.join(".");
    validate_control(&control)
}

/// Parse `deck/control/<control>/<gesture>` into its control and gesture (the control
/// re-joined with `.`, e.g. `keys.0`). `None` for any other topic.
pub fn parse_control_gesture(topic: &str) -> Option<ControlGesture> {
    let segs = segments(topic);
    if segs.len() < 4 || segs[0] != "deck" || segs[1] != "control" {
        return None;
    }
    let (gesture, control_parts) = segs[2..].split_last()?;
    let control = control_parts.join(".");
    if validate_control(&control).is_err() || !GESTURES.contains(&gesture.as_str()) {
        return None;
    }
    Some(ControlGesture {
        control,
        gesture: gesture.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matching_rules() {
        let keys_star = TopicPattern::parse("deck/control/keys/*").unwrap();
        assert!(keys_star.matches("deck/control/keys.0/down"));
        assert!(keys_star.matches("deck/control/keys.14/up"));
        assert!(!keys_star.matches("deck/control/dial/rotate"));
        assert!(!keys_star.matches("app/focus/changed"));
        let exact = TopicPattern::parse("deck/control/keys.0/down").unwrap();
        assert!(exact.matches("deck/control/keys.0/down"));
        assert!(!exact.matches("deck/control/keys.0/up"));
        assert!(!exact.matches("deck/control/keys.1/down"));
        let deck_star = TopicPattern::parse("deck/*").unwrap();
        assert!(deck_star.matches("deck/control/keys.0/down"));
        assert!(deck_star.matches("deck/system/start"));
        assert!(!deck_star.matches("app/x/y"));
        let star = TopicPattern::parse("*").unwrap();
        assert!(star.matches("deck/control/keys.0/down"));
        assert!(star.matches("app/a/b"));
        // prefix semantics: a shorter pattern matches deeper topics
        let ctrl = TopicPattern::parse("deck/control/keys.0").unwrap();
        assert!(ctrl.matches("deck/control/keys.0/down"));
        // mid-pattern `*` is exactly one segment
        let mid = TopicPattern::parse("app/*/changed").unwrap();
        assert!(mid.matches("app/focus/changed"));
        assert!(!mid.matches("app/focus/other/changed"));
        assert!(mid.is_wildcard());
        assert!(!exact.is_wildcard());
    }

    #[test]
    fn pattern_parse_rejects_bad_syntax() {
        for bad in [
            "",
            "deck//x",
            "deck/x y",
            "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q",
        ] {
            assert!(TopicPattern::parse(bad).is_err(), "{bad:?}");
        }
        assert!(TopicPattern::parse("deck/control/keys/*").is_ok());
    }

    #[test]
    fn validate_topic_grammar() {
        for ok in [
            "deck/control/keys.0/down",
            "deck/control/keys.14/long",
            "deck/control/dial/rotate",
            "deck/control/knobs.2/up",
            "deck/control/faders.4/slide",
            "deck/control/rollers.1/rotate",
            "deck/control/keys.3/value",
            "deck/device/virtual-0102/connected",
            "deck/device/SN123/orientation",
            "deck/system/start",
            "deck/system/binding-error",
            "daemon/start",
            "daemon/stop",
            "app/focus/changed",
            "app/demo/hi",
            "app/cron/nightly",
            "plugin/demo/ready",
        ] {
            assert!(validate_topic(ok).is_ok(), "{ok}: {:?}", validate_topic(ok));
        }
        for bad in [
            "other/x",
            "deck",
            "deck/control",
            "deck/control/keys.0",
            "deck/control/keys.15/down",
            "deck/control/keys/x/down",
            "deck/control/pedal/down",
            "deck/control/keys.0/bogus",
            "deck/control/dial",
            "deck/device/onlyserial",
            "deck/device/s/bogus",
            "deck/system",
            "deck/system/bogus",
            "daemon",
            "daemon/bogus",
            "app",
            "app/onlysender",
            "deck/control/keys.0/*",
            "",
        ] {
            assert!(validate_topic(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn control_gesture_parsing() {
        let cg = parse_control_gesture("deck/control/keys.0/down").unwrap();
        assert_eq!(cg.control, "keys.0");
        assert_eq!(cg.gesture, "down");
        let cg = parse_control_gesture("deck/control/dial/rotate").unwrap();
        assert_eq!(cg.control, "dial");
        assert_eq!(cg.gesture, "rotate");
        assert!(parse_control_gesture("app/focus/changed").is_none());
        assert!(parse_control_gesture("deck/control/keys.99/down").is_none());
    }

    #[test]
    fn validate_control_ranges() {
        assert!(validate_control("keys.0").is_ok());
        assert!(validate_control("keys.14").is_ok());
        assert!(validate_control("keys.15").is_err());
        assert!(validate_control("dial").is_ok());
        assert!(validate_control("knobs.2").is_ok());
        assert!(validate_control("knobs.3").is_err());
        assert!(validate_control("faders.4").is_ok());
        assert!(validate_control("rollers.1").is_ok());
        assert!(validate_control("rollers.2").is_err());
        assert!(validate_control("keys").is_err());
        assert!(validate_control("pedal.0").is_err());
    }
}
