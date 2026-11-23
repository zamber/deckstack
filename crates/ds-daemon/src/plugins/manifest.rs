//! Plugin manifest (ticket DS-050): the `manifest.json` every plugin ships in
//! `~/.local/share/deckstack/plugins/<id>/`, validated against the normative
//! `schemas/plugin-manifest.json` (embedded via `ds_state::schema`) **before** the
//! manifest is used for anything (contract `docs/contracts/plugins.md` §3: a manifest
//! mismatch at `register` is `E_SCHEMA` + kill).
//!
//! On top of the JSON Schema the host enforces a few semantic rules the schema cannot
//! express: unique action ids, a `command` that needs no PATH lookup (argv[0] with no
//! `/` is resolved against the fixed child PATH up front, so the spawn is always an
//! absolute path — the daemon never relies on an ambient PATH), and non-empty
//! `fs_roots` entries.

use ds_ipc::error::{ErrorCode, IpcError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One declared action: `id` + the JSON Schema its invoke `args` are validated against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the action arguments (validated before every invoke).
    pub params: Value,
}

/// Resource limits (rlimits) requested by the manifest; applied at spawn. Defaults
/// 10 s CPU / 64 nproc / 64 nofile / 1024 MiB AS (docs/20-security.md §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default = "default_cpu_s")]
    pub cpu_s: u64,
    #[serde(default = "default_nproc")]
    pub nproc: u64,
    #[serde(default = "default_nofile")]
    pub nofile: u64,
    #[serde(default = "default_as_mb")]
    pub as_mb: u64,
}

pub const fn default_cpu_s() -> u64 {
    crate::policy::RLIMIT_CPU_SECS
}
pub const fn default_nproc() -> u64 {
    crate::policy::RLIMIT_NPROC
}
pub const fn default_nofile() -> u64 {
    crate::policy::RLIMIT_NOFILE
}
pub const fn default_as_mb() -> u64 {
    crate::policy::RLIMIT_AS_BYTES >> 20
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            cpu_s: default_cpu_s(),
            nproc: default_nproc(),
            nofile: default_nofile(),
            as_mb: default_as_mb(),
        }
    }
}

/// A validated plugin manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// argv array, executed directly — never through a shell.
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    pub capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fs_roots: Vec<String>,
}

impl Manifest {
    /// The effective resource limits (manifest override or the defaults).
    pub fn resources(&self) -> Resources {
        self.resources.unwrap_or_default()
    }

    /// Find one action by id.
    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.id == id)
    }

    /// Does the manifest grant this exact capability string (`state.write`,
    /// `net:lights.lan`, …)?
    pub fn has_cap(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }

    /// Does the manifest grant a `net:<glob>` capability matching `host`?
    /// Glob semantics (M5): `*` matches exactly one DNS label, matching is anchored
    /// against the full host (`lights.lan` matches only `lights.lan`; `*.lan`
    /// matches `lights.lan` but not `lan`, `a.b.lan` or `lights.lan.evil.com`).
    pub fn net_cap_matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.capabilities.iter().any(|cap| {
            cap.strip_prefix("net:")
                .is_some_and(|glob| super::wire::glob_matches_host(glob, &host))
        })
    }

    /// Does the manifest grant an `fs:<root>` capability covering `path`?
    pub fn fs_cap_covers(&self, path: &str) -> bool {
        self.capabilities.iter().any(|cap| {
            cap.strip_prefix("fs:")
                .is_some_and(|root| super::wire::path_under_root(path, root))
        })
    }

    /// Serialize back to the exact JSON shape the schema validates (used for the
    /// `register` comparison and for `/plugins/<id>/manifest` in state).
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).expect("Manifest serializes")
    }
}

/// The compiled `schemas/plugin-manifest.json` validator (draft 2020-12).
pub fn manifest_validator() -> &'static jsonschema::Validator {
    static VALIDATOR: std::sync::OnceLock<jsonschema::Validator> = std::sync::OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let schema: Value = serde_json::from_str(ds_state::schema::PLUGIN_MANIFEST_SCHEMA_JSON)
            .expect("embedded schemas/plugin-manifest.json parses");
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .should_validate_formats(true)
            .build(&schema)
            .expect("schemas/plugin-manifest.json compiles")
    })
}

