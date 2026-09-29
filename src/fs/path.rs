//! Path policy for everything reachable through an export.
//!
//! Paths on the wire are always *relative to an export root*. The rules are
//! deliberately textual and strict, but they must not be stricter than the
//! client contract: a colon is a legal character in a name (for example
//! `2024:Q1.md`), while a leading Windows drive prefix is not, because it
//! would let one platform's absolute path masquerade as a relative one.
//!
//! Resolution to a file descriptor happens afterwards in [`crate::fs::export`]
//! with `openat2` and `RESOLVE_BENEATH`, so text is the first gate, never the
//! only one.

use crate::core::error::Error;

/// Longest accepted wire path.
pub const MAX_PATH_BYTES: usize = 4096;
/// Prefix reserved for the service's own staging files.
pub const RESERVED_PREFIX: &str = ".itookit-upload-";

/// A path is valid when it is relative, non-empty in every segment, free of
/// separators the kernel would reinterpret, and outside the reserved namespace.
pub fn validate(path: &str) -> Result<(), Error> {
    if path.is_empty() {
        return Ok(());
    }
    if path.len() > MAX_PATH_BYTES
        || path.starts_with('/')
        || has_drive_prefix(path)
        || path.contains(['\\', '\0'])
        || path.split('/').any(invalid_segment)
    {
        return Err(Error::invalid());
    }
    Ok(())
}

/// Names the service reserves for staging and never exposes to clients.
pub fn is_reserved(name: &str) -> bool {
    name.starts_with(RESERVED_PREFIX)
}

/// `C:/host` is a platform absolute path, not an export-relative name.
fn has_drive_prefix(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or(path);
    let bytes = first.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn invalid_segment(part: &str) -> bool {
    part == ".." || part == "." || is_reserved(part) || part.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_relative_paths_and_the_root() {
        assert!(validate("").is_ok());
        assert!(validate("a").is_ok());
        assert!(validate("a/b/c.txt").is_ok());
        assert!(validate("with space/ünïcode.md").is_ok());
    }

    #[test]
    fn accepts_colons_that_are_not_a_drive_prefix() {
        // The HTTP driver contract: `2024:Q1.md` is a legal export-relative name.
        assert!(validate("2024:Q1.md").is_ok());
        assert!(validate("a/b:c").is_ok());
        assert!(validate(".itookit-upload-x/y").is_err());
    }

    #[test]
    fn rejects_platform_paths_traversal_and_reserved_names() {
        for path in [
            "/etc/passwd",
            "C:/host",
            "z:file",
            "../escape",
            "a/../b",
            ".",
            "./a",
            "a//b",
            "a/",
            "a\\b",
            "a\0b",
            ".itookit-upload-abc",
            "dir/.itookit-upload-abc",
        ] {
            assert!(validate(path).is_err(), "{path}");
        }
        assert!(validate(&"a".repeat(MAX_PATH_BYTES - 1)).is_ok());
        assert!(validate(&"a".repeat(MAX_PATH_BYTES + 1)).is_err());
    }
}
