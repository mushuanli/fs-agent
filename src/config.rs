use crate::filesystem::Export;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen: String,
    pub server_id: Option<String>,
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportConfig {
    pub path: String,
    pub alias: Option<String>,
    #[serde(default)]
    pub access: Access,
}
#[derive(Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    Ro,
    Rw,
}
pub struct Client {
    pub token: String,
    pub username: Option<String>,
    pub exports: Vec<String>,
    pub write_exports: Vec<String>,
}
pub struct State {
    pub server_id: Option<String>,
    pub exports: BTreeMap<String, Arc<Export>>,
    pub clients: Vec<Client>,
    pub workers: Arc<tokio::sync::Semaphore>,
    pub cursor_key: [u8; 32],
    pub operations: crate::operations::Operations,
}

impl Config {
    pub fn state(&self) -> Result<Arc<State>, String> {
        if let Some(id) = &self.server_id {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(
                    "server_id must contain 1..128 ASCII letters, digits, '-' or '_'".into(),
                );
            }
        }
        let exports = self.open_exports()?;
        let client = self.client(&exports)?;
        let mut cursor_key = [0; 32];
        getrandom::getrandom(&mut cursor_key)
            .map_err(|_| "Random source unavailable".to_owned())?;
        Ok(Arc::new(State {
            server_id: self.server_id.clone(),
            exports,
            clients: vec![client],
            cursor_key,
            operations: Default::default(),
            workers: Arc::new(tokio::sync::Semaphore::new(16)),
        }))
    }

    fn open_exports(&self) -> Result<BTreeMap<String, Arc<Export>>, String> {
        if self.exports.is_empty() {
            return Err("no exports configured: add at least one [[exports]] section".to_owned());
        }
        let mut exports = BTreeMap::new();
        let mut roots: Vec<(String, PathBuf)> = Vec::new();
        for (index, cfg) in self.exports.iter().enumerate() {
            let alias = cfg.alias(index)?;
            if exports.contains_key(&alias) {
                return Err(format!(
                    "exports[{index}]: alias {alias:?} is already defined"
                ));
            }
            let root = std::fs::canonicalize(&cfg.path)
                .map_err(|_| format!("exports[{index}]: path {:?} does not exist", cfg.path))?;
            if let Some((other, _)) = roots
                .iter()
                .find(|(_, path)| root.starts_with(path) || path.starts_with(&root))
            {
                return Err(format!(
                    "exports[{index}]: path {:?} overlaps export {other:?}",
                    cfg.path
                ));
            }
            roots.push((alias.clone(), root));
            exports.insert(alias, Arc::new(cfg.open(index)?));
        }
        Ok(exports)
    }

    fn client(&self, exports: &BTreeMap<String, Arc<Export>>) -> Result<Client, String> {
        let token = resolve_secret("token", self.token.as_deref(), self.token_env.as_deref())?;
        let password = resolve_secret(
            "password",
            self.password.as_deref(),
            self.password_env.as_deref(),
        )?;
        let (secret, username, kind) = match (token, password) {
            (Some(token), None) => (token, None, Kind::Token),
            (None, Some(password)) => (password, Some(self.username()?), Kind::Password),
            (None, None) => {
                return Err(
                    "set exactly one of token, token_env, password or password_env".to_owned(),
                )
            }
            (Some(_), Some(_)) => {
                return Err("token and password are mutually exclusive, choose one".to_owned())
            }
        };
        if secret.len() < kind.min_len() {
            return Err(format!(
                "{kind} must be at least {} bytes, got {}",
                kind.min_len(),
                secret.len()
            ));
        }
        Ok(Client {
            token: secret,
            username,
            exports: exports.keys().cloned().collect(),
            write_exports: exports
                .iter()
                .filter(|(_, export)| export.writable())
                .map(|(alias, _)| alias.clone())
                .collect(),
        })
    }

    fn username(&self) -> Result<String, String> {
        let username = self
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
}

impl ExportConfig {
    fn alias(&self, index: usize) -> Result<String, String> {
        let alias = match &self.alias {
            Some(alias) => alias.clone(),
            None => Path::new(&self.path)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!(
                        "exports[{index}]: cannot derive alias from path {:?}, set \"alias\"",
                        self.path
                    )
                })?,
        };
        if !valid_alias(&alias) {
            return Err(format!(
                "exports[{index}]: alias {alias:?} must be non-empty and use only ASCII letters, digits, '_' or '-'; set \"alias\" explicitly"
            ));
        }
        Ok(alias)
    }

    fn open(&self, index: usize) -> Result<Export, String> {
        let opened = match self.access {
            Access::Ro => Export::open(&self.path),
            Access::Rw => Export::exclusive(&self.path),
        };
        opened.map_err(|_| format!("exports[{index}]: cannot open or lock path {:?}", self.path))
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Token,
    Password,
}
impl Kind {
    fn min_len(self) -> usize {
        match self {
            Kind::Token => 24,
            Kind::Password => 8,
        }
    }
}
impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Kind::Token => formatter.write_str("token"),
            Kind::Password => formatter.write_str("password"),
        }
    }
}

fn resolve_secret(
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

fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
