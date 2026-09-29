//! The declarative configuration file, mapped one-to-one onto TOML.
//!
//! These types carry no behavior beyond deserialization: validation and
//! wiring live in [`crate::config::credentials`], [`crate::config::exports`]
//! and [`crate::app::State`].

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub log_level: crate::core::events::Level,
    pub listen: String,
    /// Optional stable identity; otherwise an ephemeral node identity is generated.
    pub server_id: Option<String>,
    #[serde(default = "execution_enabled")]
    pub execution: bool,
    /// Browser origins allowed to read responses (CORS), not an auth control.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_env: Option<String>,
    pub token: Option<String>,
    pub token_env: Option<String>,
    #[serde(default)]
    pub exports: Vec<ExportConfig>,
}

fn execution_enabled() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub path: String,
    /// Client-visible prefix; defaults to the final path component.
    pub alias: Option<String>,
    #[serde(default)]
    pub access: Access,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Serve reads only; no lock, no revisions.
    #[default]
    Ro,
    /// Serve reads and conditional writes behind an exclusive advisory lock.
    Rw,
}
