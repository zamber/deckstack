//! Path addressing and validation for the state document (`docs/contracts/state.md` §1).
//!
//! Paths are `/`-separated (`/controls/keys/0/image`). In queries a single `*` segment
//! matches exactly one segment. Write paths must stay inside the documented vocabulary:
//! top-level sections, control kinds and index ranges (keys 0-14, knobs 0-2, faders 0-4,
//! rollers 0-1), stream targets (`keys | dial | key:<0-14>`), device serials, binding ids
//! (`b0001`, …) and plugin ids. `/telemetry/*` is not writable through durable ops — it is
//! the non-durable plane (`Store::set_telemetry`).

use crate::error::{Result, StateError};

/// Maximum number of segments in a path.
pub const MAX_SEGMENTS: usize = 16;
/// Maximum length of a whole path string.
pub const MAX_PATH_LEN: usize = 400;
/// Maximum length of a single segment.
pub const MAX_SEGMENT_LEN: usize = 80;

/// Top-level sections of the durable document (plus `telemetry`, the non-durable plane).
pub const TOP_LEVEL: [&str; 10] = [
    "meta",
    "devices",
    "controls",
    "bindings",
    "plugins",
    "streams",
    "settings",
    "policy",
    "focus",
    "telemetry",
];

const fn path_err(msg: String) -> StateError {
    StateError::Path(msg)
}

/// A parsed state path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    raw: String,
    segments: Vec<String>,
}

