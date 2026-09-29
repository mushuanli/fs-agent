//! Query strings shared by the file endpoints.

use serde::Deserialize;

/// `?path=<export-relative>&cursor=<opaque>`
///
/// `cursor` is meaningful only for `entries`, but the same type is used for
/// `content` so an incidental `cursor` parameter is accepted rather than
/// rejected as an unknown field.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub path: String,
    pub cursor: Option<String>,
}
