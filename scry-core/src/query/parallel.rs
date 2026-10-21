//! Bucket-aligned workers for unfiltered scans and bounded top-k selection.

use crate::arena::ArchivedArena;
use crate::cancel::Cancellation;

use super::is_cancelled_periodically;

/// Splits `[0, arena.len())` into up to `threads` bucket-aligned ranges, the
/// same shard unit `scan_full_parallel`/`scan_full_parallel_ranked` scan
/// independently: a front-coded bucket decodes from nothing outside itself,
/// so a shard boundary never needs synchronization with its neighbors.
pub(crate) fn bucket_shards(n: usize, threads: usize) -> impl Iterator<Item = (u32, u32)> {
    let num_buckets = n.div_ceil(crate::record::BUCKET_SIZE);
    let shard_buckets = num_buckets.div_ceil(threads.max(1)).max(1);
    (0..num_buckets)
        .step_by(shard_buckets)
        .map(move |start_bucket| {
            let end_bucket = (start_bucket + shard_buckets).min(num_buckets);
            let start = (start_bucket * crate::record::BUCKET_SIZE) as u32;
            let end = (end_bucket * crate::record::BUCKET_SIZE).min(n) as u32;
            (start, end)
        })
}

/// Runs `matches` over every name in `arena`, sharded across `threads`
/// bucket-aligned ranges. On cancellation the merged result is discarded
/// entirely (returns empty), matching the single-threaded scan's contract
/// that a superseded query yields no results rather than a partial set.
pub(super) fn scan_full_parallel(
    arena: &ArchivedArena,
    threads: usize,
    cancel: Option<Cancellation>,
    matches: impl Fn(&[u8]) -> bool + Sync,
) -> Vec<u32> {
    let n = arena.len();
    if n == 0 {
        return Vec::new();
    }
    let matches = &matches;
    let shards: Vec<Vec<u32>> = std::thread::scope(|scope| {
        bucket_shards(n, threads)
            .map(|(start, end)| {
                scope.spawn(move || {
                    let mut local = Vec::new();
                    let mut checked: u32 = 0;
                    let mut matched = false;
                    arena.for_each_name_in_with_repeats(start..end, |idx, name, repeated| {
                        if is_cancelled_periodically(cancel, &mut checked) {
                            return std::ops::ControlFlow::Break(());
                        }
                        if !repeated {
                            matched = matches(name);
                        }
                        if matched {
                            local.push(idx);
                        }
                        std::ops::ControlFlow::Continue(())
                    });
                    local
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return Vec::new();
    }
    shards.into_iter().flatten().collect()
}

/// As [`scan_full_parallel`], but ranks while it scans: each shard keeps only
/// its own top-`limit` in a bounded heap via [`rank::retain_hit`], and the
/// per-shard heaps are merged the same way. That merge is exact — any record
/// in the global top-`limit` must also be in its own shard's top-`limit`,
/// since a shard with `limit` or more strictly better matches would itself
/// disqualify it — so nothing outside the per-shard heaps needs to be kept.
/// Returns the merged heap and the total number of candidates seen (for
/// instrumentation), or `(empty, 0)` on cancellation.
pub(super) fn scan_full_parallel_ranked(
    arena: &ArchivedArena,
    threads: usize,
    cancel: Option<Cancellation>,
    limit: usize,
    is_match: impl Fn(&[u8]) -> bool + Sync,
    key_for: impl FnMut(u32, &[u8], bool) -> Option<u64> + Clone + Send,
) -> (std::collections::BinaryHeap<u64>, u64) {
    let n = arena.len();
    if n == 0 || limit == 0 {
        return (std::collections::BinaryHeap::new(), 0);
    }
    let is_match = &is_match;
    let shards: Vec<(std::collections::BinaryHeap<u64>, u64)> = std::thread::scope(|scope| {
        bucket_shards(n, threads)
            .map(|(start, end)| {
                let mut key_for = key_for.clone();
                scope.spawn(move || {
                    let mut heap = std::collections::BinaryHeap::new();
                    let mut seen: u64 = 0;
                    let mut checked: u32 = 0;
                    let mut matched = false;
                    arena.for_each_name_in_with_repeats(start..end, |idx, name, repeated| {
                        if is_cancelled_periodically(cancel, &mut checked) {
                            return std::ops::ControlFlow::Break(());
                        }
                        if !repeated {
                            matched = is_match(name);
                        }
                        if matched {
                            if let Some(key) = key_for(idx, name, repeated) {
                                seen += 1;
                                crate::rank::retain_hit(&mut heap, key, limit);
                            }
                        }
                        std::ops::ControlFlow::Continue(())
                    });
                    (heap, seen)
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    if cancel.is_some_and(|c| c.is_cancelled()) {
        return (std::collections::BinaryHeap::new(), 0);
    }
    let mut merged = std::collections::BinaryHeap::with_capacity(limit.min(4096));
    let mut total = 0u64;
    for (heap, seen) in shards {
        total += seen;
        for key in heap {
            crate::rank::retain_hit(&mut merged, key, limit);
        }
    }
    (merged, total)
}
