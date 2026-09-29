//! The single failure vocabulary of the service.
//!
//! Every layer reports problems as an [`Error`]: a stable machine-readable
//! `code` plus the HTTP status the transport should use. The mapping from
//! `std::io`/`rustix` failures lives here so no other module needs to know how
//! kernel error numbers translate into protocol responses.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// A failure carrying a stable code and the status that represents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
}

impl Error {
    pub const fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    /// Input that is syntactically wrong (bad path, bad header, bad action).
    pub const fn invalid() -> Self {
        Self::new(StatusCode::BAD_REQUEST, "EINVAL")
    }

    /// The request carried no usable credentials or the wrong ones.
    pub const fn unauthenticated() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "EACCES")
    }

    /// The identity is known but not allowed to do this.
    pub const fn forbidden(code: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code)
    }

    pub const fn not_found(code: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, code)
    }

    pub const fn conflict(code: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code)
    }

    /// The export is busy with a command or another operation.
    pub const fn busy() -> Self {
        Self::new(StatusCode::CONFLICT, "EBUSY")
    }

    pub const fn precondition_failed() -> Self {
        Self::new(StatusCode::PRECONDITION_FAILED, "ECONFLICT")
    }

    pub const fn precondition_required(code: &'static str) -> Self {
        Self::new(StatusCode::PRECONDITION_REQUIRED, code)
    }

    /// The caller asked to stop; committed effects are never rolled back.
    pub const fn cancelled() -> Self {
        Self::new(StatusCode::REQUEST_TIMEOUT, "ECANCELLED")
    }

    pub const fn timed_out() -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, "ETIMEDOUT")
    }

    /// A resource the caller can retry later (worker pool exhausted).
    pub const fn unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "EIO")
    }

    pub const fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "EIO")
    }

    /// A well-formed request for something this build does not implement.
    pub const fn unsupported() -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "ECAPABILITY")
    }

    pub const fn too_large(code: &'static str) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, code)
    }

    pub const fn too_many(code: &'static str) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code)
    }

    pub const fn range_unsatisfiable() -> Self {
        Self::new(StatusCode::RANGE_NOT_SATISFIABLE, "EINVAL")
    }

    /// Whether this is "the entry does not exist".
    pub fn is_not_found(&self) -> bool {
        self.status == StatusCode::NOT_FOUND
    }

    /// Whether this is "the node exists but this protocol does not support it".
    pub fn is_unsupported(&self) -> bool {
        self.status == StatusCode::UNPROCESSABLE_ENTITY
    }

    pub fn is_range_unsatisfiable(&self) -> bool {
        self.status == StatusCode::RANGE_NOT_SATISFIABLE
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "code": self.code }))).into_response()
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match error.kind() {
            NotFound => Self::not_found("ENOENT"),
            AlreadyExists => Self::conflict("EEXIST"),
            DirectoryNotEmpty => Self::conflict("ENOTEMPTY"),
            PermissionDenied => Self::forbidden("EACCES"),
            // A component longer than NAME_MAX is client input, not a server fault.
            InvalidFilename => Self::invalid(),
            _ => Self::internal(),
        }
    }
}

impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        use rustix::io::Errno;
        match error {
            Errno::LOOP | Errno::XDEV | Errno::ACCESS => Self::forbidden("EACCES"),
            // A read-only filesystem or a mount boundary is not a server fault.
            Errno::ROFS => Self::forbidden("EROFS"),
            Errno::NOTDIR => Self::new(StatusCode::BAD_REQUEST, "ENOTDIR"),
            Errno::ISDIR => Self::conflict("EISDIR"),
            // The client can act on "no space left" and "quota exceeded".
            Errno::NOSPC | Errno::DQUOT => Self::new(StatusCode::INSUFFICIENT_STORAGE, "ENOSPC"),
            // A component longer than NAME_MAX is client input, not a server fault.
            Errno::NAMETOOLONG => Self::invalid(),
            _ => std::io::Error::from(error).into(),
        }
    }
}
