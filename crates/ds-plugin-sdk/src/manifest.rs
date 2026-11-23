//! The plugin manifest, mirroring `schemas/plugin-manifest.json` (normative).
//!
//! The SDK serializes [`Manifest`] into the `register` message and validates it against the
//! schema's rules (patterns, lengths, bounds, `additionalProperties: false` via
//! `deny_unknown_fields`) *before* the handshake, so a bad manifest fails fast in the plugin
//! process instead of being killed by the host with `E_SCHEMA`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ServeError;

/// `id` pattern: `^[A-Za-z0-9._-]{1,80}$`.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// `version` pattern: `^[0-9]+\.[0-9]+\.[0-9]+$`.
pub fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// `net:<glob>` host-glob pattern: `net:` + `^[A-Za-z0-9.*-]{1,200}$`.
pub fn valid_net_glob(glob: &str) -> bool {
    !glob.is_empty()
        && glob.len() <= 200
        && glob
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'*' || b == b'-')
}

/// One action declared in the manifest. Field order matches the schema's `properties`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDef {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the action arguments (validated by the host with the full
    /// jsonschema crate before every invoke; the SDK checks the subset in
    /// [`crate::params`]).
    pub params: Value,
}

/// A capability grant. Serializes to the schema's string spellings, e.g. `"state.read"`,
/// `"net:lights.lan"`, `"fs:/home/u/media"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Capability {
    StateRead,
    StateWrite,
    DeviceWrite,
    Exec,
    Emit,
    /// `net:<host-glob>` — host-mediated network (S4). `*` matches exactly one DNS label,
    /// anchored against the full host.
    Net(String),
    /// `fs:<absolute root>`.
    Fs(String),
}

impl Capability {
    /// The wire/declared spelling.
    pub fn as_str(&self) -> String {
        match self {
            Self::StateRead => "state.read".to_string(),
            Self::StateWrite => "state.write".to_string(),
            Self::DeviceWrite => "device.write".to_string(),
            Self::Exec => "exec".to_string(),
            Self::Emit => "emit".to_string(),
            Self::Net(g) => format!("net:{g}"),
            Self::Fs(r) => format!("fs:{r}"),
        }
    }

    /// Parse a declared capability string; `None` if it does not match the schema pattern.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "state.read" => Self::StateRead,
            "state.write" => Self::StateWrite,
            "device.write" => Self::DeviceWrite,
            "exec" => Self::Exec,
            "emit" => Self::Emit,
            _ => {
                if let Some(g) = s.strip_prefix("net:") {
                    if !valid_net_glob(g) {
                        return None;
                    }
                    Self::Net(g.to_string())
                } else {
                    let r = s.strip_prefix("fs:")?;
                    if !r.starts_with('/') {
                        return None;
                    }
                    Self::Fs(r.to_string())
                }
            }
        })
    }
}

impl Serialize for Capability {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Capability::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("bad capability: {s}")))
    }
}

/// Events the plugin wants delivered (schema `events` enum, upstream event names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    #[serde(rename = "keyDown")]
    KeyDown,
    #[serde(rename = "keyUp")]
    KeyUp,
    #[serde(rename = "dialRotate")]
    DialRotate,
    #[serde(rename = "slide")]
    Slide,
    #[serde(rename = "state")]
    State,
    #[serde(rename = "willAppear")]
    WillAppear,
    #[serde(rename = "willDisappear")]
    WillDisappear,
    #[serde(rename = "focus")]
    Focus,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KeyDown => "keyDown",
            Self::KeyUp => "keyUp",
            Self::DialRotate => "dialRotate",
            Self::Slide => "slide",
            Self::State => "state",
            Self::WillAppear => "willAppear",
            Self::WillDisappear => "willDisappear",
            Self::Focus => "focus",
        }
    }
}

/// `resources` — applied as rlimits at spawn; defaults 10/64/64/1024.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_s: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nproc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nofile: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_mb: Option<u32>,
}

/// The manifest of `schemas/plugin-manifest.json`. Field order matches the schema so the
/// `register` wire message reads like the documented shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// argv array, executed directly — never through a shell. Defaults to the current
    /// executable (the command that started this process).
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionDef>,
    pub capabilities: Vec<Capability>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<EventKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fs_roots: Vec<String>,
}

