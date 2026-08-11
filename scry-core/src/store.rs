use crate::arena::{ArchivedArena, Arena};
use crate::frnmap::{FrnEntry, FrnMap};
use crate::record::FORMAT_VERSION;
use memmap2::Mmap;
use rkyv::ser::serializers::{
    AllocScratch, CompositeSerializer, FallbackScratch, HeapScratch, SharedSerializeMap,
    WriteSerializer,
};
use rkyv::ser::Serializer as _;
use rkyv::vec::{ArchivedVec, VecResolver};
use std::fs::File;
use std::io::{BufWriter, Write};
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

/// Stand-in for `&Arena` that lets compaction emit the v9 archive without
/// ever materializing an owned `Arena`: scalar header fields plus a `&[T]`
/// view into each column, which — unlike a real `Arena`'s fields — can be
/// backed by a [`crate::spool::Spool`]'s mmap rather than a `Vec`.
///
/// Its [`rkyv::Archive`] `Archived` type is `ArchivedArena`, the *same* type
/// `#[derive(Archive)]` generates for `Arena` — this hand-written impl below
/// drives the same primitives (`rkyv::out_field!` for each field's offset,
/// `ArchivedVec::resolve_from_len`/`serialize_copy_from_slice` for each `Vec`
/// field) that the derived code would, just fed from borrowed slices instead
/// of owned `Vec`s. Field order here matches `Arena`'s declaration order, so
/// the emitted bytes match what `serializer.serialize_value(&arena)` would
/// have produced from the equivalent owned `Arena` exactly. This keeps
/// `FORMAT_VERSION` and `check_archived_root::<Arena>` unchanged — it is not
/// a custom container, just a different producer for the same archive.
pub struct ArenaColumns<'a> {
    pub format_version: u32,
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_serial: u64,
    pub names: &'a [u8],
    pub bucket_offsets: &'a [u32],
    pub parents: &'a [u32],
    pub mtimes: &'a [u32],
    pub sizes: &'a [u32],
    pub size_exact_bits: &'a [u8],
    pub trigram_index: &'a [u8],
    pub dfs_positions: &'a [u32],
    pub dfs_records: &'a [u32],
    pub dfs_ends: &'a [u32],
    pub dfs_size_prefix: &'a [u64],
}

#[doc(hidden)]
pub struct ArenaColumnsResolver {
    names: VecResolver,
    bucket_offsets: VecResolver,
    parents: VecResolver,
    mtimes: VecResolver,
    sizes: VecResolver,
    size_exact_bits: VecResolver,
    trigram_index: VecResolver,
    dfs_positions: VecResolver,
    dfs_records: VecResolver,
    dfs_ends: VecResolver,
    dfs_size_prefix: VecResolver,
}

impl rkyv::Archive for ArenaColumns<'_> {
    type Archived = ArchivedArena;
    type Resolver = ArenaColumnsResolver;

    #[allow(clippy::unit_arg)]
    unsafe fn resolve(&self, pos: usize, resolver: Self::Resolver, out: *mut ArchivedArena) {
        let (fp, fo) = rkyv::out_field!(out.format_version);
        rkyv::Archive::resolve(&self.format_version, pos + fp, (), fo);
        let (fp, fo) = rkyv::out_field!(out.journal_id);
        rkyv::Archive::resolve(&self.journal_id, pos + fp, (), fo);
        let (fp, fo) = rkyv::out_field!(out.next_usn);
        rkyv::Archive::resolve(&self.next_usn, pos + fp, (), fo);
        let (fp, fo) = rkyv::out_field!(out.volume_serial);
        rkyv::Archive::resolve(&self.volume_serial, pos + fp, (), fo);

        let (fp, fo) = rkyv::out_field!(out.names);
        ArchivedVec::resolve_from_len(self.names.len(), pos + fp, resolver.names, fo);
        let (fp, fo) = rkyv::out_field!(out.bucket_offsets);
        ArchivedVec::resolve_from_len(
            self.bucket_offsets.len(),
            pos + fp,
            resolver.bucket_offsets,
            fo,
        );
        let (fp, fo) = rkyv::out_field!(out.parents);
        ArchivedVec::resolve_from_len(self.parents.len(), pos + fp, resolver.parents, fo);
        let (fp, fo) = rkyv::out_field!(out.mtimes);
        ArchivedVec::resolve_from_len(self.mtimes.len(), pos + fp, resolver.mtimes, fo);
        let (fp, fo) = rkyv::out_field!(out.sizes);
        ArchivedVec::resolve_from_len(self.sizes.len(), pos + fp, resolver.sizes, fo);
        let (fp, fo) = rkyv::out_field!(out.size_exact_bits);
        ArchivedVec::resolve_from_len(
            self.size_exact_bits.len(),
            pos + fp,
            resolver.size_exact_bits,
            fo,
        );
        let (fp, fo) = rkyv::out_field!(out.trigram_index);
        ArchivedVec::resolve_from_len(
            self.trigram_index.len(),
            pos + fp,
            resolver.trigram_index,
            fo,
        );
        let (fp, fo) = rkyv::out_field!(out.dfs_positions);
        ArchivedVec::resolve_from_len(
            self.dfs_positions.len(),
            pos + fp,
            resolver.dfs_positions,
            fo,
        );
        let (fp, fo) = rkyv::out_field!(out.dfs_records);
        ArchivedVec::resolve_from_len(self.dfs_records.len(), pos + fp, resolver.dfs_records, fo);
        let (fp, fo) = rkyv::out_field!(out.dfs_ends);
        ArchivedVec::resolve_from_len(self.dfs_ends.len(), pos + fp, resolver.dfs_ends, fo);
        let (fp, fo) = rkyv::out_field!(out.dfs_size_prefix);
        ArchivedVec::resolve_from_len(
            self.dfs_size_prefix.len(),
            pos + fp,
            resolver.dfs_size_prefix,
            fo,
        );
    }
}

