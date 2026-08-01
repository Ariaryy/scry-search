/// Format version stamped into every snapshot. `ArenaStore::open` rejects
/// snapshots with a different version rather than parsing them leniently —
/// a version bump is a breaking format change, not a migration.
pub const FORMAT_VERSION: u32 = 6;

/// Seconds between the Windows FILETIME epoch (1601-01-01) and the Unix
/// epoch (1970-01-01). `mtime_secs` is stored relative to 1970 rather than
/// 1601 because a `u32` spans only ~136 years: anchored at 1601 it saturates
/// in 1737, making every real timestamp `u32::MAX`. Anchored at 1970 the same
/// four bytes reach 2106.
pub const FILETIME_UNIX_EPOCH_SECS: i64 = 11_644_473_600;

/// Front-coding bucket size: every 32 records share a common-prefix table.
/// Larger buckets compress better but increase random-access cost (up to
/// BUCKET_SIZE sequential decode steps). 32 is empirically good for
/// filename corpora.
pub const BUCKET_SIZE: usize = 32;

/// Bit 31 of a packed parent word — set for directories.
pub const DIR_BIT: u32 = 0x8000_0000;

/// Sentinel parent index meaning "no parent" (volume root).
/// Must not equal DIR_BIT; capped at 2^31 - 2 so it fits in 31 bits.
pub const PARENT_NONE: u32 = 0x7FFF_FFFF;

/// Packs a parent index and directory flag into one word.
/// Stored in `Arena.parents`; bits 0..30 = parent record index, bit 31 = is_dir.
///
/// This is the hot word: `full_path` hops through parent links on every
/// displayed result, so it lives alone in its column so that a 64-byte cache
/// line carries 16 useful parents instead of 8 (as it would interleaved with
/// mtime and size).
#[inline]
pub fn pack_parent(parent: u32, is_dir: bool) -> u32 {
    debug_assert!(parent <= PARENT_NONE, "parent index exceeds PARENT_NONE");
    let flags = if is_dir { DIR_BIT } else { 0 };
    flags | parent
}

/// Extracts the parent record index from a packed parent word.
#[inline]
pub fn unpack_parent(word: u32) -> u32 {
    word & !DIR_BIT
}

/// Tests the directory flag in a packed parent word.
#[inline]
pub fn word_is_dir(word: u32) -> bool {
    word & DIR_BIT != 0
}

/// Convert bytes to KiB, rounding up and saturating at `u32::MAX`.
pub fn bytes_to_size_kib(bytes: u64) -> u32 {
    bytes.div_ceil(1024).min(u32::MAX as u64) as u32
}

/// Convert a Windows FILETIME (100ns ticks since 1601-01-01) to whole seconds
/// since the **Unix epoch**, clamping rather than wrapping. USN records give
/// FILETIMEs natively; 100ns precision is far more than a filename index
/// needs. Timestamps before 1970 clamp to 0; after 2106 they clamp to
/// `u32::MAX`.
pub fn filetime_to_secs(ft: i64) -> u32 {
    if ft <= 0 {
        return 0;
    }
    let unix = ft / 10_000_000 - FILETIME_UNIX_EPOCH_SECS;
    unix.clamp(0, u32::MAX as i64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_parent_round_trips() {
        let word = pack_parent(42, true);
        assert!(word_is_dir(word));
        assert_eq!(unpack_parent(word), 42);

        let word2 = pack_parent(PARENT_NONE, false);
        assert!(!word_is_dir(word2));
        assert_eq!(unpack_parent(word2), PARENT_NONE);
    }

    #[test]
    fn filetime_to_secs_boundary_cases() {
        // Pre-1970 (including the FILETIME epoch itself) clamps to 0.
        assert_eq!(filetime_to_secs(0), 0);
        assert_eq!(filetime_to_secs(-1), 0);
        assert_eq!(filetime_to_secs(10_000_000), 0); // 1601-01-01T00:00:01Z
                                                     // Exactly the Unix epoch.
        assert_eq!(filetime_to_secs(FILETIME_UNIX_EPOCH_SECS * 10_000_000), 0);
        assert_eq!(
            filetime_to_secs((FILETIME_UNIX_EPOCH_SECS + 1) * 10_000_000),
            1
        );
        // Far future clamps instead of wrapping.
        assert_eq!(filetime_to_secs(i64::MAX), u32::MAX);
    }

    /// Regression test for the format-v2 epoch bug: `mtime_secs` was
    /// originally seconds since 1601, which a u32 can only represent up to
    /// 1737 — so every real-world timestamp saturated to u32::MAX and the
    /// field carried no information at all.
    #[test]
    fn filetime_to_secs_represents_modern_timestamps() {
        // 2001-01-01T00:00:00Z == Unix 978_307_200.
        const FT_2001: i64 = 126_227_808_000_000_000;
        assert_eq!(filetime_to_secs(FT_2001), 978_307_200);

        // 2026-08-01T00:00:00Z == Unix 1_785_542_400.
        const FT_2026: i64 = 134_300_160_000_000_000;
        let secs_2026 = filetime_to_secs(FT_2026);
        assert_eq!(secs_2026, 1_785_542_400);

        // The defining property: a present-day timestamp must NOT saturate.
        assert_ne!(
            secs_2026,
            u32::MAX,
            "modern timestamps must not saturate — the epoch is wrong"
        );
        // And ordering must be preserved across real dates.
        assert!(filetime_to_secs(FT_2001) < secs_2026);
    }

    #[test]
    fn size_kib_roundtrips_and_saturates() {
        for (input, expected_kib) in [
            (0u64, 0u32),
            (1, 1),
            (1023, 1),
            (1024, 1),
            (u64::MAX, u32::MAX),
        ] {
            assert_eq!(bytes_to_size_kib(input), expected_kib);
        }
    }
}