impl Path {
    /// Parse and syntax-check a path. `/` is the root (zero segments).
    pub fn parse(input: &str) -> Result<Self> {
        if input.is_empty() || !input.starts_with('/') {
            return Err(path_err(format!("path must start with '/': {input:?}")));
        }
        if input.len() > MAX_PATH_LEN {
            return Err(path_err(format!(
                "path longer than {MAX_PATH_LEN} chars: {input:?}"
            )));
        }
        let mut segments = Vec::new();
        if input != "/" {
            for seg in input[1..].split('/') {
                if seg.is_empty() {
                    return Err(path_err(format!("empty path segment in {input:?}")));
                }
                if seg == "." || seg == ".." {
                    return Err(path_err(format!(
                        "'.'/'..' segments are not allowed in {input:?}"
                    )));
                }
                if seg.len() > MAX_SEGMENT_LEN {
                    return Err(path_err(format!(
                        "segment longer than {MAX_SEGMENT_LEN} chars in {input:?}"
                    )));
                }
                if !seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '*'))
                {
                    return Err(path_err(format!(
                        "illegal character in path segment {seg:?}"
                    )));
                }
                segments.push(seg.to_string());
            }
        }
        if segments.len() > MAX_SEGMENTS {
            return Err(path_err(format!(
                "more than {MAX_SEGMENTS} segments in {input:?}"
            )));
        }
        Ok(Self {
            raw: input.to_string(),
            segments,
        })
    }

    /// The root path `/`.
    pub fn root() -> Self {
        Self {
            raw: "/".to_string(),
            segments: Vec::new(),
        }
    }

    /// The path segments (empty for the root).
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The original string form.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Whether any segment is the `*` wildcard.
    pub fn has_wildcard(&self) -> bool {
        self.segments.iter().any(|s| s == "*")
    }

    /// Whether this path addresses the non-durable telemetry plane.
    pub fn is_telemetry(&self) -> bool {
        self.segments.first().is_some_and(|s| s == "telemetry")
    }

    /// Subscription matching: `self` (a pattern) matches `event` when the pattern is a
    /// segment-wise prefix of the event path, with `*` matching exactly one segment.
    pub fn matches(&self, event: &Path) -> bool {
        if self.segments.len() > event.segments.len() {
            return false;
        }
        self.segments
            .iter()
            .zip(event.segments.iter())
            .all(|(p, e)| p == "*" || p == e)
    }

    /// Read-side validation: the first segment must be a known top-level section.
    pub fn validate_read(&self) -> Result<()> {
        if let Some(first) = self.segments.first() {
            if !TOP_LEVEL.contains(&first.as_str()) {
                return Err(path_err(format!("unknown top-level path '/{first}'")));
            }
        }
        Ok(())
    }

    /// Write-side validation of the durable vocabulary (`docs/contracts/state.md` §1).
    ///
    /// Note: `schemas/state.json` uses `patternProperties` without
    /// `additionalProperties: false` under `/controls` and `/streams`, so JSON-Schema
    /// validation alone would admit e.g. `/controls/keys/99`. This layer enforces the
    /// documented ranges (S8) on top of the schema.
    pub fn validate_write(&self) -> Result<()> {
        if self.has_wildcard() {
            return Err(path_err(
                "wildcard '*' is not allowed in write paths".to_string(),
            ));
        }
        let Some(first) = self.segments.first() else {
            return Err(path_err(
                "cannot write the root document '/'; use a section path".to_string(),
            ));
        };
        match first.as_str() {
            "meta" => Err(path_err(
                "/meta is read-only; rev is managed by the store".to_string(),
            )),
            "telemetry" => Err(path_err(
                "/telemetry/* is non-durable; use Store::set_telemetry (no rev, never persisted)"
                    .to_string(),
            )),
            "devices" => {
                if let Some(serial) = self.segments.get(1) {
                    if !is_device_serial(serial) {
                        return Err(path_err(format!("bad device serial {serial:?}")));
                    }
                }
                Ok(())
            }
            "controls" => {
                let kind = self.segments.get(1).map_or("", String::as_str);
                match kind {
                    "keys" => check_index(self.segments.get(2), 14),
                    "knobs" => check_index(self.segments.get(2), 2),
                    "faders" => check_index(self.segments.get(2), 4),
                    "rollers" => check_index(self.segments.get(2), 1),
                    "dial" => Ok(()),
                    "" => Ok(()), // replacing the whole /controls object
                    other => Err(path_err(format!("unknown control kind '{other}'"))),
                }
            }
            "bindings" => match self.segments.len() {
                1 => Ok(()),
                2 if is_binding_id(&self.segments[1]) => Ok(()),
                2 => Err(path_err(format!("bad binding id {:?}", self.segments[1]))),
                _ => Err(path_err(
                    "bindings are addressed as /bindings or /bindings/<id>".to_string(),
                )),
            },
            "plugins" => {
                if let Some(id) = self.segments.get(1) {
                    if !is_plugin_id(id) {
                        return Err(path_err(format!("bad plugin id {id:?}")));
                    }
                }
                Ok(())
            }
            "streams" => {
                if let Some(target) = self.segments.get(1) {
                    if !is_stream_target(target) {
                        return Err(path_err(format!("bad stream target {target:?}")));
                    }
                }
                Ok(())
            }
            "settings" | "policy" | "focus" => Ok(()),
            other => Err(path_err(format!("unknown top-level path '/{other}'"))),
        }
    }

    /// Write-side validation for the telemetry plane: `/telemetry`,
    /// `/telemetry/streams`, `/telemetry/streams/<target>` and below.
    pub fn validate_telemetry_write(&self) -> Result<()> {
        if !self.is_telemetry() {
            return Err(path_err(
                "set_telemetry requires a /telemetry/... path".to_string(),
            ));
        }
        if self.has_wildcard() {
            return Err(path_err(
                "wildcard '*' is not allowed in write paths".to_string(),
            ));
        }
        match self.segments.get(1).map_or("", String::as_str) {
            "" => Ok(()), // /telemetry itself
            "streams" => {
                if let Some(target) = self.segments.get(2) {
                    if !is_stream_target(target) {
                        return Err(path_err(format!("bad telemetry stream target {target:?}")));
                    }
                }
                Ok(())
            }
            other => Err(path_err(format!(
                "unknown telemetry path '/telemetry/{other}'"
            ))),
        }
    }
}

fn check_index(seg: Option<&String>, max: u32) -> Result<()> {
    if let Some(s) = seg {
        match s.parse::<u32>() {
            Ok(i) if i <= max => Ok(()),
            _ => Err(path_err(format!(
                "control index '{s}' out of range (0-{max}) — S8: only documented indices are addressable"
            ))),
        }
    } else {
        Ok(())
    }
}