impl<S: rkyv::ser::Serializer + ?Sized> rkyv::Serialize<S> for ArenaColumns<'_> {
    fn serialize(&self, serializer: &mut S) -> Result<Self::Resolver, S::Error> {
        // Safety: u8/u32/u64 have no padding and their archived
        // representation equals their native one (no endian feature is
        // enabled in this workspace), so a raw byte copy is copy-safe.
        let names =
            unsafe { ArchivedVec::<u8>::serialize_copy_from_slice(self.names, serializer)? };
        let bucket_offsets = unsafe {
            ArchivedVec::<u32>::serialize_copy_from_slice(self.bucket_offsets, serializer)?
        };
        let parents =
            unsafe { ArchivedVec::<u32>::serialize_copy_from_slice(self.parents, serializer)? };
        let mtimes =
            unsafe { ArchivedVec::<u32>::serialize_copy_from_slice(self.mtimes, serializer)? };
        let sizes =
            unsafe { ArchivedVec::<u32>::serialize_copy_from_slice(self.sizes, serializer)? };
        let size_exact_bits = unsafe {
            ArchivedVec::<u8>::serialize_copy_from_slice(self.size_exact_bits, serializer)?
        };
        let trigram_index = unsafe {
            ArchivedVec::<u8>::serialize_copy_from_slice(self.trigram_index, serializer)?
        };
        let dfs_positions = unsafe {
            ArchivedVec::<u32>::serialize_copy_from_slice(self.dfs_positions, serializer)?
        };
        let dfs_records =
            unsafe { ArchivedVec::<u32>::serialize_copy_from_slice(self.dfs_records, serializer)? };
        let dfs_ends =
            unsafe { ArchivedVec::<u32>::serialize_copy_from_slice(self.dfs_ends, serializer)? };
        let dfs_size_prefix = unsafe {
            ArchivedVec::<u64>::serialize_copy_from_slice(self.dfs_size_prefix, serializer)?
        };
        Ok(ArenaColumnsResolver {
            names,
            bucket_offsets,
            parents,
            mtimes,
            sizes,
            size_exact_bits,
            trigram_index,
            dfs_positions,
            dfs_records,
            dfs_ends,
            dfs_size_prefix,
        })
    }
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
mod tests {
    use super::*;

    /// `save_columns_with` must emit exactly the archive `save_with` would
    /// for the equivalent owned `Arena` — same header, same rkyv bytes — so
    /// this compares full file contents, not just that `ArenaStore::open`
    /// accepts the result.
    #[test]
    fn save_columns_with_matches_save_with_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = crate::arena::ArenaBuilder::default();
        b.push("alpha", 10, true);
        b.push("alpha/beta.txt", 20, false);
        b.push("gamma.txt", 30, false);
        for i in 0..(crate::trigram::TRIGRAM_BLOCK + crate::record::BUCKET_SIZE + 7) {
            b.push(&format!("entry_{i:04}_shared_suffix.dat"), i as u32, false);
        }
        let (mut arena, _) = b.build();
        arena.journal_id = 42;
        arena.next_usn = 7;
        arena.volume_serial = 99;

