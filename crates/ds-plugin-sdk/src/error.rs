//! Error codes and error envelopes for the plugin side of the wire protocol.
//!
//! The codes are exactly the stable control-plane enum of `schemas/ipc-message.json`
//! (`$defs.error`): the plugin→host `error` object and the `ok:false` reply payload use the
//! same spellings, so one mapping table serves the whole deckstack surface. The SDK keeps its
//! own copy (dependency-light: serde/serde_json only) instead of depending on `ds-ipc`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The stable error codes (`schemas/ipc-message.json` `$defs.error`).
///
/// The variants are exactly the schema enum; [`ErrorCode::as_str`] is the wire spelling
/// (`E_SCHEMA`, …), which is also how the enum (de)serializes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Bad or missing token.
    EAuth,
    /// Valid token, but its scope is below the request's requirement.
    EScope,
    /// Path not found / bad syntax.
    EPath,
    /// Value violates the schema; also the usage error for an unknown op/action.
    ESchema,
    /// `if_rev` mismatch (host-side state conflict).
    EConflict,
    /// Size/rate/queue limit exceeded (e.g. oversized net URL or reply body).
    ELimit,
    /// Capability grant missing (plugins/exec/net/fs).
    ECap,
    /// Host unreachable / closed the pipe while a request was in flight.
    EUnreachable,
    /// Hardware error.
    EDevice,
    /// Protocol version mismatch.
    EVersion,
    /// Bug / handler failed unexpectedly.
    EInternal,
}

impl ErrorCode {
    /// The wire spelling used in the `code` field.
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

    /// The stable CLI exit code for this error (`docs/contracts/cli.md` §3). The plugin
    /// process itself uses the same mapping for its own fatal errors.
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

impl Serialize for ErrorCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ErrorCode::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown error code: {s}")))
    }
}

/// One error object as it appears on the wire: `{"code":…,"message":…}` (plus optional
/// `path`/`current_rev` for host state errors — the plugin never emits those two).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginError {
    pub code: ErrorCode,
    pub message: String,
}

impl PluginError {
    /// A new error with just code + message.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// Schema/usage error (unknown action, params do not match the declared JSON Schema).
    pub fn schema(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ESchema, message)
    }

    /// Capability error (the host denied a request/effect with `E_CAP`).
    pub fn cap(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ECap, message)
    }

    /// Limit error (oversized URL/body/line, too many headers…).
    pub fn limit(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ELimit, message)
    }

    /// The host closed the pipe / never answered a request.
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::EUnreachable, message)
    }

    /// Anything else (handler bug, unexpected host message while waiting).
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::EInternal, message)
    }

    /// The wire JSON object.
    pub fn to_value(&self) -> Value {
        serde_json::json!({ "code": self.code.as_str(), "message": self.message })
    }

    /// Parse a wire error object; `None` if the shape is wrong.
    pub fn from_value(v: &Value) -> Option<Self> {
        let code = ErrorCode::parse(v.get("code")?.as_str()?)?;
        let message = v.get("message")?.as_str()?.to_string();
        Some(Self { code, message })
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for PluginError {}

/// Fatal, top-level errors of [`crate::serve`]: the plugin could not run the protocol at all
/// (bad manifest, wrong first message, protocol mismatch, I/O error). These are reported on
/// stderr and via the process exit code — the host's watchdog treats them like a crash.
#[derive(Debug)]
pub enum ServeError {
    /// The manifest does not satisfy `schemas/plugin-manifest.json`.
    Manifest(String),
    /// First host message was not `hello`.
    ExpectedHello(String),
    /// `hello.protocol` != 1: the host kills mismatched plugins; surfaced cleanly here.
    Version { got: u64, want: u64 },
    /// stdin closed before the handshake completed.
    UnexpectedEof,
    /// I/O error on stdin/stdout.
    Io(std::io::Error),
}

impl ServeError {
    /// Process exit code for this error, using the documented CLI mapping where it applies
    /// (`E_VERSION`/`E_INTERNAL` → 8, `E_SCHEMA` → 1).
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Manifest(_) => ErrorCode::ESchema.exit_code(),
            Self::ExpectedHello(_) => ErrorCode::ESchema.exit_code(),
            Self::Version { .. } => ErrorCode::EVersion.exit_code(),
            Self::UnexpectedEof => ErrorCode::EUnreachable.exit_code(),
            Self::Io(_) => ErrorCode::EInternal.exit_code(),
        }
    }
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(m) => write!(f, "invalid manifest: {m}"),
            Self::ExpectedHello(m) => write!(f, "expected hello as first host message: {m}"),
            Self::Version { got, want } => {
                write!(
                    f,
                    "protocol version mismatch: host sent {got}, SDK speaks {want}"
                )
            }
            Self::UnexpectedEof => write!(f, "host closed stdin before the handshake"),
            Self::Io(e) => write!(f, "stdio I/O error: {e}"),
        }
    }
}

impl std::error::Error for ServeError {}

impl From<std::io::Error> for ServeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

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
        assert_eq!(ErrorCode::ECap.exit_code(), 4);
        assert_eq!(ErrorCode::ELimit.exit_code(), 5);
        assert_eq!(ErrorCode::EVersion.exit_code(), 8);
    }

    #[test]
    fn error_envelope_round_trip() {
        let err = PluginError::cap("net:lights.lan not granted");
        let v = err.to_value();
        assert_eq!(
            v,
            serde_json::json!({"code": "E_CAP", "message": "net:lights.lan not granted"})
        );
        assert_eq!(PluginError::from_value(&v), Some(err));
        assert_eq!(
            PluginError::from_value(&serde_json::json!({"code": "E_NOPE"})),
            None
        );
    }
}
