//! HTTP `Range` policy for content reads.
//!
//! A single-range parser with an explicit fallback policy: unknown or
//! multi-range forms degrade to a full response, while a syntactically valid
//! but unsatisfiable range is reported as `416` with `bytes */<size>`.

use crate::core::error::Error;

/// A resolved byte window within a resource of the reported size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub length: u64,
    /// Whether the response must be `206 Partial Content`.
    pub partial: bool,
}

impl ByteRange {
    fn full(size: u64) -> Self {
        Self {
            start: 0,
            length: size,
            partial: false,
        }
    }

    /// `bytes */<size>` header value for an unsatisfiable range.
    pub fn unsatisfiable_header(size: u64) -> String {
        format!("bytes */{size}")
    }

    /// `bytes <start>-<end>/<size>` header value for a partial response.
    pub fn content_range(self, size: u64) -> String {
        format!(
            "bytes {}-{}/{size}",
            self.start,
            self.start + self.length - 1
        )
    }
}

pub fn resolve(header: Option<&str>, size: u64) -> Result<ByteRange, Error> {
    let Some(header) = header else {
        return Ok(ByteRange::full(size));
    };
    // Multi-range responses and unknown units are ignored, per RFC 7233.
    if header.contains(',') {
        return Ok(ByteRange::full(size));
    }
    let Some(value) = header.strip_prefix("bytes=") else {
        return Ok(ByteRange::full(size));
    };
    let (start, end) = value.split_once('-').ok_or_else(Error::invalid)?;
    if start.is_empty() {
        let suffix: u64 = end.parse().map_err(|_| Error::invalid())?;
        if suffix == 0 || size == 0 {
            return Err(Error::range_unsatisfiable());
        }
        let length = suffix.min(size);
        return Ok(ByteRange {
            start: size - length,
            length,
            partial: true,
        });
    }
    let start: u64 = start.parse().map_err(|_| Error::invalid())?;
    let end: u64 = if end.is_empty() {
        size.saturating_sub(1)
    } else {
        end.parse().map_err(|_| Error::invalid())?
    };
    if start >= size || end < start {
        return Err(Error::range_unsatisfiable());
    }
    Ok(ByteRange {
        start,
        length: end.min(size - 1) - start + 1,
        partial: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_multiple_ranges_return_the_whole_body() {
        assert_eq!(resolve(None, 10).unwrap(), ByteRange::full(10));
        assert_eq!(
            resolve(Some("bytes=0-1,4-5"), 10).unwrap(),
            ByteRange::full(10)
        );
    }

    #[test]
    fn resolves_explicit_and_suffix_ranges() {
        assert_eq!(
            resolve(Some("bytes=3-5"), 10).unwrap(),
            ByteRange {
                start: 3,
                length: 3,
                partial: true
            }
        );
        assert_eq!(
            resolve(Some("bytes=7-"), 10).unwrap(),
            ByteRange {
                start: 7,
                length: 3,
                partial: true
            }
        );
        assert_eq!(
            resolve(Some("bytes=-4"), 10).unwrap(),
            ByteRange {
                start: 6,
                length: 4,
                partial: true
            }
        );
        // An end beyond the resource is clamped instead of rejected.
        assert_eq!(resolve(Some("bytes=8-99"), 10).unwrap().length, 2);
    }

    #[test]
    fn ignores_unknown_units_and_rejects_malformed_ranges() {
        // RFC 7233: an unsupported unit means "send the whole representation".
        assert_eq!(resolve(Some("items=0-1"), 10).unwrap(), ByteRange::full(10));
        for header in ["bytes=", "bytes=a-b", "bytes=5-2", "bytes=10-", "bytes=-0"] {
            assert!(resolve(Some(header), 10).is_err(), "{header}");
        }
        assert!(resolve(Some("bytes=0-"), 0).is_err());
    }
}
