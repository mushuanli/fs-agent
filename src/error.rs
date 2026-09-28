use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub struct Error(pub StatusCode, pub &'static str);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"code": self.1}))).into_response()
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        use std::io::ErrorKind::*;
        match error.kind() {
            NotFound => Self(StatusCode::NOT_FOUND, "ENOENT"),
            AlreadyExists => Self(StatusCode::CONFLICT, "EEXIST"),
            DirectoryNotEmpty => Self(StatusCode::CONFLICT, "ENOTEMPTY"),
            PermissionDenied => Self(StatusCode::FORBIDDEN, "EACCES"),
            _ => Self(StatusCode::INTERNAL_SERVER_ERROR, "EIO"),
        }
    }
}
impl From<rustix::io::Errno> for Error {
    fn from(error: rustix::io::Errno) -> Self {
        use rustix::io::Errno;
        match error {
            Errno::LOOP | Errno::XDEV | Errno::ACCESS => Self(StatusCode::FORBIDDEN, "EACCES"),
            Errno::NOTDIR => Self(StatusCode::BAD_REQUEST, "ENOTDIR"),
            _ => std::io::Error::from(error).into(),
        }
    }
}
pub fn invalid() -> Error {
    Error(StatusCode::BAD_REQUEST, "EINVAL")
}
