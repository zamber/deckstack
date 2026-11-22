//! Errors returned by [`crate::Store`] and the path/document helpers.
//!
//! `code()` maps to the IPC error codes of `docs/contracts/ipc.md` §4 (`E_PATH`,
//! `E_SCHEMA`, `E_CONFLICT`, `E_INTERNAL`) so the daemon can translate them 1:1.

use std::fmt;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, StateError>;

/// Everything that can go wrong while reading or mutating the state document.
#[derive(Debug)]
pub enum StateError {
    /// `E_PATH` — path not found, bad syntax, or outside the documented vocabulary
    /// (`docs/contracts/state.md` §1).
    Path(String),
    /// `E_SCHEMA` — the document (or a mutated copy of it) violates `schemas/state.json`.
    Schema(String),
    /// `E_CONFLICT` — `if_rev` did not match; carries the current revision.
    Conflict { current_rev: u64 },
    /// `E_INTERNAL` — I/O failure or a bug (corrupt store, missing `/meta`, …).
    Internal(String),
}

impl StateError {
    /// The IPC error code for this error (`docs/contracts/ipc.md` §4).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Path(_) => "E_PATH",
            Self::Schema(_) => "E_SCHEMA",
            Self::Conflict { .. } => "E_CONFLICT",
            Self::Internal(_) => "E_INTERNAL",
        }
    }
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(m) => write!(f, "E_PATH: {m}"),
            Self::Schema(m) => write!(f, "E_SCHEMA: {m}"),
            Self::Conflict { current_rev } => {
                write!(
                    f,
                    "E_CONFLICT: if_rev mismatch, current_rev is {current_rev}"
                )
            }
            Self::Internal(m) => write!(f, "E_INTERNAL: {m}"),
        }
    }
}

impl std::error::Error for StateError {}

impl From<std::io::Error> for StateError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
