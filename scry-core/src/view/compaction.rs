//! File-backed snapshot and FRN compaction.

use std::fs::File;
use std::path::Path;

use super::IndexView;
use crate::arena::SpooledArenaBuilder;
use crate::delta::ParentRef;
use crate::dfs;
use crate::frnmap::{FrnEntry, FrnMap};
use crate::store::{self, ArenaColumns, StoreError};
use crate::PARENT_NONE;

impl IndexView {
    /// Merge the overlay into a new base without re-enumerating the volume.
    ///
    /// Streams records straight into final sorted order instead of staging
    /// then sorting: base survivors are already name-sorted on disk, and the
    /// (small) set of live delta additions is sorted once in memory, so a
    /// two-way merge on name (ASCII case-insensitive, base wins ties — the
    /// old `ArenaBuilder::build()` path always put base pushes ahead of
    /// delta pushes for equal names) produces final order directly. This
    /// avoids `ArenaBuilder::build()`'s `order`/`rank` permutation arrays and
    /// its `staging_names` copy, both `O(total name bytes)`.
    ///
    /// Every record is pushed with a `PARENT_NONE` placeholder parent
    /// (`SpooledArenaBuilder::push`'s own final index — the push count — is
    /// already known, so nothing needs to round-trip through a raw marker);
    /// a second pass then patches every live record's real parent. A
    /// record's own final index and its parent's final index are each
    /// recomputed by formula in that second pass rather than looked up in a
    /// stored old-to-new map: a base index's final position is
    /// `base_new_index` (the existing tombstone-rank formula) plus the
    /// count of live delta names sorting before it (`delta_before_count`,
    /// binary search into the small sorted delta list); a delta index's
    /// final position is the count of live base names sorting before it
    /// (`base_before_count`, `ArchivedArena::lower_bound_name` binary search
    /// into the on-disk base, cheap because it runs once per delta addition,
    /// not once per base record) plus its rank within the sorted delta list.
    /// This costs a second `O(n)` walk over base but no `O(n)`-sized array.
    ///
    /// FRNs are handled entirely separately from the push/patch phase above,
    /// since a sidecar entry needs frn order, not push (name) order: the
    /// base sidecar is already frn-sorted on disk (`FrnMap::save_with`'s
    /// invariant), so filtering out tombstoned entries and remapping the
    /// survivors' indices through `final_index_of_base` preserves that
    /// order — a filtered subsequence of a sorted sequence stays sorted.
    /// That frn-ascending stream is then merged with the small
    /// (compaction-threshold-bounded) sorted list of live delta FRNs and
    /// written straight to the sidecar by `FrnMap::save_streaming`, without
    /// ever collecting a full-size `Vec<FrnEntry>` (the `by_index` vector
    /// this function used to build).
    ///
    /// Every temporary file this creates lives under `scratch_dir` and is
    /// deleted automatically when its `Spool`/`ByteSpool` drops; `on_create`
    /// runs against every one of them (scratch and final alike) so a caller
    /// can mark them auxiliary before they can generate a watched USN
    /// journal entry.
    /// `on_phase` fires with a short label after each major phase (merge,
    /// dfs build, prefix sums, final serialize, frn merge) — a hook for a
    /// caller to sample process memory between phases, so a future hidden
    /// `O(n)` heap allocation shows up as a spike attributable to one named
    /// phase instead of an unexplained total. No-op cost for a caller that
    /// passes an empty closure.
    pub fn compact_to_snapshot(
        &self,
        scratch_dir: &Path,
        snapshot_path: &Path,
        on_create: &impl Fn(&File),
        on_phase: &mut impl FnMut(&str),
    ) -> Result<(), StoreError> {
        let arena = self.base.archived();

        // Rank query over the tombstone bitset in place of a dense
        // `old index -> new index` array: compaction only removes base
        // records (it never reorders the survivors relative to each other),
        // so a live base record's new index is just its old index minus the
        // number of tombstones before it. `tomb_superblocks` costs ~4 bytes
        // per 256 records (~31 KB at 2M records) versus the 16 MB a
        // `Vec<Option<u32>>` over every base record cost.
        let tomb_superblocks = crate::bitvec::build_superblocks(self.delta.tombstones.as_bytes());
        let tomb_rank =
            crate::bitvec::RankSelect::new(self.delta.tombstones.as_bytes(), &tomb_superblocks);
        let base_new_index = |old: u32| -> Option<u32> {
            if self.delta.tombstones.get(old) {
                None
            } else {
                Some(old - tomb_rank.rank1(old as usize) as u32)
            }
        };

        let mut frns_by_delta = vec![None; self.delta.added.len()];
        for (&frn, &index) in &self.delta.added_frns {
            frns_by_delta[index as usize] = Some(frn);
        }

        // Live delta additions, sorted once by name (base wins ties, so this
        // list only needs to be internally consistent, not tie-broken
        // against base). Small — bounded by the compaction threshold, not by
        // base's size — so an `O(d log d)` sort here is cheap.
        let mut delta_sorted: Vec<u32> = self.delta.live_added().map(|(idx, _)| idx).collect();
        delta_sorted.sort_unstable_by(|&a, &b| {
            crate::ascii::cmp_ci(
                self.delta.added[a as usize].name.as_bytes(),
                self.delta.added[b as usize].name.as_bytes(),
            )
        });
        // Rank of each live delta index within `delta_sorted` — the delta
        // half of a final index, symmetric with `base_new_index` for base.
        let mut delta_rank = vec![0u32; self.delta.added.len()];
        for (rank, &idx) in delta_sorted.iter().enumerate() {
            delta_rank[idx as usize] = rank as u32;
        }
        // Count of live delta names strictly less than `name` — the delta
        // contribution to a base record's final index.
        let delta_before_count = |name: &[u8]| -> u32 {
            delta_sorted.partition_point(|&idx| {
                crate::ascii::cmp_ci(self.delta.added[idx as usize].name.as_bytes(), name)
                    == std::cmp::Ordering::Less
            }) as u32
        };
        // Count of live base names strictly less than `name` — the base
        // contribution to a delta record's final index.
        let base_before_count = |name: &[u8]| -> u32 {
            let boundary = arena.lower_bound_name(name);
            boundary - tomb_rank.rank1(boundary as usize) as u32
        };
        let final_index_of_base = |old: u32, name_buf: &mut Vec<u8>| -> Option<u32> {
            let new = base_new_index(old)?;
            arena.name_into(old, name_buf);
            Some(new + delta_before_count(name_buf))
        };
        let final_index_of_delta = |idx: u32| -> u32 {
            base_before_count(self.delta.added[idx as usize].name.as_bytes())
                + delta_rank[idx as usize]
        };

        let live_base = arena.len() - self.delta.tombstones.count_ones() as usize;
        let live_delta = delta_sorted.len();
        let mut builder = SpooledArenaBuilder::new(live_base + live_delta, scratch_dir, on_create)?;
        builder.set_snapshot_cursor(self.journal_id, self.next_usn, self.volume_serial);

        {
            let mut base_old = 0u32;
            while base_old < arena.len() as u32 && self.delta.tombstones.get(base_old) {
                base_old += 1;
            }
            let mut delta_i = 0usize;
            let mut base_name = Vec::new();

            loop {
                let base_has = base_old < arena.len() as u32;
                let delta_has = delta_i < delta_sorted.len();
                if !base_has && !delta_has {
                    break;
                }
                if base_has {
                    arena.name_into(base_old, &mut base_name);
                }
                let take_base = if base_has && delta_has {
                    let delta_name = self.delta.added[delta_sorted[delta_i] as usize]
                        .name
                        .as_bytes();
                    crate::ascii::cmp_ci(&base_name, delta_name) != std::cmp::Ordering::Greater
                } else {
                    base_has
                };

                if take_base {
                    builder.push(
                        &base_name,
                        arena.mtime(base_old),
                        arena.is_dir(base_old),
                        arena.size_bytes(base_old),
                        arena.size_exact(base_old),
                        PARENT_NONE,
                    );
                    base_old += 1;
                    while base_old < arena.len() as u32 && self.delta.tombstones.get(base_old) {
                        base_old += 1;
                    }
                } else {
                    let delta_idx = delta_sorted[delta_i];
                    let record = &self.delta.added[delta_idx as usize];
                    builder.push(
                        record.name.as_bytes(),
                        record.mtime_secs,
                        record.is_dir,
                        record.size_bytes,
                        record.size_exact,
                        PARENT_NONE,
                    );
                    delta_i += 1;
                }
            }
        }

        let mut name_buf = Vec::new();
        let mut parent_name_buf = Vec::new();
        for old in 0..arena.len() as u32 {
            let Some(final_self) = final_index_of_base(old, &mut name_buf) else {
                continue;
            };
            let parent = arena.parent(old);
            if parent == PARENT_NONE {
                continue;
            }
            if let Some(final_parent) = final_index_of_base(parent, &mut parent_name_buf) {
                builder.patch_parent(final_self, final_parent);
            }
        }
        for (idx, record) in self.delta.live_added() {
            let final_self = final_index_of_delta(idx);
            let final_parent = match record.parent {
                ParentRef::Base(index) => final_index_of_base(index, &mut name_buf),
                ParentRef::Delta(index) => self.delta.added[index as usize]
                    .live
                    .then(|| final_index_of_delta(index)),
                ParentRef::None => None,
            };
            if let Some(final_parent) = final_parent {
                builder.patch_parent(final_self, final_parent);
            }
        }

        on_phase("merge");
        let columns = builder.finish();
        let dfs_layout =
            dfs::build_file_backed(columns.parents.as_slice(), scratch_dir, on_create)?;
        let size_exact_bits = crate::arena::derive_spooled_size_exact(
            columns.parents.as_slice(),
            &columns.size_exact_inputs,
            &dfs_layout,
            scratch_dir,
            on_create,
        )?;
        on_phase("dfs_build");
        let dfs_size_prefix = dfs::prefix_sums_u64_file_backed(
            &dfs_layout.records,
            &columns.sizes,
            &scratch_dir.join("dfs-size-prefix.spool"),
            on_create,
        )?;
        on_phase("prefix_sums");

        // Small (delta-threshold-bounded), sorted by frn: the delta half of
        // the sidecar merge.
        let mut delta_frn_entries: Vec<FrnEntry> = self
            .delta
            .live_added()
            .filter_map(|(idx, _)| {
                frns_by_delta[idx as usize].map(|frn| FrnEntry {
                    frn,
                    index: final_index_of_delta(idx),
                    _pad: 0,
                })
            })
            .collect();
        delta_frn_entries.sort_unstable_by_key(|entry| entry.frn);

        // The base half: the on-disk sidecar is already frn-sorted
        // (`FrnMap::save_with`'s invariant), so filtering out tombstoned
        // entries and remapping survivors through `final_index_of_base`
        // preserves that order without ever collecting a full-size Vec.
        let mut base_name_buf = Vec::new();
        let final_index_of_base_ref = &final_index_of_base;
        let mut base_frn_iter = self
            .base
            .frn_map
            .as_ref()
            .map(|map| map.iter())
            .into_iter()
            .flatten()
            .filter_map(move |entry| {
                final_index_of_base_ref(entry.index, &mut base_name_buf).map(|index| FrnEntry {
                    frn: entry.frn,
                    index,
                    _pad: 0,
                })
            })
            .peekable();
        let mut delta_frn_iter = delta_frn_entries.into_iter().peekable();
        let merged_frns =
            std::iter::from_fn(
                move || match (base_frn_iter.peek(), delta_frn_iter.peek()) {
                    (Some(b), Some(d)) => {
                        if b.frn <= d.frn {
                            base_frn_iter.next()
                        } else {
                            delta_frn_iter.next()
                        }
                    }
                    (Some(_), None) => base_frn_iter.next(),
                    (None, Some(_)) => delta_frn_iter.next(),
                    (None, None) => None,
                },
            );
        // Snapshot and sidecar are separate files and therefore cannot be
        // renamed atomically as a pair. Give both this compaction generation
        // the same nonzero tag. Opening a snapshot ignores any sidecar whose
        // tag differs, so a crash between the two renames cannot apply FRNs
        // containing record indices from one generation to another.
        let snapshot_tag = store::fresh_snapshot_tag();
        store::save_columns_with_tag(
            &ArenaColumns {
                format_version: crate::record::FORMAT_VERSION,
                journal_id: columns.journal_id,
                next_usn: columns.next_usn,
                volume_serial: columns.volume_serial,
                names: columns.names.as_slice(),
                bucket_offsets: columns.bucket_offsets.as_slice(),
                parents: columns.parents.as_slice(),
                mtimes: columns.mtimes.as_slice(),
                sizes: columns.sizes.as_slice(),
                size_exact_bits: size_exact_bits.as_slice(),
                trigram_index: columns.trigram_index.as_slice(),
                dfs_positions: dfs_layout.positions.as_slice(),
                dfs_records: dfs_layout.records.as_slice(),
                dfs_ends: dfs_layout.subtree_ends.as_slice(),
                dfs_size_prefix: dfs_size_prefix.as_slice(),
            },
            snapshot_path,
            snapshot_tag,
            on_create,
        )?;
        on_phase("serialize");

        FrnMap::save_streaming(
            &snapshot_path.with_extension("frn"),
            merged_frns,
            snapshot_tag,
            on_create,
        )?;
        on_phase("frn_merge");

        Ok(())
    }
}
