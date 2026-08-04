//! Result ordering, expressed as a single `u64` sort key per candidate.
//!
//! The search loops keep only the best `limit` candidates in a bounded heap,
//! so every candidate that survives the match test is compared. Making the
//! comparison a trait object, or a tuple of several fields, would put a branch
//! or a multi-word compare on the hottest path in the program. Instead each
//! ordering packs its fields into one `u64` that sorts **ascending, smallest
//! first**, and the heap compares plain integers.
//!
//! Every key ends with the record index in its low 32 bits. That makes the
//! order total — two files with the same size or the same timestamp still have
//! a defined position — so results are stable across runs rather than
//! depending on which block a parallel scan finished first. It also means the
//! record can be read back out of the key, which is why the heap stores keys
//! and nothing else.
//!
//! Descending orderings (newest first, largest first) are expressed by storing
//! the bitwise complement of the field. Complementing preserves the ordering
//! of unsigned integers exactly, so no separate comparator is needed.

/// How to order results.
///
/// Adding a variant means adding a `*_key` constructor below and a wire
/// discriminant in `protocol::Request`; nothing in the search loops changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// Best match first: exact name, then prefix, then anything; shorter names
    /// win ties. What a launcher or a search box wants.
    #[default]
    Relevance = 0,
    /// Most recently modified first.
    Recent = 1,
    /// Largest first. Paired with directory sizes this is what a
    /// disk-usage view wants.
    Largest = 2,
}

impl Order {
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(Order::Relevance),
            1 => Some(Order::Recent),
            2 => Some(Order::Largest),
            _ => None,
        }
    }

    /// Whether this ordering reads the cold `mtimes`/`sizes` columns. The
    /// search loops use it to skip a cold-column read per candidate when the
    /// ordering doesn't need one.
    #[inline]
    pub fn needs_metadata(self) -> bool {
        !matches!(self, Order::Relevance)
    }
}

/// Name lengths are clamped to this before entering a key. A filename cannot
/// approach it — NTFS caps at 255 — so the clamp only guards against a
/// corrupt length corrupting the ordering of the bits above it.
const NAME_LEN_BITS: u32 = 24;
const NAME_LEN_MAX: u64 = (1 << NAME_LEN_BITS) - 1;

/// `quality` is the match class (0 = exact, 1 = prefix, 2 = other, or the
/// count of unmatched path terms); lower is better.
#[inline]
pub fn relevance_key(quality: u8, name_len: u32, record: u32) -> u64 {
    ((quality as u64) << 56) | ((name_len as u64).min(NAME_LEN_MAX) << 32) | record as u64
}

/// Newest first: seconds since the Unix epoch, complemented.
#[inline]
pub fn recent_key(mtime_secs: u32, record: u32) -> u64 {
    ((!mtime_secs as u64) << 32) | record as u64
}

/// Largest first: size in KiB, complemented. KiB rather than bytes because
/// that is the width the index actually stores, and because it leaves the low
/// 32 bits for the record.
#[inline]
pub fn largest_key(size_kib: u32, record: u32) -> u64 {
    ((!size_kib as u64) << 32) | record as u64
}

/// The record a key was built for.
#[inline]
pub fn key_record(key: u64) -> u32 {
    key as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_carries_its_record_back() {
        for record in [0u32, 1, 4095, u32::MAX - 1] {
            assert_eq!(key_record(relevance_key(2, 40, record)), record);
            assert_eq!(key_record(recent_key(1_700_000_000, record)), record);
            assert_eq!(key_record(largest_key(9_001, record)), record);
        }
    }

    #[test]
    fn relevance_prefers_better_class_then_shorter_name() {
        assert!(relevance_key(0, 100, 5) < relevance_key(1, 1, 5));
        assert!(relevance_key(1, 4, 5) < relevance_key(1, 40, 5));
        // Record only breaks exact ties, never outranks a real field.
        assert!(relevance_key(1, 4, u32::MAX) < relevance_key(1, 5, 0));
    }

    #[test]
    fn recent_and_largest_sort_descending_on_their_field() {
        assert!(
            recent_key(2_000, 0) < recent_key(1_000, 0),
            "newer sorts first"
        );
        assert!(
            largest_key(4_096, 0) < largest_key(4, 0),
            "bigger sorts first"
        );
        // Zero — which for `size` means *unknown*, not empty — sorts last
        // rather than first, so an unpopulated column doesn't crowd out real
        // results at the top of a size-ordered list.
        assert!(largest_key(1, 0) < largest_key(0, 0));
    }

    /// The clamp must not let an absurd length bleed into the quality bits and
    /// promote a bad match over a good one.
    #[test]
    fn an_overlong_name_cannot_outrank_a_better_match_class() {
        assert!(relevance_key(0, u32::MAX, 0) < relevance_key(1, 0, 0));
    }
}
