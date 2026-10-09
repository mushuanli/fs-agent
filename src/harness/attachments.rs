//! Inline attachments never expose a host path or fetch a caller-supplied URL.
use crate::core::error::Error;
use base64::Engine;
use serde_json::{json, Value};

pub fn input(prompt: &str, attachments: &Value) -> Result<Value, Error> {
    let mut input = vec![json!({"type":"text","text":prompt})];
    if attachments.is_null() {
        return Ok(json!(input));
    }
    let files = attachments
        .as_array()
        .filter(|files| files.len() <= 5)
        .ok_or_else(Error::invalid)?;
    let mut bytes = 0;
    for file in files {
        let content = file["content"].as_str().ok_or_else(Error::invalid)?;
        bytes += content.len();
        if bytes > 512 * 1024 {
            return Err(Error::invalid());
        }
        append(&mut input, file, content)?;
    }
    Ok(json!(input))
}
fn name(file: &Value) -> Result<&str, Error> {
    file["name"]
        .as_str()
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 256
                && !name
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '/' | '\\'))
        })
        .ok_or_else(Error::invalid)
}
fn append(input: &mut Vec<Value>, file: &Value, content: &str) -> Result<(), Error> {
    let name = name(file)?;
    match file["kind"].as_str() {
        Some("text")
            if content.len() <= 64 * 1024
                && !content
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
        {
            input.push(json!({"type":"text","text":format!("Attachment: {name}\n{content}")}));
        }
        Some("image") => {
            validate_image(file)?;
            input.push(json!({"type":"text","text":format!("Image attachment: {name}")}));
            input.push(json!({"type":"image","url":content}));
        }
        _ => return Err(Error::invalid()),
    }
    Ok(())
}

fn validate_image(file: &Value) -> Result<(), Error> {
    let mime = file["mimeType"]
        .as_str()
        .filter(|mime| matches!(*mime, "image/png" | "image/jpeg" | "image/webp"))
        .ok_or_else(Error::invalid)?;
    let prefix = format!("data:{mime};base64,");
    let encoded = file["content"]
        .as_str()
        .and_then(|content| content.strip_prefix(&prefix))
        .ok_or_else(Error::invalid)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid())?;
    if bytes.len() > 256 * 1024
        || !match mime {
            "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
            "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff"),
            "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
            _ => false,
        }
    {
        return Err(Error::invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inline_inputs_preserve_names_text_and_images_without_paths_or_remote_urls() {
        let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture");
        let files = json!([{"kind":"text","name":"notes.md","content":"quoted \"text\"\n中文"},
            {"kind":"image","name":"image.png","mimeType":"image/png","content":format!("data:image/png;base64,{png}")}]);
        let result = input("Review", &files).unwrap();
        assert_eq!(
            result[1]["text"],
            "Attachment: notes.md\nquoted \"text\"\n中文"
        );
        assert_eq!(result[3]["type"], "image");
        assert!(input("x", &json!([{"kind":"image","name":"image.png","mimeType":"image/png","content":"http://localhost/private"}])).is_err());
        assert!(input(
            "x",
            &json!([{"kind":"text","name":"../../auth","content":"x"}])
        )
        .is_err());
        assert!(input(
            "x",
            &json!([{"kind":"text","name":"blob","content":"x\u{0}"}])
        )
        .is_err());
        assert!(input(
            "x",
            &json!([{"kind":"text","name":"large","content":"x".repeat(65537)}])
        )
        .is_err());
    }
}