impl Manifest {
    /// New manifest with the required fields. `command` defaults to `[current_exe]` (the
    /// process was started by exactly that command); override with [`Manifest::command`].
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: impl Into<String>) -> Self {
        let command = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .map(|s| vec![s])
            .unwrap_or_default();
        Self {
            id: id.into(),
            name: name.into(),
            version: version.into(),
            description: None,
            command,
            actions: Vec::new(),
            capabilities: Vec::new(),
            events: Vec::new(),
            resources: None,
            fs_roots: Vec::new(),
        }
    }

    /// Builder: set the argv array (1..=16 entries).
    pub fn command(mut self, argv: Vec<String>) -> Self {
        self.command = argv;
        self
    }

    /// Builder: set the human-readable description (≤ 500 chars).
    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }

    /// Builder: declare one action (id + JSON-Schema `params`).
    pub fn action(mut self, id: impl Into<String>, params: Value) -> Self {
        self.actions.push(ActionDef {
            id: id.into(),
            name: None,
            description: None,
            params,
        });
        self
    }

    /// Builder: grant one capability.
    pub fn capability(mut self, cap: Capability) -> Self {
        self.capabilities.push(cap);
        self
    }

    /// Builder: subscribe to one event kind.
    pub fn event(mut self, ev: EventKind) -> Self {
        self.events.push(ev);
        self
    }

    /// Builder: set the resource limits.
    pub fn resources(mut self, r: Resources) -> Self {
        self.resources = Some(r);
        self
    }

    /// Builder: add an fs root the plugin may read/write.
    pub fn fs_root(mut self, root: impl Into<String>) -> Self {
        self.fs_roots.push(root.into());
        self
    }

    /// Validate against `schemas/plugin-manifest.json` (patterns, lengths, bounds).
    pub fn validate(&self) -> Result<(), ServeError> {
        let bad = |m: String| ServeError::Manifest(m);
        if !valid_id(&self.id) {
            return Err(bad(format!(
                "id {:?} must match ^[A-Za-z0-9._-]{{1,80}}$",
                self.id
            )));
        }
        if self.name.is_empty() || self.name.len() > 120 {
            return Err(bad("name must be 1..=120 chars".into()));
        }
        if !valid_version(&self.version) {
            return Err(bad(format!(
                "version {:?} must match ^[0-9]+\\.[0-9]+\\.[0-9]+$",
                self.version
            )));
        }
        if let Some(d) = &self.description {
            if d.len() > 500 {
                return Err(bad("description must be <= 500 chars".into()));
            }
        }
        if self.command.is_empty()
            || self.command.len() > 16
            || self.command.iter().any(|c| c.is_empty())
        {
            return Err(bad("command must be 1..=16 non-empty argv entries".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for a in &self.actions {
            if !valid_id(&a.id) {
                return Err(bad(format!("action id {:?} is invalid", a.id)));
            }
            if !seen.insert(a.id.clone()) {
                return Err(bad(format!("duplicate action id {:?}", a.id)));
            }
            if let Some(n) = &a.name {
                if n.len() > 120 {
                    return Err(bad("action name must be <= 120 chars".into()));
                }
            }
            if let Some(d) = &a.description {
                if d.len() > 500 {
                    return Err(bad("action description must be <= 500 chars".into()));
                }
            }
            if !a.params.is_object() {
                return Err(bad(format!(
                    "action {:?} params must be a JSON object",
                    a.id
                )));
            }
        }
        for c in &self.capabilities {
            match c {
                Capability::Net(g) if !valid_net_glob(g) => {
                    return Err(bad(format!("bad net glob {g:?}")))
                }
                Capability::Fs(r) if !r.starts_with('/') => {
                    return Err(bad(format!("fs root {r:?} must be absolute")))
                }
                _ => {}
            }
        }
        if let Some(r) = &self.resources {
            let in_range = |v: u32, lo: u32, hi: u32| v >= lo && v <= hi;
            if let Some(v) = r.cpu_s {
                if !in_range(v, 1, 3600) {
                    return Err(bad("resources.cpu_s must be 1..=3600".into()));
                }
            }
            if let Some(v) = r.nproc {
                if !in_range(v, 1, 4096) {
                    return Err(bad("resources.nproc must be 1..=4096".into()));
                }
            }
            if let Some(v) = r.nofile {
                if !in_range(v, 8, 4096) {
                    return Err(bad("resources.nofile must be 8..=4096".into()));
                }
            }
            if let Some(v) = r.as_mb {
                if !in_range(v, 64, 65536) {
                    return Err(bad("resources.as_mb must be 64..=65536".into()));
                }
            }
        }
        for f in &self.fs_roots {
            if !f.starts_with('/') {
                return Err(bad(format!("fs_roots entry {f:?} must be absolute")));
            }
        }
        Ok(())
    }

    /// The manifest as a JSON value (the `manifest` field of `register`).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("Manifest is serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn demo() -> Manifest {
        Manifest::new("demo", "Demo counter", "0.1.0")
            .command(vec!["/usr/bin/demo-plugin".to_string()])
            .description("counter")
            .action(
                "bump",
                json!({"type":"object","properties":{"by":{"type":"integer","default":1}}}),
            )
            .capability(Capability::StateWrite)
            .capability(Capability::Net("lights.lan".to_string()))
            .event(EventKind::KeyDown)
            .resources(Resources {
                cpu_s: Some(10),
                nproc: Some(64),
                nofile: Some(64),
                as_mb: Some(1024),
            })
            .fs_root("/home/u/media")
    }

    #[test]
    fn valid_manifest_round_trips_through_json() {
        let m = demo();
        m.validate().unwrap();
        let v = m.to_value();
        // additionalProperties:false is mirrored by deny_unknown_fields on parse:
        let back: Manifest = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back.to_value(), v);
        assert_eq!(v["id"], "demo");
        assert_eq!(v["capabilities"], json!(["state.write", "net:lights.lan"]));
        assert_eq!(v["events"], json!(["keyDown"]));
        assert_eq!(
            v["actions"][0]["params"]["properties"]["by"]["type"],
            "integer"
        );
    }

    #[test]
    fn deny_unknown_fields_mirrors_additional_properties_false() {
        let v = serde_json::json!({"id":"demo","name":"n","version":"0.1.0","command":["x"],
            "capabilities":[], "bogus": 1});
        assert!(serde_json::from_value::<Manifest>(v).is_err());
    }

    #[test]
    fn validation_rejects_bad_fields() {
        let mut m = demo();
        m.id = "bad id!".into();
        assert!(m.validate().is_err());
        let mut m = demo();
        m.version = "1.0".into();
        assert!(m.validate().is_err());
        let mut m = demo();
        m.command = vec![];
        assert!(m.validate().is_err());
        let mut m = demo();
        m.capabilities = vec![Capability::Net("bad glob!".into())];
        assert!(m.validate().is_err());
        let mut m = demo();
        m.fs_roots = vec!["relative".into()];
        assert!(m.validate().is_err());
        let mut m = demo();
        m.actions.push(ActionDef {
            id: "bump".into(),
            name: None,
            description: None,
            params: json!({}),
        });
        assert!(m.validate().is_err(), "duplicate action id rejected");
    }

    #[test]
    fn capability_parse_round_trip() {
        for c in [
            Capability::StateRead,
            Capability::StateWrite,
            Capability::DeviceWrite,
            Capability::Exec,
            Capability::Emit,
            Capability::Net("*.lan".into()),
            Capability::Fs("/home/u/media".into()),
        ] {
            assert_eq!(Capability::parse(&c.as_str()), Some(c));
        }
        assert_eq!(Capability::parse("net:bad glob"), None);
        assert_eq!(Capability::parse("fs:relative"), None);
        assert_eq!(Capability::parse("nope"), None);
    }

    #[test]
    fn new_defaults_command_to_current_exe() {
        let m = Manifest::new("x", "x", "0.1.0");
        assert_eq!(m.command.len(), 1);
        assert!(m.command[0].starts_with('/'));
    }
}
