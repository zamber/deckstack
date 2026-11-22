//! The error envelope of `docs/contracts/ipc.md` §4: stable codes, the CLI exit-code
//! mapping, and the JSON shape (`{"code":…, "message":…, "path"?, "current_rev"?}`).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The stable error codes of the control plane (`schemas/ipc-message.json` `$defs.error`).
///
/// The variants are exactly the schema enum; [`ErrorCode::as_str`] is the wire spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorCode {
    /// Bad or missing token (exit 4).
    EAuth,
    /// Valid token, but its scope is below the request's requirement (exit 4).
    EScope,
    /// Path not found / bad syntax (exit 2).
    EPath,
    /// Value violates the schema; also the "usage error" for an unknown op (exit 1) —
    /// an op that is not in the schema's op list fails message validation before dispatch.
    ESchema,
    /// `if_rev` mismatch; `current_rev` rides along in the envelope (exit 3).
    EConflict,
    /// Size/rate/queue limit (exit 5).
    ELimit,
    /// Capability grant missing — plugins/exec/net/fs (exit 4).
    ECap,
    /// Daemon not running; client-side only (exit 6).
    EUnreachable,
    /// Hardware error, incl. `Forbidden` (exit 7).
    EDevice,
    /// Protocol mismatch (exit 8).
    EVersion,
    /// Bug (exit 8).
    EInternal,
}

impl ErrorCode {
    /// The wire spelling used in the envelope's `code` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EAuth => "E_AUTH",
            Self::EScope => "E_SCOPE",
            Self::EPath => "E_PATH",
            Self::ESchema => "E_SCHEMA",
            Self::EConflict => "E_CONFLICT",
            Self::ELimit => "E_LIMIT",
            Self::ECap => "E_CAP",
            Self::EUnreachable => "E_UNREACHABLE",
            Self::EDevice => "E_DEVICE",
            Self::EVersion => "E_VERSION",
            Self::EInternal => "E_INTERNAL",
        }
    }

    /// Parse the wire spelling; `None` for anything not in the schema enum.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "E_AUTH" => Self::EAuth,
            "E_SCOPE" => Self::EScope,
            "E_PATH" => Self::EPath,
            "E_SCHEMA" => Self::ESchema,
            "E_CONFLICT" => Self::EConflict,
            "E_LIMIT" => Self::ELimit,
            "E_CAP" => Self::ECap,
            "E_UNREACHABLE" => Self::EUnreachable,
            "E_DEVICE" => Self::EDevice,
            "E_VERSION" => Self::EVersion,
            "E_INTERNAL" => Self::EInternal,
            _ => return None,
        })
    }

    /// The stable CLI exit code for this error (`docs/contracts/cli.md` §3,
    /// `docs/contracts/ipc.md` §4).
    pub fn exit_code(self) -> i32 {
        match self {
            Self::EAuth | Self::EScope | Self::ECap => 4,
            Self::EPath => 2,
            Self::ESchema => 1,
            Self::EConflict => 3,
            Self::ELimit => 5,
            Self::EUnreachable => 6,
            Self::EDevice => 7,
            Self::EVersion | Self::EInternal => 8,
        }
    }
}

/// One error of the envelope: `{"code","message","path"?,"current_rev"?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_rev: Option<u64>,
}

impl IpcError {
    /// A new error with just code + message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            path: None,
            current_rev: None,
        }
    }

    /// Attach a path (echoed back for `E_PATH`).
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attach the current revision (for `E_CONFLICT`).
    pub fn with_current_rev(mut self, rev: u64) -> Self {
        self.current_rev = Some(rev);
        self
    }

    /// The envelope JSON object (`$defs.error` of `schemas/ipc-message.json`).
    pub fn to_value(&self) -> Value {
        let mut m = serde_json::Map::new();
        m.insert("code".into(), json!(self.code.as_str()));
        m.insert("message".into(), json!(self.message));
        if let Some(p) = &self.path {
            m.insert("path".into(), json!(p));
        }
        if let Some(r) = self.current_rev {
            m.insert("current_rev".into(), json!(r));
        }
        Value::Object(m)
    }

    /// Parse an envelope; `None` if the shape is wrong (missing/unknown code etc.).
    pub fn from_value(v: &Value) -> Option<Self> {
        let code = ErrorCode::parse(v.get("code")?.as_str()?)?;
        let message = v.get("message")?.as_str()?.to_string();
        let path = v.get("path").and_then(|p| p.as_str()).map(str::to_string);
        let current_rev = v.get("current_rev").and_then(|r| r.as_u64());
        Some(Self {
            code,
            message,
            path,
            current_rev,
        })
    }
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for IpcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_round_trip_and_exit_codes() {
        for code in [
            ErrorCode::EAuth,
            ErrorCode::EScope,
            ErrorCode::EPath,
            ErrorCode::ESchema,
            ErrorCode::EConflict,
            ErrorCode::ELimit,
            ErrorCode::ECap,
            ErrorCode::EUnreachable,
            ErrorCode::EDevice,
            ErrorCode::EVersion,
            ErrorCode::EInternal,
        ] {
            assert_eq!(ErrorCode::parse(code.as_str()), Some(code));
        }
        assert_eq!(ErrorCode::parse("E_NOPE"), None);
        assert_eq!(ErrorCode::EAuth.exit_code(), 4);
        assert_eq!(ErrorCode::EScope.exit_code(), 4);
        assert_eq!(ErrorCode::EPath.exit_code(), 2);
        assert_eq!(ErrorCode::ESchema.exit_code(), 1);
        assert_eq!(ErrorCode::EConflict.exit_code(), 3);
        assert_eq!(ErrorCode::ELimit.exit_code(), 5);
        assert_eq!(ErrorCode::EUnreachable.exit_code(), 6);
        assert_eq!(ErrorCode::EDevice.exit_code(), 7);
        assert_eq!(ErrorCode::EVersion.exit_code(), 8);
        assert_eq!(ErrorCode::EInternal.exit_code(), 8);
    }

    #[test]
    fn envelope_round_trip() {
        let err = IpcError::new(ErrorCode::EConflict, "rev is 42").with_current_rev(42);
        let v = err.to_value();
        assert_eq!(v["code"], "E_CONFLICT");
        assert_eq!(v["current_rev"], 42);
        assert!(v.get("path").is_none());
        let back = IpcError::from_value(&v).unwrap();
        assert_eq!(back, err);
    }
}
