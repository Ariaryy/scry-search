//! A sorted, coalesced set of half-open `u32` ranges over DFS positions.
//!
//! Path-term matching builds one `IntervalSet` per term (a directory match
//! contributes its whole `dfs_position..dfs_end` subtree span, a leaf match
//! contributes a single point) and then intersects the per-term sets to find
//! records whose DFS position satisfies every term. Runs are coalesced
//! eagerly so both `len_positions` and `intersect_into` stay linear in the
//! number of runs rather than the number of positions.

/// A sorted, non-overlapping, non-touching set of half-open `[start, end)`
/// ranges.
#[derive(Debug, Default, Clone)]
pub struct IntervalSet {
    runs: Vec<(u32, u32)>,
    /// Runs pushed since the last [`Self::coalesce`] — `push_span`/
    /// `push_point` append here in whatever order the caller matches
    /// records, so a scan never pays sorting cost until it actually needs
    /// the coalesced form.
    pending: Vec<(u32, u32)>,
}

impl IntervalSet {
    pub fn clear(&mut self) {
        self.runs.clear();
        self.pending.clear();
    }

    /// True once coalesced. Before `coalesce`, `pending` may hold entries
    /// even when `runs` is empty, so this only reflects the caller's own
    /// bookkeeping after a `coalesce` call.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty() && self.pending.is_empty()
    }

    /// Queue a half-open span for the next [`Self::coalesce`]. `start >= end`
    /// is silently ignored (an empty span contributes nothing).
    pub fn push_span(&mut self, start: u32, end: u32) {
        if start < end {
            self.pending.push((start, end));
        }
    }

    pub fn push_point(&mut self, position: u32) {
        self.push_span(position, position + 1);
    }

    /// Sort and merge every queued span (plus any already-coalesced runs)
    /// into the minimal non-touching run list.
    pub fn coalesce(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        self.runs.append(&mut self.pending);
        self.runs.sort_unstable();
        let mut write = 0;
        for read in 1..self.runs.len() {
            let (start, end) = self.runs[read];
            let (_, last_end) = self.runs[write];
            if start <= last_end {
                self.runs[write].1 = self.runs[write].1.max(end);
            } else {
                write += 1;
                self.runs[write] = (start, end);
            }
        }
        self.runs.truncate(write + 1);
    }

    /// Total number of individual positions covered. Only meaningful after
    /// [`Self::coalesce`].
    pub fn len_positions(&self) -> u64 {
        self.runs
            .iter()
            .map(|&(start, end)| (end - start) as u64)
            .sum()
    }

    pub fn runs(&self) -> &[(u32, u32)] {
        &self.runs
    }

    /// Binary search over the coalesced runs. Only meaningful after
    /// [`Self::coalesce`].
    pub fn contains(&self, position: u32) -> bool {
        self.runs
            .binary_search_by(|&(start, end)| {
                if position < start {
                    std::cmp::Ordering::Greater
                } else if position >= end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    /// Iterate every individual position in ascending order. Only
    /// meaningful after [`Self::coalesce`]; intended for small result sets,
    /// not as a substitute for run-level operations on large ones.
    pub fn iter_positions(&self) -> impl Iterator<Item = u32> + '_ {
        self.runs.iter().flat_map(|&(start, end)| start..end)
    }

    /// Intersect two coalesced sets into `out` (cleared first) via a
    /// two-pointer merge over runs. `self` and `other` must already be
    /// coalesced; `out` comes out coalesced too since overlapping runs
    /// on each side never produce touching output runs from adjacent
    /// input pairs — the smallest possible gap between two intersection
    /// results is bounded by the gap between their source runs.
    pub fn intersect_into(&self, other: &IntervalSet, out: &mut IntervalSet) {
        out.clear();
        let (mut i, mut j) = (0, 0);
        let (a, b) = (&self.runs, &other.runs);
        while i < a.len() && j < b.len() {
            let (a_start, a_end) = a[i];
            let (b_start, b_end) = b[j];
            let start = a_start.max(b_start);
            let end = a_end.min(b_end);
            if start < end {
                out.runs.push((start, end));
            }
            if a_end < b_end {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn to_hash_set(set: &IntervalSet) -> HashSet<u32> {
        set.iter_positions().collect()
    }

    fn next_rand(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn coalesce_merges_overlapping_and_touching_spans() {
        let mut set = IntervalSet::default();
        set.push_span(10, 20);
        set.push_span(20, 25); // touching, must merge
        set.push_span(5, 12); // overlapping, must merge
        set.push_span(100, 110); // disjoint, stays separate
        set.coalesce();
        assert_eq!(set.runs(), &[(5, 25), (100, 110)]);
        assert_eq!(set.len_positions(), 20 + 10);
    }

    #[test]
    fn push_point_is_a_unit_span() {
        let mut set = IntervalSet::default();
        set.push_point(7);
        set.coalesce();
        assert_eq!(set.runs(), &[(7, 8)]);
        assert!(set.contains(7));
        assert!(!set.contains(6));
        assert!(!set.contains(8));
    }

    #[test]
    fn empty_span_contributes_nothing() {
        let mut set = IntervalSet::default();
        set.push_span(5, 5);
        set.push_span(9, 3);
        set.coalesce();
        assert!(set.is_empty());
    }

    #[test]
    fn contains_matches_a_brute_force_membership_check() {
        let mut set = IntervalSet::default();
        for &(start, end) in &[(0u32, 3u32), (5, 6), (10, 20), (19, 25)] {
            set.push_span(start, end);
        }
        set.coalesce();
        let expected: HashSet<u32> = [
            0, 1, 2, 5, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        ]
        .into_iter()
        .collect();
        for position in 0..30 {
            assert_eq!(
                set.contains(position),
                expected.contains(&position),
                "position {position}"
            );
        }
    }

    #[test]
    fn intersect_into_matches_a_hashset_reference_over_random_spans() {
        let mut state = 0x243F_6A88_85A3_08D3u64;
        for _ in 0..500 {
            let mut a = IntervalSet::default();
            let mut b = IntervalSet::default();
            for _ in 0..(next_rand(&mut state) % 12 + 1) {
                let start = (next_rand(&mut state) % 100) as u32;
                let len = (next_rand(&mut state) % 15 + 1) as u32;
                a.push_span(start, start + len);
            }
            for _ in 0..(next_rand(&mut state) % 12 + 1) {
                let start = (next_rand(&mut state) % 100) as u32;
                let len = (next_rand(&mut state) % 15 + 1) as u32;
                b.push_span(start, start + len);
            }
            a.coalesce();
            b.coalesce();

            let expected: HashSet<u32> = to_hash_set(&a)
                .intersection(&to_hash_set(&b))
                .copied()
                .collect();

            let mut out = IntervalSet::default();
            a.intersect_into(&b, &mut out);
            let actual = to_hash_set(&out);
            assert_eq!(actual, expected, "a={:?} b={:?}", a.runs(), b.runs());

            // The result must itself be a valid coalesced run list: sorted,
            // non-overlapping, non-touching.
            for pair in out.runs().windows(2) {
                assert!(
                    pair[0].1 < pair[1].0,
                    "runs should not touch: {:?}",
                    out.runs()
                );
            }
        }
    }

    #[test]
    fn intersect_into_with_an_empty_set_is_empty() {
        let mut a = IntervalSet::default();
        a.push_span(0, 10);
        a.coalesce();
        let b = IntervalSet::default();
        let mut out = IntervalSet::default();
        a.intersect_into(&b, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn intersect_is_commutative() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..200 {
            let mut a = IntervalSet::default();
            let mut b = IntervalSet::default();
            for _ in 0..(next_rand(&mut state) % 8 + 1) {
                let start = (next_rand(&mut state) % 50) as u32;
                let len = (next_rand(&mut state) % 10 + 1) as u32;
                a.push_span(start, start + len);
            }
            for _ in 0..(next_rand(&mut state) % 8 + 1) {
                let start = (next_rand(&mut state) % 50) as u32;
                let len = (next_rand(&mut state) % 10 + 1) as u32;
                b.push_span(start, start + len);
            }
            a.coalesce();
            b.coalesce();

            let mut ab = IntervalSet::default();
            a.intersect_into(&b, &mut ab);
            let mut ba = IntervalSet::default();
            b.intersect_into(&a, &mut ba);
            assert_eq!(ab.runs(), ba.runs());
        }
    }
}
