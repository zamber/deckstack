//! Output shaping and the error/exit-code contract of `docs/contracts/cli.md` §2–§3:
//! human default (one line per fact), `--json` = exactly one JSON object on stdout
//! (pretty; `--compact` one line), `--ndjson` = one object per line for streaming verbs.
//! Errors go to stderr — human text by default, one JSON object with `--json` — and
//! carry the stable exit code 0–8 mapped from the ds-client error codes.

use ds_client::ClientError;
use ds_ipc::error::{ErrorCode, IpcError};
use serde_json::{json, Value};

/// A failure of one `ds` invocation: a usage error (exit 1) or a client/daemon error.
#[derive(Debug)]
pub enum Failure {
    /// Bad arguments / contract violation on the client side → `E_SCHEMA`, exit 1.
    Usage(String),
    /// Anything from ds-client (unreachable, server envelope, limit, protocol).
    Client(ClientError),
}

impl Failure {
    /// The stable CLI exit code (`docs/contracts/cli.md` §3).
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 1,
            Self::Client(e) => e.exit_code(),
        }
    }

    /// The stable IPC error code.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Usage(_) => ErrorCode::ESchema,
            Self::Client(e) => e.code(),
        }
    }

    /// The human-readable message (server envelopes render without the `E_*:` prefix,
    /// unreachable carries the `ds daemon start` hint — `docs/contracts/cli.md` §5).
    pub fn message(&self) -> String {
        match self {
            Self::Usage(m) => m.clone(),
            Self::Client(ClientError::Unreachable { path, hint, .. }) => {
                format!("daemon unreachable at {} — {hint}", path.display())
            }
            Self::Client(ClientError::Server(e)) => {
                // ds-state renders its errors as "E_PATH: …"; the human line already
                // carries the code in the [exit N] suffix, so strip the prefix
                let prefix = format!("{}: ", e.code.as_str());
                e.message
                    .strip_prefix(prefix.as_str())
                    .unwrap_or(&e.message)
                    .to_string()
            }
            Self::Client(other) => other.to_string(),
        }
    }

    /// The `--json` error object for stderr: `{"ok":false,"error":{…}}`.
    pub fn envelope(&self) -> Value {
        let mut err = json!({
            "code": self.code().as_str(),
            "message": self.message(),
        });
        if let Self::Client(ClientError::Server(e)) = self {
            if let Some(p) = &e.path {
                err["path"] = json!(p);
            }
            if let Some(r) = e.current_rev {
                err["current_rev"] = json!(r);
            }
        }
        json!({"ok": false, "error": err})
    }
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        Self::Client(e)
    }
}

/// Print a failure to stderr: one JSON object with `--json`, `ds: …  [exit N]` otherwise.
pub fn print_failure(f: &Failure, json_mode: bool) {
    if json_mode {
        match serde_json::to_string_pretty(&f.envelope()) {
            Ok(s) => eprintln!("{s}"),
            Err(_) => eprintln!("ds: {}  [exit {}]", f.message(), f.exit_code()),
        }
    } else {
        eprintln!("ds: {}  [exit {}]", f.message(), f.exit_code());
    }
}

/// Print one JSON object on stdout (pretty, or one line with `compact`).
pub fn print_object(value: &Value, compact: bool) {
    let s = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    match s {
        Ok(s) => println!("{s}"),
        Err(_) => println!("{value:?}"),
    }
}

/// Render a value for humans: scalars plainly, objects as one `key: value` line per
/// entry (nested structures compact-JSON on the same line), arrays as `- item` lines.
pub fn render_human(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|v| format!("- {}", render_scalar_or_compact(v)))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}: {}", render_scalar_or_compact(v)))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn render_scalar_or_compact(v: &Value) -> String {
    match v {
        Value::Object(_) | Value::Array(_) => serde_json::to_string(v).unwrap_or_default(),
        other => render_human(other),
    }
}

