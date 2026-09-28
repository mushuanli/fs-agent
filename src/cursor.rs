use crate::error::{invalid, Error};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

#[derive(Serialize, Deserialize)]
struct Cursor {
    binding: String,
    last: String,
    expires: u64,
}
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn encode(key: &[u8], binding: &str, last: &str) -> String {
    let payload = serde_json::to_vec(&Cursor {
        binding: binding.into(),
        last: last.into(),
        expires: now() + 300,
    })
    .unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(&payload);
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&payload),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}
pub fn decode(key: &[u8], binding: &str, token: &str) -> Result<String, Error> {
    if token.len() > 16_384 {
        return Err(invalid());
    }
    let (payload, signature) = token.split_once('.').ok_or_else(invalid)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| invalid())?;
    let signature = URL_SAFE_NO_PAD.decode(signature).map_err(|_| invalid())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
    mac.update(&payload);
    mac.verify_slice(&signature).map_err(|_| invalid())?;
    let cursor: Cursor = serde_json::from_slice(&payload).map_err(|_| invalid())?;
    if cursor.binding != binding || cursor.expires < now() {
        return Err(invalid());
    }
    Ok(cursor.last)
}
