//! Authentication and authorization policy.
//!
//! The service has a single configured principal, but the checks are written
//! per client so a second one can be added without touching the handlers:
//!
//! * **Authentication** turns an `Authorization` header into an identity.
//!   A password client uses HTTP Basic and never falls back to Bearer; a token
//!   client uses Bearer and has no username.
//! * **Authorization** decides whether that identity may read or write an
//!   export alias. The server never trusts a client's own conclusion.

use crate::core::error::Error;
use axum::http::HeaderMap;
use base64::{engine::general_purpose::STANDARD, Engine};

/// One authenticated principal and the aliases it may reach.
pub struct Client {
    secret: String,
    username: Option<String>,
    exports: Vec<String>,
    write_exports: Vec<String>,
}

impl Client {
    pub fn new(
        secret: String,
        username: Option<String>,
        exports: Vec<String>,
        write_exports: Vec<String>,
    ) -> Self {
        Self {
            secret,
            username,
            exports,
            write_exports,
        }
    }

    /// Whether this client may read `alias`.
    pub fn may_read(&self, alias: &str) -> bool {
        self.exports.iter().any(|candidate| candidate == alias)
    }

    /// Whether this client may write `alias`.
    pub fn may_write(&self, alias: &str) -> bool {
        self.write_exports
            .iter()
            .any(|candidate| candidate == alias)
    }

    /// Whether this client may write anywhere.
    pub fn can_write(&self) -> bool {
        !self.write_exports.is_empty()
    }

    /// Aliases visible to this client, in configuration order.
    pub fn exports(&self) -> &[String] {
        &self.exports
    }

    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    /// The configured secret. Exposed for configuration tests and diagnostics;
    /// it must never be logged or included in a response.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Compare the presented header against this client without early exit.
    fn matches(&self, authorization: &str, basic: Option<(&str, &str)>) -> bool {
        match (&self.username, basic) {
            (Some(username), Some((name, password))) => {
                equal(username.as_bytes(), name.as_bytes())
                    & equal(self.secret.as_bytes(), password.as_bytes())
            }
            // Token clients accept Bearer only; Basic never authenticates them.
            (None, _) => split_scheme(authorization).is_some_and(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("bearer")
                    && equal(self.secret.as_bytes(), token.as_bytes())
            }),
            _ => false,
        }
    }
}

/// The configured identities plus the installation identity they belong to.
pub struct Auth {
    server_id: Option<String>,
    clients: Vec<Client>,
}

impl Auth {
    pub fn new(server_id: Option<String>, clients: Vec<Client>) -> Self {
        Self { server_id, clients }
    }

    /// Stable installation identity, advertised through `/v1/capabilities`.
    pub fn server_id(&self) -> Option<&str> {
        self.server_id.as_deref()
    }

    pub fn clients(&self) -> &[Client] {
        &self.clients
    }

    /// Resolve the request identity, or `EACCES`.
    pub fn identify(&self, headers: &HeaderMap) -> Result<usize, Error> {
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(Error::unauthenticated)?;
        // RFC 7235: the scheme name is case-insensitive.
        let basic = split_scheme(authorization)
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
            .and_then(|(_, value)| STANDARD.decode(value).ok())
            .and_then(|bytes| String::from_utf8(bytes).ok());
        let credentials = basic.as_deref().and_then(|value| value.split_once(':'));
        self.clients
            .iter()
            .position(|client| client.matches(authorization, credentials))
            .ok_or_else(Error::unauthenticated)
    }

    /// The client behind an identity returned by [`Self::identify`].
    pub fn client(&self, identity: usize) -> &Client {
        &self.clients[identity]
    }

    /// [`Self::identify`] followed by a read check on `alias`.
    pub fn authorize(&self, identity: usize, alias: &str) -> Result<&Client, Error> {
        let client = self.client(identity);
        if !client.may_read(alias) {
            return Err(Error::forbidden("EACCES"));
        }
        Ok(client)
    }
}

/// Split an `Authorization` header into its scheme and credentials.
fn split_scheme(authorization: &str) -> Option<(&str, &str)> {
    authorization.split_once(' ')
}

/// Length-independent, branch-free comparison of two secrets.
fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", value.parse().unwrap());
        headers
    }

    fn password_client() -> Auth {
        Auth::new(
            None,
            vec![Client::new(
                "password-at-least-8-bytes".into(),
                Some("alice".into()),
                vec!["docs".into()],
                vec![],
            )],
        )
    }

    #[test]
    fn password_clients_require_basic_and_the_right_username() {
        let auth = password_client();
        let message = |value: &str| format!("Basic {}", STANDARD.encode(value));
        assert!(auth
            .identify(&headers(&message("alice:password-at-least-8-bytes")))
            .is_ok());
        assert!(auth
            .identify(&headers(&message("bob:password-at-least-8-bytes")))
            .is_err());
        assert!(auth.identify(&headers(&message("alice:wrong"))).is_err());
        assert!(auth
            .identify(&headers("Bearer password-at-least-8-bytes"))
            .is_err());
        assert!(auth.identify(&headers("Basic not-base64!")).is_err());
        assert!(auth.identify(&HeaderMap::new()).is_err());
    }

    #[test]
    fn token_clients_require_bearer() {
        let auth = Auth::new(
            None,
            vec![Client::new(
                "token-at-least-24-bytes-long".into(),
                None,
                vec!["docs".into()],
                vec![],
            )],
        );
        assert!(auth
            .identify(&headers("Bearer token-at-least-24-bytes-long"))
            .is_ok());
        assert!(auth
            .identify(&headers("Bearer token-at-least-24-bytes-lonG"))
            .is_err());
        assert!(auth
            .identify(&headers(&format!(
                "Basic {}",
                STANDARD.encode(":token-at-least-24-bytes-long")
            )))
            .is_err());
    }

    #[test]
    fn authorization_is_scoped_to_the_alias() {
        let auth = password_client();
        assert!(auth.authorize(0, "docs").is_ok());
        assert!(auth.authorize(0, "other").is_err());
        assert!(!auth.client(0).may_write("docs"));
    }
}