        let derived_path = dir.path().join("derived.rkyv");
        save(&arena, &derived_path).unwrap();

        let columns = ArenaColumns {
            format_version: arena.format_version,
            journal_id: arena.journal_id,
            next_usn: arena.next_usn,
            volume_serial: arena.volume_serial,
            names: &arena.names,
            bucket_offsets: &arena.bucket_offsets,
            parents: &arena.parents,
            mtimes: &arena.mtimes,
            sizes: &arena.sizes,
            size_exact_bits: &arena.size_exact_bits,
            trigram_index: &arena.trigram_index,
            dfs_positions: &arena.dfs_positions,
            dfs_records: &arena.dfs_records,
            dfs_ends: &arena.dfs_ends,
            dfs_size_prefix: &arena.dfs_size_prefix,
        };
        let manual_path = dir.path().join("manual.rkyv");
        save_columns_with(&columns, &manual_path, |_| {}).unwrap();

        assert_eq!(
            std::fs::read(&derived_path).unwrap(),
            std::fs::read(&manual_path).unwrap()
        );

        // And the manually-produced archive must actually validate and read
        // back correctly through the normal open path.
        let store = ArenaStore::open(&manual_path).unwrap();
        let reopened = store.archived();
        assert_eq!(reopened.len(), arena.len());
        let mut name = Vec::new();
        reopened.name_into(1, &mut name);
        assert_eq!(name, b"alpha/beta.txt");
    }

    /// Replacing the path publishes a new generation without invalidating an
    /// mmap held by an in-flight reader. This is the file-level fault case;
    /// ArcSwap and client handshake tests cover publication/detection above it.
    #[test]
    fn live_mapping_survives_atomic_snapshot_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("replaceable.rkyv");

        let mut first_builder = crate::arena::ArenaBuilder::default();
        first_builder.push_bytes_with_frn(b"first", 0, false, 1);
        let (first, mut first_frns) = first_builder.build();
        save_with_sidecar(&first, &mut first_frns, &path, |_| {}, |_| {}).unwrap();
        let old_mapping = ArenaStore::open(&path).unwrap();
        let old_tag = u32::from_le_bytes(old_mapping.archive_bytes()[12..16].try_into().unwrap());

        let mut second_builder = crate::arena::ArenaBuilder::default();
        second_builder.push_bytes_with_frn(b"second-a", 0, false, 2);
        second_builder.push_bytes_with_frn(b"second-b", 0, false, 3);
        let (second, mut second_frns) = second_builder.build();
        save_with_sidecar(&second, &mut second_frns, &path, |_| {}, |_| {}).unwrap();
        let new_mapping = ArenaStore::open(&path).unwrap();
        let new_tag = u32::from_le_bytes(new_mapping.archive_bytes()[12..16].try_into().unwrap());

        assert_eq!(old_mapping.archived().len(), 1);
        assert_eq!(old_mapping.archived().name(0), "first");
        assert_eq!(new_mapping.archived().len(), 2);
        assert_ne!(old_tag, new_tag, "replacement must publish a new tag");
    }

    #[test]
    fn open_ignores_a_sidecar_from_another_snapshot_generation() {
        let dir = tempfile::tempdir().unwrap();

        let mut first_builder = crate::arena::ArenaBuilder::default();
        first_builder.push_bytes_with_frn(b"first", 0, false, 11);
        let (first, mut first_frns) = first_builder.build();
        let first_path = dir.path().join("first.rkyv");
        save_with_sidecar(&first, &mut first_frns, &first_path, |_| {}, |_| {}).unwrap();
        assert!(ArenaStore::open(&first_path).unwrap().frn_map.is_some());

        let mut second_builder = crate::arena::ArenaBuilder::default();
        second_builder.push_bytes_with_frn(b"second", 0, false, 22);
        let (second, mut second_frns) = second_builder.build();
        let second_path = dir.path().join("second.rkyv");
        save_with_sidecar(&second, &mut second_frns, &second_path, |_| {}, |_| {}).unwrap();

        std::fs::copy(
            second_path.with_extension("frn"),
            first_path.with_extension("frn"),
        )
        .unwrap();
        let reopened = ArenaStore::open(&first_path).unwrap();
        assert!(reopened.frn_map.is_none());
    }

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
