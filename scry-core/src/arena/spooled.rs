//! Bounded-memory construction from sorted records into file-backed columns.

use crate::record::{bytes_to_size_kib, pack_parent, word_is_dir, BUCKET_SIZE};
use crate::spool::{ByteSpool, Spool};
use crate::trigram::{for_each_trigram, num_blocks, row_bytes, TRIGRAM_BLOCK, TRIGRAM_ROWS};
use std::fs::File;
use std::io;
use std::path::Path;

use super::bitmap_get;
use super::names::common_prefix_len;

/// Spool-backed name encoder for [`SpooledArenaBuilder`]: same
/// incremental front-coding and bucket-boundary logic, byte-identical output
/// to the owned encoder given the same names in the same order, but `blob` and
/// `offsets` are mmap-backed instead of heap `Vec`s. `prev` stays a small
/// heap `Vec` — bounded by one name's length, not by record count.
struct SpooledFrontCoder {
    blob: ByteSpool,
    offsets: Spool<u32>,
    prev: Vec<u8>,
    pos_in_bucket: usize,
    absolute_index: usize,
}

impl SpooledFrontCoder {
    fn push(&mut self, name: &[u8], trigram_index: &mut Spool<u8>, row_len: usize) {
        index_trigrams_spooled(self.absolute_index, name, trigram_index, row_len);
        if self.pos_in_bucket == 0 {
            self.offsets.push(self.blob.len() as u32);
            write_varint_spooled(&mut self.blob, name.len() as u32);
            self.blob.extend_from_slice(name);
        } else {
            let shared = common_prefix_len(&self.prev, name);
            write_varint_spooled(&mut self.blob, shared as u32);
            let suffix = &name[shared..];
            write_varint_spooled(&mut self.blob, suffix.len() as u32);
            self.blob.extend_from_slice(suffix);
        }
        self.prev.clear();
        self.prev.extend_from_slice(name);
        self.absolute_index += 1;
        self.pos_in_bucket += 1;
        if self.pos_in_bucket == BUCKET_SIZE {
            self.pos_in_bucket = 0;
        }
    }

    fn finish(mut self) -> (ByteSpool, Spool<u32>) {
        self.offsets.push(self.blob.len() as u32); // sentinel
        (self.blob, self.offsets)
    }
}

fn write_varint_spooled(out: &mut ByteSpool, mut v: u32) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        } else {
            out.push(byte | 0x80);
        }
    }
}

fn index_trigrams_spooled(index: usize, name: &[u8], matrix: &mut Spool<u8>, row_len: usize) {
    if matrix.is_empty() {
        return;
    }
    let block = index / TRIGRAM_BLOCK;
    for_each_trigram(name, |hash| {
        let i = hash as usize * row_len + block / 8;
        let byte = matrix.get(i);
        matrix.set(i, byte | (1 << (block % 8)));
    });
}

/// Builds an Arena's columns from records already known to be in final
/// sorted (name-ascending, tie-broken however the caller's merge already
/// resolved ties) order, writing every column straight to a [`Spool`]/
/// [`ByteSpool`] as records arrive rather than an owned `Vec` — so pushing
/// two million records costs a handful of resident mmap pages, not ~120 MB
/// of heap. FRN handling lives entirely outside this type now: a record's
/// FRN sidecar entry depends on its *name-sorted* final index, which the
/// sidecar needs in FRN order, not push order, so `IndexView::compact_to_snapshot`
/// produces it as a separate frn-sorted merge (see `crate::frnmap::FrnMap::save_streaming`)
/// instead of collecting entries here.
///
/// This exists for `IndexView::compact_to_snapshot`, which can produce
/// records in final order cheaply (base survivors are already name-sorted on
/// disk; delta additions are a small in-memory list, sorted once) via a merge
/// of the two streams — so paying for `ArenaBuilder`'s full stage-then-sort
/// pass would duplicate work the caller already did. It intentionally does
/// not replace `ArenaBuilder`: callers that push records in arbitrary order
/// (the MFT/USN enumerators) still need a real sort, and that path's memory
/// profile is out of scope here (see AGENTS.md).
///
/// A record's parent may not have been pushed yet when the record itself is
/// pushed (name order and tree order are unrelated), so `push` accepts a
/// caller-defined *raw* parent marker instead of a final index, and
/// `patch_parent` overwrites it once the parent's final index is known. This
/// mirrors `ArenaBuilder::push_bytes_with_metadata` + `set_parent`, except
/// here a record's own final index is known at push time (it is just the
/// push count), so there is no separate order/rank permutation to build.
pub struct SpooledArenaBuilder {
    front_coder: SpooledFrontCoder,
    parents: Spool<u32>,
    mtimes: Spool<u32>,
    sizes: Spool<u32>,
    size_exact_inputs: Spool<u8>,
    trigram_index: Spool<u8>,
    row_len: usize,
    journal_id: u64,
    next_usn: i64,
    volume_serial: u64,
}

/// The finished columns, still spool-backed, ready for `dfs::build_file_backed`
/// and `store::save_columns_with` — never assembled into an owned `Arena`.
pub struct SpooledColumns {
    pub names: ByteSpool,
    pub bucket_offsets: Spool<u32>,
    pub parents: Spool<u32>,
    pub mtimes: Spool<u32>,
    pub sizes: Spool<u32>,
    pub size_exact_inputs: Spool<u8>,
    pub trigram_index: Spool<u8>,
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_serial: u64,
}

