//! Authenticated pagination cursors.
//!
//! A cursor is an HMAC-signed `(binding, last, expires)` tuple. The binding
//! pins it to one identity, alias and directory, so a cursor cannot be
//! replayed against a different listing.

use crate::core::error::Error;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Shortest lifetime that still allows a client to walk a large directory.
const LIFETIME_SECONDS: u64 = 300;
const MAX_TOKEN_BYTES: usize = 16_384;

#[derive(Serialize, Deserialize)]
struct Cursor {
    binding: String,
    last: String,
    expires: u64,
}

pub fn encode(key: &[u8], binding: &str, last: &str) -> String {
    let payload = serde_json::to_vec(&Cursor {
        binding: binding.into(),
        last: last.into(),
        expires: now() + LIFETIME_SECONDS,
    })
    .unwrap_or_default();
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&payload);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

pub fn decode(key: &[u8], binding: &str, token: &str) -> Result<String, Error> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(Error::invalid());
    }
    let (payload, signature) = token.split_once('.').ok_or_else(Error::invalid)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| Error::invalid())?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| Error::invalid())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&payload);
    mac.verify_slice(&signature).map_err(|_| Error::invalid())?;
    let cursor: Cursor = serde_json::from_slice(&payload).map_err(|_| Error::invalid())?;
    if cursor.binding != binding || cursor.expires < now() {
        return Err(Error::invalid());
    }
    Ok(cursor.last)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
