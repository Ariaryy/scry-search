use crate::arena::{ArchivedArena, Arena};
use crate::frnmap::{FrnEntry, FrnMap};
use crate::record::FORMAT_VERSION;
use memmap2::Mmap;
use rkyv::ser::serializers::{
    AllocScratch, CompositeSerializer, FallbackScratch, HeapScratch, SharedSerializeMap,
    WriteSerializer,
};
use rkyv::ser::Serializer as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use thiserror::Error;

mod columns;
pub use columns::{ArenaColumns, ArenaColumnsResolver};

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

/// Magic, version, and a snapshot generation tag. Sixteen rather than twelve
/// so the archive stays 8-aligned behind it — the archive contains `u64`
/// fields, and rkyv's validator rejects a misaligned buffer. The tag pairs a
/// snapshot with its separately-renamed FRN sidecar; zero means unpaired.
const HEADER_LEN: usize = 16;

pub(crate) fn fresh_snapshot_tag() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU32 = AtomicU32::new(0);
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = elapsed.as_nanos() as u64;
    let sequence = NEXT.fetch_add(0x9e37_79b9, Ordering::Relaxed);
    let tag = (nanos as u32) ^ ((nanos >> 32) as u32) ^ std::process::id() ^ sequence;
    if tag == 0 {
        1
    } else {
        tag
    }
}

fn write_header(mut writer: impl Write, snapshot_tag: u32) -> std::io::Result<()> {
    writer.write_all(&MAGIC)?;
    writer.write_all(&FORMAT_VERSION.to_le_bytes())?;
    writer.write_all(&snapshot_tag.to_le_bytes())
}

/// Serializer that writes the archive straight to a `BufWriter<File>` instead
/// of building it up in an in-memory `AlignedVec` first. Scratch space and the
/// shared-pointer map still allocate — `Arena` has no shared (`Rc`-style)
/// fields, so `SharedSerializeMap` stays empty, and 1024 bytes of inline
/// scratch is enough for `Arena`'s own out-of-line `Vec` resolvers before it
/// would spill to the heap — but the archive's bytes themselves are streamed
/// out one write at a time rather than buffered, which is what made
/// `rkyv::to_bytes` cost one ~50 MB allocation on a large index.
type StreamingSerializer = CompositeSerializer<
    WriteSerializer<BufWriter<File>>,
    FallbackScratch<HeapScratch<1024>, AllocScratch>,
    SharedSerializeMap,
>;

/// The complete snapshot image — header followed by the rkyv archive — built
/// entirely in memory. Only for callers that need a contiguous, in-memory
/// image rather than a file (e.g. copying an archive straight into a shared
/// section for a test fixture); `save_with` below is the on-disk path and
/// streams instead of buffering.
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

/// Validates the header and returns the archive bytes and sidecar pairing tag.
fn split_header(bytes: &[u8]) -> Result<(&[u8], u32), StoreError> {
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
    let snapshot_tag = u32::from_le_bytes(header[12..16].try_into().expect("four bytes"));
    Ok((&bytes[HEADER_LEN..], snapshot_tag))
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
    save_with_tag(arena, path, 0, on_create)
}

fn save_with_tag<F>(
    arena: &Arena,
    path: &Path,
    snapshot_tag: u32,
    on_create: F,
) -> Result<(), StoreError>
where
    F: FnOnce(&File),
{
    let tmp_path = path.with_extension("tmp");
    let file = {
        let f = File::create(&tmp_path)?;
        on_create(&f);
        let mut writer = BufWriter::new(f);
        write_header(&mut writer, snapshot_tag)?;

        let mut serializer = StreamingSerializer::new(
            WriteSerializer::new(writer),
            FallbackScratch::default(),
            SharedSerializeMap::default(),
        );
        serializer
            .serialize_value(arena)
            .map_err(|e| StoreError::Validation(e.to_string()))?;
        let (writer, _, _) = serializer.into_components();
        let mut buf_writer = writer.into_inner();
        buf_writer.flush()?;
        buf_writer
            .into_inner()
            .map_err(|e| StoreError::Io(e.into_error()))?
    };
    file.sync_all()?;
    std::fs::rename(tmp_path, path)?;
    Ok(())
}

/// Like [`save_with`], but for [`ArenaColumns`] instead of an owned `Arena` —
/// compaction's entry point, so it never has to build one.
pub fn save_columns_with<F>(
    columns: &ArenaColumns<'_>,
    path: &Path,
    on_create: F,
) -> Result<(), StoreError>
where
    F: FnOnce(&File),
{
    save_columns_with_tag(columns, path, 0, on_create)
}

pub(crate) fn save_columns_with_tag<F>(
    columns: &ArenaColumns<'_>,
    path: &Path,
    snapshot_tag: u32,
    on_create: F,
) -> Result<(), StoreError>
where
    F: FnOnce(&File),
{
    let tmp_path = path.with_extension("tmp");
    let file = {
        let f = File::create(&tmp_path)?;
        on_create(&f);
        let mut writer = BufWriter::new(f);
        write_header(&mut writer, snapshot_tag)?;

        let mut serializer = StreamingSerializer::new(
            WriteSerializer::new(writer),
            FallbackScratch::default(),
            SharedSerializeMap::default(),
        );
        serializer
            .serialize_value(columns)
            .map_err(|e| StoreError::Validation(e.to_string()))?;
        let (writer, _, _) = serializer.into_components();
        let mut buf_writer = writer.into_inner();
        buf_writer.flush()?;
        buf_writer
            .into_inner()
            .map_err(|e| StoreError::Io(e.into_error()))?
    };
    file.sync_all()?;
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
    let snapshot_tag = fresh_snapshot_tag();
    save_with_tag(arena, path, snapshot_tag, on_arena_create)?;
    FrnMap::save_with(
        &path.with_extension("frn"),
        frns,
        snapshot_tag,
        on_sidecar_create,
    )?;
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
        let (archive, snapshot_tag) = split_header(&mmap[..])?;
        rkyv::check_archived_root::<Arena>(archive)
            .map_err(|e| StoreError::Validation(e.to_string()))?;
        let archived = unsafe { rkyv::archived_root::<Arena>(archive) };
        validate_column_lengths(archived)?;
        let sidecar = path.with_extension("frn");
        let frn_map = match FrnMap::open(&sidecar, snapshot_tag) {
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
    let (archive, _) = split_header(bytes)?;
    rkyv::check_archived_root::<Arena>(archive)
        .map_err(|e| StoreError::Validation(e.to_string()))?;
    let archived = unsafe { rkyv::archived_root::<Arena>(archive) };
    validate_column_lengths(archived)?;
    Ok(archived)
}

fn validate_column_lengths(arena: &ArchivedArena) -> Result<(), StoreError> {
    let expected = arena.parents.len().div_ceil(8);
    if arena.size_exact_bits.len() != expected {
        return Err(StoreError::Validation(format!(
            "size exactness bitmap has {} bytes, expected {expected}",
            arena.size_exact_bits.len()
        )));
    }
    Ok(())
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
mod tests;
