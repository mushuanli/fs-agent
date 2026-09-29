//! Credential policy: which secret is configured, from where, and how strong.
//!
//! Exactly one of the four fields may be set. Inline values are accepted for
//! local use; `*_env` variants keep the secret out of the configuration file.
//! Nothing here logs or formats the resolved secret.

use crate::config::model::Config;

/// Minimum secret length per authentication scheme.
const TOKEN_MIN_BYTES: usize = 24;
const PASSWORD_MIN_BYTES: usize = 8;

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Token,
    Password,
}

impl Kind {
    fn min_bytes(self) -> usize {
        match self {
            Self::Token => TOKEN_MIN_BYTES,
            Self::Password => PASSWORD_MIN_BYTES,
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Token => formatter.write_str("token"),
            Self::Password => formatter.write_str("password"),
        }
    }
}

/// A validated credential, ready to become an [`crate::auth::Client`].
pub(crate) struct Credentials {
    pub secret: String,
    /// `None` for legacy Bearer tokens, which have no username.
    pub username: Option<String>,
}

pub(crate) fn resolve(config: &Config) -> Result<Credentials, String> {
    let token = secret(
        "token",
        config.token.as_deref(),
        config.token_env.as_deref(),
    )?;
    let password = secret(
        "password",
        config.password.as_deref(),
        config.password_env.as_deref(),
    )?;
    let (secret, username, kind) = match (token, password) {
        (Some(token), None) => (token, None, Kind::Token),
        (None, Some(password)) => (password, Some(username(config)?), Kind::Password),
        (None, None) => {
            return Err("set exactly one of token, token_env, password or password_env".to_owned())
        }
        (Some(_), Some(_)) => {
            return Err("token and password are mutually exclusive, choose one".to_owned())
        }
    };
    if secret.len() < kind.min_bytes() {
        return Err(format!(
            "{kind} must be at least {} bytes, got {}",
            kind.min_bytes(),
            secret.len()
        ));
    }
    Ok(Credentials { secret, username })
}

fn secret(
    field: &str,
    value: Option<&str>,
    environment: Option<&str>,
) -> Result<Option<String>, String> {
    match (value, environment) {
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(format!("{field} and {field}_env cannot both be set")),
        (Some(value), None) => Ok(Some(value.to_owned())),
        (None, Some(name)) => std::env::var(name)
            .map(Some)
            .map_err(|_| format!("environment variable {name:?} is not set")),
    }
}

/// Basic authentication needs a username; the environment is the fallback.
fn username(config: &Config) -> Result<String, String> {
    let username = config
        .username
        .clone()
        .or_else(|| std::env::var("FS_SERVER_USER").ok())
        .ok_or_else(|| {
            "username is required: set \"username\" or the FS_SERVER_USER environment variable"
                .to_owned()
        })?;
    if username.is_empty() || username.contains([':', '\r', '\n']) {
        return Err(format!(
            "invalid username {username:?}: must be non-empty and contain no ':'"
        ));
    }
    Ok(username)
}