fn is_device_serial(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_binding_id(s: &str) -> bool {
    s.len() >= 5 && s.starts_with('b') && s[1..].chars().all(|c| c.is_ascii_digit())
}

fn is_plugin_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SEGMENT_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_stream_target(s: &str) -> bool {
    s == "keys"
        || s == "dial"
        || (s.starts_with("key:") && check_index(Some(&s[4..].to_string()), 14).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ok() {
        let p = Path::parse("/controls/keys/0/image").unwrap();
        assert_eq!(p.segments().len(), 4);
        assert_eq!(p.as_str(), "/controls/keys/0/image");
        assert!(Path::parse("/").unwrap().segments().is_empty());
        assert!(Path::parse("/telemetry/streams/key:14/fps")
            .unwrap()
            .is_telemetry());
        assert!(Path::parse("/controls/*").unwrap().has_wildcard());
    }

    #[test]
    fn parse_rejects_bad_syntax() {
        for bad in [
            "",
            "controls",
            "/controls//keys",
            "/controls/",
            "//",
            "/a b/c",
            "/x\ty",
            "/plugins/../etc",
            "/a/./b",
        ] {
            assert!(Path::parse(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn write_vocabulary() {
        assert!(Path::parse("/controls/keys/14/label")
            .unwrap()
            .validate_write()
            .is_ok());
        assert!(Path::parse("/controls/keys/15")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/controls/keys/99/image")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/controls/dial/image")
            .unwrap()
            .validate_write()
            .is_ok());
        assert!(Path::parse("/controls/knobs/2/led")
            .unwrap()
            .validate_write()
            .is_ok());
        assert!(Path::parse("/controls/knobs/3/led")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/streams/key:14")
            .unwrap()
            .validate_write()
            .is_ok());
        assert!(Path::parse("/streams/panel")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/bindings/b0001")
            .unwrap()
            .validate_write()
            .is_ok());
        assert!(Path::parse("/bindings/x1")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/meta/rev").unwrap().validate_write().is_err());
        assert!(Path::parse("/telemetry/streams/keys")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/nope").unwrap().validate_write().is_err());
        assert!(Path::parse("/controls/*")
            .unwrap()
            .validate_write()
            .is_err());
        assert!(Path::parse("/settings/log/level")
            .unwrap()
            .validate_write()
            .is_ok());
        assert!(Path::parse("/focus/app").unwrap().validate_write().is_ok());
    }

    #[test]
    fn telemetry_write_validation() {
        assert!(Path::parse("/telemetry")
            .unwrap()
            .validate_telemetry_write()
            .is_ok());
        assert!(Path::parse("/telemetry/streams/keys/sent")
            .unwrap()
            .validate_telemetry_write()
            .is_ok());
        assert!(Path::parse("/telemetry/streams/key:14")
            .unwrap()
            .validate_telemetry_write()
            .is_ok());
        assert!(Path::parse("/telemetry/streams/key:15")
            .unwrap()
            .validate_telemetry_write()
            .is_err());
        assert!(Path::parse("/telemetry/other")
            .unwrap()
            .validate_telemetry_write()
            .is_err());
        assert!(Path::parse("/controls/keys/0")
            .unwrap()
            .validate_telemetry_write()
            .is_err());
    }

    #[test]
    fn pattern_matching() {
        let pat = Path::parse("/controls/*").unwrap();
        assert!(pat.matches(&Path::parse("/controls/keys").unwrap()));
        assert!(pat.matches(&Path::parse("/controls/keys/0/image").unwrap()));
        assert!(!pat.matches(&Path::parse("/settings/x").unwrap()));
        assert!(!pat.matches(&Path::parse("/controls").unwrap()));
        let root = Path::root();
        assert!(root.matches(&Path::parse("/anything/at/all").unwrap()));
        let exact = Path::parse("/controls/keys/0").unwrap();
        assert!(exact.matches(&Path::parse("/controls/keys/0").unwrap()));
        assert!(exact.matches(&Path::parse("/controls/keys/0/image").unwrap()));
    }
}
