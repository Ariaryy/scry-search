use crate::arena::{ArchivedArena, Arena};
use crate::frnmap::{FrnEntry, FrnMap};
use crate::record::FORMAT_VERSION;
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("archive validation failed: {0}")]
    Validation(String),
    #[error("format version mismatch: found {found}, expected {expected}")]
    VersionMismatch { found: u32, expected: u32 },
    #[error("not a scry snapshot")]
    BadMagic,
}

/// Leading bytes of every snapshot, ahead of the rkyv archive.
///
/// The version also lives inside the archive, but it cannot be *read* from one
/// written by a different version: rkyv resolves fields by offsets derived from
/// the current struct definition, so a layout change makes every field of an
/// older archive garbage — including the version field that was supposed to
/// detect the change. Validating such an archive fails deep inside bytecheck
/// with a pointer-out-of-bounds message that describes the symptom rather than
/// the cause, and `VersionMismatch` never fires. This header is outside the
/// archive and fixed forever, so a stale snapshot is diagnosed before rkyv sees
/// a byte of it.
const MAGIC: [u8; 8] = *b"SCRYIDX\0";

/// Magic, version, and four reserved bytes. Sixteen rather than twelve so the
/// archive stays 8-aligned behind it — the archive contains `u64` fields, and
/// rkyv's validator rejects a misaligned buffer.
const HEADER_LEN: usize = 16;

/// The complete snapshot image — header followed by the rkyv archive.
///
/// Returned as an `AlignedVec` because the archive behind the 16-byte header
/// must stay 8-aligned; a plain `Vec<u8>` gives no such guarantee and rkyv's
/// validator would reject the result on a bad allocation day.
pub fn to_bytes(arena: &Arena) -> Result<rkyv::AlignedVec, StoreError> {
    let archive =
        rkyv::to_bytes::<_, 1024>(arena).map_err(|e| StoreError::Validation(e.to_string()))?;
    let mut out = rkyv::AlignedVec::with_capacity(HEADER_LEN + archive.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&archive);
    Ok(out)
}

/// Validates the header and returns the archive bytes behind it.
fn split_header(bytes: &[u8]) -> Result<&[u8], StoreError> {
    let header = bytes.get(..HEADER_LEN).ok_or(StoreError::BadMagic)?;
    if header[..8] != MAGIC {
        return Err(StoreError::BadMagic);
    }
    let found = u32::from_le_bytes(header[8..12].try_into().expect("four bytes"));
    if found != FORMAT_VERSION {
        return Err(StoreError::VersionMismatch {
            found,
            expected: FORMAT_VERSION,
        });
    }
    Ok(&bytes[HEADER_LEN..])
}

/// Serialize an Arena to disk via rkyv. This is the only place allocation-heavy
/// serialization happens — it's an offline/background step (snapshot compaction),
/// never on the query path.
pub fn save(arena: &Arena, path: &Path) -> Result<(), StoreError> {
    save_with(arena, path, |_| {})
}