fn schema_err(msg: impl Into<String>) -> IpcError {
    IpcError::new(ErrorCode::ESchema, msg)
}

/// Validate a raw manifest JSON value against `schemas/plugin-manifest.json` plus the
/// semantic rules. Returns the typed manifest. Every failure is `E_SCHEMA`.
pub fn validate_manifest(raw: &Value) -> Result<Manifest, IpcError> {
    if let Err(err) = manifest_validator().validate(raw) {
        return Err(schema_err(format!(
            "plugin manifest violates schemas/plugin-manifest.json: {err}"
        )));
    }
    let manifest: Manifest = serde_json::from_value(raw.clone())
        .map_err(|e| schema_err(format!("plugin manifest does not deserialize: {e}")))?;
    // Semantic rules the schema cannot express.
    if manifest.command.is_empty() {
        return Err(schema_err("plugin manifest 'command' must not be empty"));
    }
    if manifest.command.iter().any(|c| c.contains('\0')) {
        return Err(schema_err(
            "plugin manifest 'command' must not contain NUL bytes",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for a in &manifest.actions {
        if !seen.insert(a.id.as_str()) {
            return Err(schema_err(format!(
                "plugin manifest has a duplicate action id {:?}",
                a.id
            )));
        }
    }
    for root in &manifest.fs_roots {
        if !root.starts_with('/') {
            return Err(schema_err(format!(
                "plugin manifest fs_root {root:?} must be an absolute path"
            )));
        }
    }
    Ok(manifest)
}

/// Read + validate `<dir>/manifest.json` (no dir-name check — used by `install` from an
/// arbitrary source dir).
pub fn read_manifest(dir: &Path) -> Result<Manifest, IpcError> {
    let path = dir.join("manifest.json");
    let raw_str = std::fs::read_to_string(&path).map_err(|e| {
        IpcError::new(
            ErrorCode::EPath,
            format!("cannot read plugin manifest {}: {e}", path.display()),
        )
    })?;
    let raw: Value = serde_json::from_str(&raw_str).map_err(|e| {
        schema_err(format!(
            "plugin manifest {} is not valid JSON: {e}",
            path.display()
        ))
    })?;
    validate_manifest(&raw)
}

/// Load + validate `<dir>/manifest.json`, additionally requiring the directory name to
/// equal the manifest id (an installed plugin lives at `<base>/<id>/`).
pub fn load_manifest(dir: &Path) -> Result<Manifest, IpcError> {
    let manifest = read_manifest(dir)?;
    if manifest.id != dir_name(dir) {
        return Err(schema_err(format!(
            "plugin dir {:?} does not match the manifest id {:?}",
            dir.file_name().map(|s| s.to_string_lossy().to_string()),
            manifest.id
        )));
    }
    Ok(manifest)
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Resolve the manifest's `command[0]` to an absolute path. A bare name (`python3`)
/// is resolved against the fixed child PATH (`/usr/local/bin:/usr/bin:/bin`) — the
/// daemon never uses the ambient PATH and never spawns through a shell. Returns
/// `E_PATH` when the binary is not found.
pub fn resolve_command0(command: &[String]) -> Result<PathBuf, IpcError> {
    let c0 = &command[0];
    if c0.contains('/') {
        return Ok(PathBuf::from(c0));
    }
    for dir in super::spawn::CHILD_PATH_DIRS {
        let cand = Path::new(dir).join(c0);
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(IpcError::new(
        ErrorCode::EPath,
        format!(
            "plugin command[0] {c0:?} has no '/' and is not in the fixed child PATH {:?} — \
             no ambient PATH lookup is ever done",
            super::spawn::CHILD_PATH_DIRS
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good_raw() -> Value {
        json!({
            "id": "demo",
            "name": "Demo",
            "version": "0.1.0",
            "command": ["/usr/bin/python3", "plugin.py"],
            "capabilities": ["state.write", "net:lights.lan", "emit"],
            "actions": [
                {"id": "bump", "params": {"type": "object", "properties": {"by": {"type": "integer"}}}}
            ],
            "events": ["keyDown"],
            "resources": {"cpu_s": 5},
            "fs_roots": ["/home/u/media"]
        })
    }

    #[test]
    fn valid_manifest_passes_and_defaults_apply() {
        let m = validate_manifest(&good_raw()).unwrap();
        assert_eq!(m.id, "demo");
        assert_eq!(m.resources().cpu_s, 5);
        assert_eq!(m.resources().nproc, 64);
        assert_eq!(m.action("bump").unwrap().id, "bump");
        assert!(m.has_cap("state.write"));
        assert!(!m.has_cap("exec"));
        // round-trip through the schema validator (state embeds the manifest)
        assert!(manifest_validator().validate(&m.to_json()).is_ok());
    }

    #[test]
    fn schema_violations_are_e_schema() {
        let mut bad = good_raw();
        bad["version"] = json!("1.0"); // not semver
        assert_eq!(
            validate_manifest(&bad).unwrap_err().code,
            ErrorCode::ESchema
        );
        let mut bad = good_raw();
        bad["id"] = json!("bad id!"); // pattern violation
        assert_eq!(
            validate_manifest(&bad).unwrap_err().code,
            ErrorCode::ESchema
        );
        let mut bad = good_raw();
        bad["capabilities"] = json!(["root"]); // pattern violation
        assert_eq!(
            validate_manifest(&bad).unwrap_err().code,
            ErrorCode::ESchema
        );
        let mut bad = good_raw();
        bad["command"] = json!([]); // minItems 1
        assert_eq!(
            validate_manifest(&bad).unwrap_err().code,
            ErrorCode::ESchema
        );
        let mut bad = good_raw();
        bad["extra"] = json!(1); // additionalProperties false
        assert_eq!(
            validate_manifest(&bad).unwrap_err().code,
            ErrorCode::ESchema
        );
    }

    #[test]
    fn duplicate_action_ids_are_rejected() {
        let mut raw = good_raw();
        raw["actions"] = json!([
            {"id": "bump", "params": {}},
            {"id": "bump", "params": {}}
        ]);
        assert_eq!(
            validate_manifest(&raw).unwrap_err().code,
            ErrorCode::ESchema
        );
    }

    #[test]
    fn net_glob_matching_is_anchored_one_label_per_star() {
        let m = validate_manifest(&good_raw()).unwrap();
        assert!(m.net_cap_matches("lights.lan"));
        assert!(!m.net_cap_matches("lan"));
        assert!(!m.net_cap_matches("a.b.lan"));
        assert!(!m.net_cap_matches("lights.lan.evil.com"));
        assert!(m.net_cap_matches("LIGHTS.lan")); // DNS matching is case-insensitive
        let mut raw = good_raw();
        raw["capabilities"] = json!(["net:*.lan"]);
        let m = validate_manifest(&raw).unwrap();
        assert!(m.net_cap_matches("lights.lan"));
        assert!(!m.net_cap_matches("a.b.lan"));
        assert!(!m.net_cap_matches("lan"));
        let mut raw = good_raw();
        raw["capabilities"] = json!(["net:*"]);
        let m = validate_manifest(&raw).unwrap();
        assert!(m.net_cap_matches("lights"));
        assert!(!m.net_cap_matches("lights.lan"));
    }

    #[test]
    fn fs_cap_prefix_checks() {
        let mut raw = good_raw();
        raw["capabilities"] = json!(["fs:/home/u/media"]);
        let m = validate_manifest(&raw).unwrap();
        assert!(m.fs_cap_covers("/home/u/media/x.png"));
        assert!(m.fs_cap_covers("/home/u/media"));
        assert!(!m.fs_cap_covers("/home/u/mediax"));
        assert!(!m.fs_cap_covers("/home/u/other"));
    }

    #[test]
    fn resolve_command0_never_uses_ambient_path() {
        assert_eq!(
            resolve_command0(&["/usr/bin/python3".into(), "x".into()]).unwrap(),
            PathBuf::from("/usr/bin/python3")
        );
        // bare name resolved against the fixed child PATH
        let p = resolve_command0(&["sh".into()]).unwrap();
        assert!(p.is_absolute() && p.is_file());
        // unknown bare name -> E_PATH
        let err = resolve_command0(&["definitely-not-a-real-binary-xyz".into()]).unwrap_err();
        assert_eq!(err.code, ErrorCode::EPath);
    }
}
