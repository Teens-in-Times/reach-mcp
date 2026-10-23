//! Stable operation failures, distinct from transport errors and running tasks.
use serde::{Deserialize, Serialize};
use std::fmt;
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    InvalidArguments,
    PermissionDenied,
    NotFound,
    Conflict,
    AmbiguousMatch,
    UnsupportedEncoding,
    CapacityExceeded,
    InvalidState,
    CursorInvalid,
    Io,
    Internal,
    ExecutionFailed,
    Interrupted,
    DurabilityLost,
    OutputExpired,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Failure {
    pub code: Code,
    pub message: String,
    pub recovery: String,
}
pub type Result<T> = std::result::Result<T, Failure>;
impl Failure {
    pub fn new(code: Code, message: impl Into<String>, recovery: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: recovery.into(),
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(
            Code::InvalidArguments,
            message,
            "Correct arguments using Discover. If a task_id was returned, inspect that task before submitting different work.",
        )
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(
            Code::Conflict,
            message,
            "Read the current file, reconcile changes, then submit its current SHA-256.",
        )
    }
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Failure {}
impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => Code::NotFound,
            std::io::ErrorKind::PermissionDenied => Code::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => Code::Conflict,
            _ => Code::Io,
        };
        Self::new(
            code,
            e.to_string(),
            "Inspect the path and OS permissions before retrying.",
        )
    }
}
impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        if let Some(f) = e.downcast_ref::<Failure>() {
            return f.clone();
        }
        let code = e
            .downcast_ref::<std::io::Error>()
            .map_or(Code::Internal, |io| match io.kind() {
                std::io::ErrorKind::NotFound => Code::NotFound,
                std::io::ErrorKind::PermissionDenied => Code::PermissionDenied,
                _ => Code::Io,
            });
        Self::new(
            code,
            format!("{e:#}"),
            "Inspect current state and logs before repeating a mutating operation.",
        )
    }
}