impl SpooledArenaBuilder {
    /// `total_entries` must be the exact final record count — it sizes the
    /// trigram block filter and every fixed-capacity column spool up front.
    /// `dir` holds this compaction's scratch files; `on_create` runs against
    /// every one of them, so a caller can mark them auxiliary before they
    /// can generate a watched USN event, same as the final snapshot file.
    pub fn new(total_entries: usize, dir: &Path, on_create: &impl Fn(&File)) -> io::Result<Self> {
        let blocks = num_blocks(total_entries);
        let row_len = row_bytes(blocks);
        let trigram_len = if total_entries < TRIGRAM_BLOCK {
            0
        } else {
            TRIGRAM_ROWS * row_len
        };
        Ok(SpooledArenaBuilder {
            front_coder: SpooledFrontCoder {
                blob: ByteSpool::create(&dir.join("names.spool"), 4096, |f| on_create(f))?,
                offsets: Spool::create(
                    &dir.join("bucket-offsets.spool"),
                    total_entries.div_ceil(BUCKET_SIZE) + 1,
                    |f| on_create(f),
                )?,
                prev: Vec::new(),
                pos_in_bucket: 0,
                absolute_index: 0,
            },
            parents: Spool::create(&dir.join("parents.spool"), total_entries, |f| on_create(f))?,
            mtimes: Spool::create(&dir.join("mtimes.spool"), total_entries, |f| on_create(f))?,
            sizes: Spool::create(&dir.join("sizes.spool"), total_entries, |f| on_create(f))?,
            size_exact_inputs: Spool::zeroed(
                &dir.join("size-exact-inputs.spool"),
                total_entries.div_ceil(8),
                |f| on_create(f),
            )?,
            trigram_index: Spool::zeroed(&dir.join("trigram.spool"), trigram_len, |f| {
                on_create(f)
            })?,
            row_len,
            journal_id: 0,
            next_usn: 0,
            volume_serial: 0,
        })
    }

    pub fn set_snapshot_cursor(&mut self, journal_id: u64, next_usn: i64, volume_serial: u64) {
        self.journal_id = journal_id;
        self.next_usn = next_usn;
        self.volume_serial = volume_serial;
    }

    /// Pushes the next record in final sorted order. `raw_parent` is an
    /// opaque, caller-chosen placeholder (`PARENT_NONE` for no parent) —
    /// this type never interprets it, it only round-trips it back through
    /// `patch_parent`'s `idx` until the caller overwrites it with a real
    /// final index. Returns this record's final index.
    pub fn push(
        &mut self,
        name: &[u8],
        mtime_secs: u32,
        is_dir: bool,
        size_bytes: u64,
        size_exact: bool,
        raw_parent: u32,
    ) -> u32 {
        let idx = self.parents.len() as u32;
        self.front_coder
            .push(name, &mut self.trigram_index, self.row_len);
        self.parents.push(pack_parent(raw_parent, is_dir));
        self.mtimes.push(mtime_secs);
        self.sizes.push(bytes_to_size_kib(size_bytes));
        if size_exact {
            let byte = idx as usize / 8;
            self.size_exact_inputs.set(
                byte,
                self.size_exact_inputs.get(byte) | (1 << (idx as usize % 8)),
            );
        }
        idx
    }

    /// Overwrites the parent index left at `idx` by `push` (its directory
    /// flag is preserved) with a resolved final index, or `PARENT_NONE`.
    pub fn patch_parent(&mut self, idx: u32, final_parent: u32) {
        let is_dir = word_is_dir(self.parents.get(idx as usize));
        self.parents
            .set(idx as usize, pack_parent(final_parent, is_dir));
    }

    /// Ends the push/patch phase and hands back the finished, still
    /// spool-backed columns for the caller's next pass (`dfs::build_file_backed`
    /// followed by `store::save_columns_with`).
    pub fn finish(self) -> SpooledColumns {
        let (names, bucket_offsets) = self.front_coder.finish();
        SpooledColumns {
            names,
            bucket_offsets,
            parents: self.parents,
            mtimes: self.mtimes,
            sizes: self.sizes,
            size_exact_inputs: self.size_exact_inputs,
            trigram_index: self.trigram_index,
            journal_id: self.journal_id,
            next_usn: self.next_usn,
            volume_serial: self.volume_serial,
        }
    }
}

pub fn derive_spooled_size_exact(
    parents: &[u32],
    raw: &Spool<u8>,
    dfs: &crate::dfs::FileBackedDfsLayout,
    dir: &Path,
    on_create: &impl Fn(&File),
) -> io::Result<Spool<u8>> {
    let bytes = parents.len().div_ceil(8);
    let mut unknown = Spool::zeroed(&dir.join("size-unknown-dfs.spool"), bytes, |f| on_create(f))?;
    for (record, &parent) in parents.iter().enumerate() {
        if !word_is_dir(parent) && !bitmap_get(raw.as_slice(), record) {
            let position = dfs.positions.get(record) as usize;
            let byte = position / 8;
            unknown.set(byte, unknown.get(byte) | (1 << (position % 8)));
        }
    }
    let samples = crate::bitvec::build_superblocks(unknown.as_slice());
    let rank = crate::bitvec::RankSelect::new(unknown.as_slice(), &samples);
    let mut result = Spool::zeroed(&dir.join("size-exact.spool"), bytes, |f| on_create(f))?;
    for (record, &parent) in parents.iter().enumerate() {
        let exact = if word_is_dir(parent) {
            let start = dfs.positions.get(record) as usize;
            let end = dfs.subtree_ends.get(record) as usize;
            rank.rank1(start) == rank.rank1(end)
        } else {
            bitmap_get(raw.as_slice(), record)
        };
        if exact {
            let byte = record / 8;
            result.set(byte, result.get(byte) | (1 << (record % 8)));
        }
    }
    Ok(result)
}
