//! Serialize borrowed columns directly into the persisted Arena layout.

use crate::arena::ArchivedArena;
use rkyv::vec::{ArchivedVec, VecResolver};

/// Stand-in for `&Arena` that lets compaction emit the v10 archive without
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
