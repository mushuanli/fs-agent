//! Identifier policy.
//!
//! Server ids, operation ids, process request ids and export aliases all share
//! the same alphabet so a single predicate decides what is addressable. Only
//! the length limit differs, so it is an explicit parameter.

/// Upper bound applied to every wire identifier that is not a path.
pub const IDENTIFIER_MAX: usize = 128;

/// Identifier alphabet: ASCII letters, digits, `-` and `_`.
pub fn is_identifier(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_identifier_byte)
}

/// [`is_identifier`] plus a byte-length limit.
pub fn is_identifier_within(value: &str, max: usize) -> bool {
    value.len() <= max && is_identifier(value)
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_unsafe_and_oversized_values() {
        assert!(is_identifier("agent-node_1"));
        assert!(!is_identifier(""));
        assert!(!is_identifier("../escape"));
        assert!(!is_identifier("with space"));
        assert!(!is_identifier("with/slash"));
        assert!(is_identifier_within(
            &"a".repeat(IDENTIFIER_MAX),
            IDENTIFIER_MAX
        ));
        assert!(!is_identifier_within(
            &"a".repeat(IDENTIFIER_MAX + 1),
            IDENTIFIER_MAX
        ));
    }
}