/// Like `save`, but calls `on_create` with the freshly-created temp file
/// before its contents are written, so callers can tag the handle (e.g. via
/// `FSCTL_MARK_HANDLE`) before any bytes hit the volume.
pub fn save_with<F>(arena: &Arena, path: &Path, on_create: F) -> Result<(), StoreError>
where
    F: FnOnce(&File),
{
    let bytes = to_bytes(arena)?;
    let tmp_path = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp_path)?;
        on_create(&f);
        f.write_all(&bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

pub fn save_with_sidecar<FA, FF>(
    arena: &Arena,
    frns: &mut [FrnEntry],
    path: &Path,
    on_arena_create: FA,
    on_sidecar_create: FF,
) -> Result<(), StoreError>
where
    FA: FnOnce(&File),
    FF: FnOnce(&File),
{
    save_with(arena, path, on_arena_create)?;
    FrnMap::save_with(&path.with_extension("frn"), frns, on_sidecar_create)?;
    Ok(())
}

/// An mmap-backed, zero-copy view of a persisted Arena. Opening this does not
/// deserialize anything — the OS page cache backs the memory, and `archived()`
/// just casts bytes.
///
/// Validation at open is cheap *because* of the format-v2 layout: the archive
/// contains three `Vec`s of plain PODs and no `String`s, so bytecheck performs
/// a handful of bounds checks rather than chasing a relative pointer and
/// UTF-8-validating a name for every one of a million records. That is what
/// keeps `open()` from faulting the whole file into RSS — which the pre-v2
/// layout did, despite this comment previously claiming otherwise.
pub struct ArenaStore {
    file: File,
    mmap: Mmap,
    pub frn_map: Option<FrnMap>,
}

impl ArenaStore {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let file = File::open(path)?;
        let mmap = unsafe { Mmap::map(&file)? };
        // Header first, so a stale snapshot is reported as a version mismatch
        // rather than as a bytecheck failure over a layout that no longer
        // applies. Then validate once at open time (bytecheck), not per query.
        let archive = split_header(&mmap[..])?;
        rkyv::check_archived_root::<Arena>(archive)
            .map_err(|e| StoreError::Validation(e.to_string()))?;
        let sidecar = path.with_extension("frn");
        let frn_map = match FrnMap::open(&sidecar) {
            Ok(map) => Some(map),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                eprintln!(
                    "scry: ignoring malformed FRN sidecar {}: {error}",
                    sidecar.display()
                );
                None
            }
        };
        Ok(Self {
            file,
            mmap,
            frn_map,
        })
    }

    #[inline]
    pub fn archived(&self) -> &ArchivedArena {
        // Safety: validated in `open` via split_header + check_archived_root.
        unsafe { rkyv::archived_root::<Arena>(&self.mmap[HEADER_LEN..]) }
    }

    /// The whole snapshot image, header included, as shared with clients.
    /// Consumers parse it with [`archived_bytes`], which expects the header.
    pub fn archive_bytes(&self) -> &[u8] {
        &self.mmap
    }

    pub fn snapshot_file(&self) -> &File {
        &self.file
    }
}

pub fn archived_bytes(bytes: &[u8]) -> Result<&ArchivedArena, StoreError> {
    let archive = split_header(bytes)?;
    rkyv::check_archived_root::<Arena>(archive)
        .map_err(|e| StoreError::Validation(e.to_string()))?;
    Ok(unsafe { rkyv::archived_root::<Arena>(archive) })
}

/// Return the archived root after the caller has validated this exact,
/// immutable byte mapping with [`archived_bytes`].
///
/// # Safety
///
/// `bytes` must be the same bytes previously accepted by [`archived_bytes`]
/// and must not have changed since validation.
pub unsafe fn archived_bytes_validated(bytes: &[u8]) -> &ArchivedArena {
    unsafe { rkyv::archived_root::<Arena>(&bytes[HEADER_LEN..]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_diagnoses_a_stale_or_alien_snapshot_from_the_header() {
        // Save a valid arena, then patch the version in its header.
        let dir = tempfile::tempdir().unwrap();
        let mut b = crate::arena::ArenaBuilder::default();
        b.push("test", 0, false);
        let arena = b.build().0;
        let path = dir.path().join("versioned.rkyv");
        save(&arena, &path).unwrap();

        // The version lives in the header at a fixed offset, so patching it is
        // exact rather than brittle — which is the whole point of the header.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&(FORMAT_VERSION - 1).to_le_bytes());
        let stale_path = dir.path().join("stale.rkyv");
        std::fs::write(&stale_path, &bytes).unwrap();
        match ArenaStore::open(&stale_path) {
            Err(StoreError::VersionMismatch { found, expected }) => {
                assert_eq!(found, FORMAT_VERSION - 1);
                assert_eq!(expected, FORMAT_VERSION);
            }
            other => panic!(
                "a stale snapshot must report a version mismatch, not a \
                 bytecheck failure over a layout that no longer applies; got {:?}",
                other.map(|_| "Ok")
            ),
        }

        // A file that isn't a snapshot at all is rejected on the magic.
        let alien_path = dir.path().join("alien.rkyv");
        std::fs::write(
            &alien_path,
            b"PK\x03\x04 definitely a zip file, not an index",
        )
        .unwrap();
        assert!(matches!(
            ArenaStore::open(&alien_path),
            Err(StoreError::BadMagic)
        ));

        let random_path = dir.path().join("random.rkyv");
        std::fs::write(
            &random_path,
            b"this is not a valid rkyv archive at all xxxx",
        )
        .unwrap();
        let result = ArenaStore::open(&random_path);
        assert!(
            result.is_err(),
            "expected error opening invalid archive, got Ok"
        );
    }

    #[test]
    fn store_opens_without_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("standalone.rkyv");
        let mut builder = crate::ArenaBuilder::default();
        builder.push("file", 0, false);
        save(&builder.build().0, &path).unwrap();
        let store = ArenaStore::open(&path).unwrap();
        assert!(store.frn_map.is_none());
    }
}