/// A server-side error turned into a [`Failure`] (for client-side checks like the
/// `get --rev` mismatch, which maps to `E_CONFLICT` / exit 3).
pub fn server_failure(e: IpcError) -> Failure {
    Failure::Client(ClientError::Server(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ds_ipc::error::ErrorCode;

    #[test]
    fn exit_codes_are_the_contract_table() {
        // docs/contracts/cli.md §3 — pinned from day one (DS-016 risk note).
        let table = [
            (ErrorCode::EAuth, 4),
            (ErrorCode::EScope, 4),
            (ErrorCode::ECap, 4),
            (ErrorCode::EPath, 2),
            (ErrorCode::ESchema, 1),
            (ErrorCode::EConflict, 3),
            (ErrorCode::ELimit, 5),
            (ErrorCode::EUnreachable, 6),
            (ErrorCode::EDevice, 7),
            (ErrorCode::EVersion, 8),
            (ErrorCode::EInternal, 8),
        ];
        for (code, exit) in table {
            assert_eq!(code.exit_code(), exit, "{code:?}");
            let f = Failure::Client(ClientError::Server(IpcError::new(code, "m")));
            assert_eq!(f.exit_code(), exit, "{code:?}");
            assert_eq!(f.code(), code);
        }
        assert_eq!(Failure::Usage("x".into()).exit_code(), 1);
        assert_eq!(Failure::Usage("x".into()).code(), ErrorCode::ESchema);
    }

    #[test]
    fn unreachable_message_carries_the_daemon_start_hint() {
        let f = Failure::Client(ClientError::Unreachable {
            path: "/run/deckstack/daemon.sock".into(),
            hint: "start it with: ds daemon start".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no socket"),
        });
        assert_eq!(f.exit_code(), 6);
        assert_eq!(
            f.message(),
            "daemon unreachable at /run/deckstack/daemon.sock — start it with: ds daemon start"
        );
        let env = f.envelope();
        assert_eq!(env["ok"], json!(false));
        assert_eq!(env["error"]["code"], json!("E_UNREACHABLE"));
    }

    #[test]
    fn server_error_envelope_keeps_path_and_current_rev() {
        let e = IpcError::new(ErrorCode::EConflict, "if_rev mismatch, current_rev is 7")
            .with_current_rev(7)
            .with_path("/controls/keys/0");
        let f = Failure::Client(ClientError::Server(e));
        assert_eq!(f.exit_code(), 3);
        assert_eq!(f.message(), "if_rev mismatch, current_rev is 7");
        let env = f.envelope();
        assert_eq!(env["error"]["code"], json!("E_CONFLICT"));
        assert_eq!(env["error"]["current_rev"], json!(7));
        assert_eq!(env["error"]["path"], json!("/controls/keys/0"));
    }

    #[test]
    fn human_rendering_is_one_line_per_fact() {
        assert_eq!(render_human(&json!("Salon")), "Salon");
        assert_eq!(render_human(&json!(80)), "80");
        assert_eq!(render_human(&json!(true)), "true");
        assert_eq!(render_human(&json!(null)), "null");
        assert_eq!(
            render_human(&json!({"kind": "color", "value": "#3cc86e"})),
            "kind: color\nvalue: #3cc86e"
        );
        assert_eq!(
            render_human(&json!({"image": {"kind": "color", "value": "#3cc86e"}, "label": "K"})),
            "image: {\"kind\":\"color\",\"value\":\"#3cc86e\"}\nlabel: K"
        );
        // arrays
        assert_eq!(render_human(&json!([1, "a"])), "- 1\n- a");
        // nested objects stay compact on one line
        assert_eq!(
            render_human(&json!({"image": {"kind": "color"}})),
            "image: {\"kind\":\"color\"}"
        );
    }

    #[test]
    fn print_object_shapes() {
        print_object(&json!({"a": 1}), true);
        print_object(&json!({"a": 1}), false);
    }
}
