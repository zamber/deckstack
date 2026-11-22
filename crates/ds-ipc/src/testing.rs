//! Test support: a [`MockHandler`] implementing a tiny in-memory state (path → value +
//! revision) so the server can be round-tripped in-process, plus helpers to create a
//! token dir. **Not part of the daemon** — `ds-ipc` itself stays transport-only; this
//! module exists so `ds-ipc` and `ds-client` tests (and examples) can run a real socket
//! end-to-end without `ds-daemon`.

use crate::auth::{Scope, TokenSet};
use crate::error::{ErrorCode, IpcError};
use crate::message::{Request, Response};
use crate::server::{Connection, Handler};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Create `<dir>/token` (install token) + `tokens.json` and return the install token.
pub fn write_token_dir(dir: &Path) -> std::io::Result<String> {
    let token = crate::auth::generate_token()?;
    TokenSet::create(dir, &token)?;
    Ok(token)
}

/// Issue a scoped token into `<dir>/tokens.json`; returns the token string.
pub fn issue_scoped(dir: &Path, id: &str, scope: Scope) -> std::io::Result<String> {
    let token = crate::auth::generate_token()?;
    TokenSet::issue(dir, id, &token, scope)?;
    Ok(token)
}

/// A minimal in-memory op handler: `ping`, `get`, `set`, `subscribe` (with event push on
/// `set`), `daemon status`, `token list`. Everything else → `E_PATH`.
pub struct MockHandler {
    state: Mutex<HashMap<String, Value>>,
    rev: Mutex<u64>,
    subs: Mutex<Vec<(Connection, String)>>,
}

impl MockHandler {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
            rev: Mutex::new(1),
            subs: Mutex::new(Vec::new()),
        }
    }

    fn get_path(req: &Request) -> Option<&str> {
        req.get("path").and_then(Value::as_str)
    }

    fn matches(sub: &str, path: &str) -> bool {
        match sub.strip_suffix('*') {
            Some(prefix) => path.starts_with(prefix),
            None => sub == path,
        }
    }
}

impl Default for MockHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl Handler for MockHandler {
    fn rev(&self) -> u64 {
        *self.rev.lock().unwrap()
    }

    fn handle(&self, conn: &Connection, req: &Request) -> Response {
        let id = req.id();
        match req.op() {
            "ping" => Response::ok(id, json!({"pong": true})),
            "get" => {
                let Some(path) = Self::get_path(req) else {
                    return Response::error_code(id, ErrorCode::EPath, "missing path");
                };
                let state = self.state.lock().unwrap();
                match state.get(path) {
                    Some(v) => Response::ok_rev(id, self.rev(), v.clone()),
                    None => Response::error(
                        id,
                        IpcError::new(ErrorCode::EPath, format!("path not found: {path}"))
                            .with_path(path),
                    ),
                }
            }
            "set" => {
                let ops = req.get("ops").and_then(Value::as_array).cloned();
                let Some(ops) = ops else {
                    return Response::error_code(id, ErrorCode::ESchema, "set needs ops");
                };
                let if_rev = req.get("if_rev").and_then(Value::as_u64);
                let mut state = self.state.lock().unwrap();
                if let Some(want) = if_rev {
                    let cur = *self.rev.lock().unwrap();
                    if want != cur {
                        return Response::error(
                            id,
                            IpcError::new(
                                ErrorCode::EConflict,
                                format!("if_rev mismatch, current_rev is {cur}"),
                            )
                            .with_current_rev(cur),
                        );
                    }
                }
                for op in &ops {
                    let path = op.get("path").and_then(Value::as_str).unwrap_or("");
                    match op.get("op").and_then(Value::as_str) {
                        Some("set") => {
                            let value = op.get("value").cloned().unwrap_or(Value::Null);
                            state.insert(path.to_string(), value);
                        }
                        Some("delete") => {
                            state.remove(path);
                        }
                        _ => {}
                    }
                }
                drop(state);
                let mut rev = self.rev.lock().unwrap();
                *rev += 1;
                let new_rev = *rev;
                drop(rev);
                // push events to matching subscribers
                let subs = self.subs.lock().unwrap().clone();
                for (c, pattern) in &subs {
                    for (path, value) in self.state.lock().unwrap().iter() {
                        if Self::matches(pattern, path) {
                            let _ = c.send_event(&json!({
                                "op": "event",
                                "path": path,
                                "rev": new_rev,
                                "value": value,
                            }));
                        }
                    }
                }
                Response::ok_rev(id, new_rev, Value::Null)
            }
            "subscribe" => {
                let Some(path) = Self::get_path(req) else {
                    return Response::error_code(id, ErrorCode::EPath, "missing path");
                };
                conn.set_idle_exempt(true);
                self.subs
                    .lock()
                    .unwrap()
                    .push((conn.clone(), path.to_string()));
                Response::ok(id, json!({"subscribed": path}))
            }
            "unsubscribe" => {
                let Some(path) = Self::get_path(req) else {
                    return Response::error_code(id, ErrorCode::EPath, "missing path");
                };
                self.subs.lock().unwrap().retain(|(_, p)| p != path);
                Response::ok(id, json!({"unsubscribed": path}))
            }
            "daemon" => Response::ok(id, json!({"running": true, "rev": self.rev()})),
            "token" => Response::ok(id, json!({"tokens": [], "install": "admin"})),
            other => Response::error(
                id,
                IpcError::new(ErrorCode::EPath, format!("mock: no such op {other}"))
                    .with_path(other),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_dir_helpers() {
        let dir = tempfile::tempdir().unwrap();
        let install = write_token_dir(dir.path()).unwrap();
        assert_eq!(install.len(), 64);
        let read_tok = issue_scoped(dir.path(), "t0001", Scope::Read).unwrap();
        let set = TokenSet::load(dir.path()).unwrap();
        assert_eq!(set.scope_of(&install), Some(Scope::Admin));
        assert_eq!(set.scope_of(&read_tok), Some(Scope::Read));
        assert_eq!(set.scope_of("nope"), None);
    }
}
