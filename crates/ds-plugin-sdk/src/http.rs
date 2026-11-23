//! The bounded host-mediated HTTP helper (S4/M5, `docs/contracts/plugins.md` §2).
//!
//! Plugins **never open sockets**: `Sdk::http` sends `{"op":"request","kind":"net",…}` to the
//! daemon, which resolves the host, matches it against the plugin's granted `net:<glob>` and
//! `/policy/net/allow_hosts`, performs the request itself, and replies
//! `{"op":"reply","ok":true,"status":200,"body":"…"}`. The SDK bounds what it will ask for
//! (URL length, method, headers, body, reply size) so a buggy plugin cannot ask the host for
//! something outside the documented envelope.

use serde_json::Value;

use crate::error::{ErrorCode, PluginError};

/// Max length of the request URL (chars). The host additionally caps its own request size.
pub const MAX_NET_URL_CHARS: usize = 2048;

/// Max length of the request body (bytes of its JSON encoding).
pub const MAX_NET_BODY_BYTES: usize = 1024 * 1024;

/// Max number of request headers.
pub const MAX_NET_HEADERS: usize = 16;

/// Max accepted reply body (chars). The host caps bodies at its own `MAX_BODY` (16 MiB
/// precedent); the SDK refuses anything beyond this bound with `E_LIMIT`.
pub const MAX_NET_REPLY_CHARS: usize = 4 * 1024 * 1024;

/// HTTP methods the SDK will send. The host may allowlist further.
pub const ALLOWED_METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];

/// A host-mediated HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    /// HTTP status code from the daemon's own request.
    pub status: u16,
    /// Response body as returned by the host (a JSON string on the wire).
    pub body: String,
}

impl HttpResponse {
    /// The body parsed as JSON, if it is valid JSON.
    pub fn body_json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// Validate method/URL/headers/body and return the normalized method.
pub(crate) fn check_request(
    method: &str,
    url: &str,
    body: Option<&Value>,
    headers: Option<&serde_json::Map<String, Value>>,
) -> Result<String, PluginError> {
    let method = method.to_ascii_uppercase();
    if !ALLOWED_METHODS.contains(&method.as_str()) {
        return Err(PluginError::new(
            ErrorCode::ESchema,
            format!("http method {method:?} is not one of {ALLOWED_METHODS:?}"),
        ));
    }
    if url.is_empty() || !url.contains("://") {
        return Err(PluginError::schema(format!(
            "http url {url:?} is not absolute"
        )));
    }
    if url.chars().count() > MAX_NET_URL_CHARS {
        return Err(PluginError::limit(format!(
            "http url longer than {MAX_NET_URL_CHARS} chars"
        )));
    }
    if let Some(b) = body {
        let len = serde_json::to_string(b).map(|s| s.len()).unwrap_or(0);
        if len > MAX_NET_BODY_BYTES {
            return Err(PluginError::limit(format!(
                "http body larger than {MAX_NET_BODY_BYTES} bytes"
            )));
        }
    }
    if let Some(h) = headers {
        if h.len() > MAX_NET_HEADERS {
            return Err(PluginError::limit(format!(
                "more than {MAX_NET_HEADERS} http headers"
            )));
        }
    }
    Ok(method)
}

/// Check a reply body against the SDK bound.
pub(crate) fn check_reply_body(body: &str) -> Result<(), PluginError> {
    if body.chars().count() > MAX_NET_REPLY_CHARS {
        return Err(PluginError::limit(format!(
            "net reply body larger than {MAX_NET_REPLY_CHARS} chars"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn method_and_url_bounds() {
        assert_eq!(
            check_request("get", "http://lights.lan/api/lamp", None, None).unwrap(),
            "GET"
        );
        assert_eq!(
            check_request("POST", "http://x/y", None, None).unwrap(),
            "POST"
        );
        assert!(check_request("CONNECT", "http://x", None, None).is_err());
        assert!(check_request("GET", "lights.lan/api", None, None).is_err());
        let long = format!("http://x/{}", "a".repeat(MAX_NET_URL_CHARS));
        let e = check_request("GET", &long, None, None).unwrap_err();
        assert_eq!(e.code, ErrorCode::ELimit);
    }

    #[test]
    fn body_and_header_bounds() {
        let big = Value::String("a".repeat(MAX_NET_BODY_BYTES));
        assert_eq!(
            check_request("POST", "http://x/y", Some(&big), None)
                .unwrap_err()
                .code,
            ErrorCode::ELimit
        );
        let mut headers = serde_json::Map::new();
        for i in 0..(MAX_NET_HEADERS + 1) {
            headers.insert(format!("h{i}"), json!(1));
        }
        assert_eq!(
            check_request("GET", "http://x/y", None, Some(&headers))
                .unwrap_err()
                .code,
            ErrorCode::ELimit
        );
    }

    #[test]
    fn reply_body_bound_and_json_helper() {
        let r = HttpResponse {
            status: 200,
            body: r#"{"state":"on"}"#.into(),
        };
        assert_eq!(r.body_json(), Some(json!({"state":"on"})));
        let big = "a".repeat(MAX_NET_REPLY_CHARS + 1);
        assert_eq!(check_reply_body(&big).unwrap_err().code, ErrorCode::ELimit);
    }
}
