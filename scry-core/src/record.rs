use rkyv::{Archive, Deserialize, Serialize};

/// Format version stamped into every snapshot. `ArenaStore::open` rejects
/// snapshots with a different version rather than parsing them leniently —
/// a version bump is a breaking format change, not a migration.
pub const FORMAT_VERSION: u32 = 4;

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

/// Bit 31 of `FileRecord::parent_and_flags` — set for directories.
pub const DIR_BIT: u32 = 0x8000_0000;

/// Sentinel parent index meaning "no parent" (volume root).
/// Must not equal DIR_BIT; capped at 2^31 - 2 so it fits in 31 bits.
pub const PARENT_NONE: u32 = 0x7FFF_FFFF;

/// One indexed filesystem entry. Two fields, 8 bytes total — this is the
/// unit multiplied by millions, so every byte here is megabytes of RSS.
///
/// Names are NOT stored here; they live in the front-coded `Arena.names`
/// blob, indexed via `Arena.bucket_offsets`. This keeps the hot record
/// array cache-friendly during scans.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Copy)]
#[archive(check_bytes)]
pub struct FileRecord {
    /// Bit 31 = is_dir flag; bits 0..30 = parent record index.
    /// Use the accessors rather than reading this field directly.
    pub parent_and_flags: u32,
    /// Seconds since 1970-01-01 UTC, clamped. Range: 0 (1970) to 2^32-1
    /// (year 2106). USN records give 100ns FILETIMEs; the narrower field
    /// saves 4 bytes per record versus an i64, and 4 bytes per record is
    /// megabytes of RSS at a million files.
    pub mtime_secs: u32,
}

impl FileRecord {
    #[inline]
    pub fn new(parent: u32, is_dir: bool, mtime_secs: u32) -> Self {
        debug_assert!(parent <= PARENT_NONE, "parent index exceeds PARENT_NONE");
        let flags = if is_dir { DIR_BIT } else { 0 };
        FileRecord {
            parent_and_flags: flags | parent,
            mtime_secs,
        }
    }

    #[inline]
    pub fn parent(&self) -> u32 {
        self.parent_and_flags & !DIR_BIT
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.parent_and_flags & DIR_BIT != 0
    }
}

impl ArchivedFileRecord {
    #[inline]
    pub fn parent(&self) -> u32 {
        self.parent_and_flags & !DIR_BIT
    }

    #[inline]
    pub fn is_dir(&self) -> bool {
        self.parent_and_flags & DIR_BIT != 0
    }
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
    fn file_record_is_8_bytes() {
        assert_eq!(std::mem::size_of::<FileRecord>(), 8);
        assert_eq!(std::mem::size_of::<ArchivedFileRecord>(), 8);
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
    fn parent_and_flags_round_trips() {
        let r = FileRecord::new(42, true, 100);
        assert!(r.is_dir());
        assert_eq!(r.parent(), 42);

        let r2 = FileRecord::new(PARENT_NONE, false, 0);
        assert!(!r2.is_dir());
        assert_eq!(r2.parent(), PARENT_NONE);
    }
}
