use crate::record::{unpack_parent, PARENT_NONE};
use crate::spool::Spool;
use std::fs::File;

use super::*;
use crate::record::pack_parent;

/// `positions` must be a bijection onto `0..n` and agree with `records`,
/// or an interval test would silently address the wrong record.
fn assert_is_permutation(layout: &DfsLayout, n: usize) {
    assert_eq!(layout.records.len(), n);
    let mut seen = vec![false; n];
    for (position, &record) in layout.records.iter().enumerate() {
        assert!(!seen[record as usize], "record {record} placed twice");
        seen[record as usize] = true;
        assert_eq!(layout.positions[record as usize], position as u32);
    }
    assert!(seen.into_iter().all(|hit| hit), "a record got no position");
}

/// The defining property: a record is a descendant of `d` exactly when its
/// position lies in `d`'s interval. Checked against an independent
/// ancestor walk over the same parent column.
fn assert_intervals_match_ancestor_walk(parents: &[u32], layout: &DfsLayout) {
    let n = parents.len();
    for ancestor in 0..n as u32 {
        let span = layout.positions[ancestor as usize]..layout.subtree_ends[ancestor as usize];
        for record in 0..n as u32 {
            let mut current = record;
            let mut is_descendant = false;
            for _ in 0..=n {
                if current == ancestor {
                    is_descendant = true;
                    break;
                }
                let parent = unpack_parent(parents[current as usize]);
                if parent == PARENT_NONE || parent as usize >= n || parent == current {
                    break;
                }
                current = parent;
            }
            let in_span = span.contains(&layout.positions[record as usize]);
            if is_descendant {
                assert!(
                    in_span,
                    "record {record} descends from {ancestor} but sits outside its interval"
                );
            }
            // The converse only holds for a well-formed forest; a cycle
            // member entered as a pseudo-root legitimately sits inside an
            // interval it does not descend from.
        }
    }
}

fn forest(edges: &[(u32, bool)]) -> Vec<u32> {
    edges
        .iter()
        .map(|&(parent, is_dir)| pack_parent(parent, is_dir))
        .collect()
}

#[test]
fn empty_parent_column_produces_an_empty_layout() {
    let layout = build(&[]);
    assert!(layout.records.is_empty());
    assert!(layout.positions.is_empty());
}

#[test]
fn subtree_interval_covers_exactly_the_descendants() {
    // 0 root, 1 and 2 children of 0, 3 and 4 children of 1.
    let parents = forest(&[
        (PARENT_NONE, true),
        (0, true),
        (0, false),
        (1, false),
        (1, false),
    ]);
    let layout = build(&parents);
    assert_is_permutation(&layout, parents.len());
    assert_intervals_match_ancestor_walk(&parents, &layout);

    let root_span = layout.positions[0]..layout.subtree_ends[0];
    assert_eq!(root_span.len(), 5, "root's subtree is the whole forest");
    let one_span = layout.positions[1]..layout.subtree_ends[1];
    assert_eq!(one_span.len(), 3, "1 plus its two children");
    let leaf_span = layout.positions[2]..layout.subtree_ends[2];
    assert_eq!(leaf_span.len(), 1, "a leaf's subtree is itself");
}

#[test]
fn descendant_count_is_the_interval_width() {
    let parents = forest(&[
        (PARENT_NONE, true),
        (0, true),
        (1, true),
        (2, false),
        (0, false),
    ]);
    let layout = build(&parents);
    for (record, expected) in [(0u32, 5u32), (1, 3), (2, 2), (3, 1), (4, 1)] {
        assert_eq!(
            layout.subtree_ends[record as usize] - layout.positions[record as usize],
            expected,
            "record {record}"
        );
    }
}

#[test]
fn a_parent_cycle_still_yields_a_permutation() {
    // 0 -> 1 -> 2 -> 0, plus an unrelated root with a child.
    let parents = forest(&[
        (1, true),
        (2, true),
        (0, true),
        (PARENT_NONE, true),
        (3, false),
    ]);
    let layout = build(&parents);
    assert_is_permutation(&layout, parents.len());
}

#[test]
fn a_self_parent_is_treated_as_a_root() {
    let parents = forest(&[(0, true), (0, false)]);
    let layout = build(&parents);
    assert_is_permutation(&layout, parents.len());
    assert_eq!(layout.subtree_ends[0] - layout.positions[0], 2);
}

#[test]
fn a_dangling_parent_index_is_treated_as_a_root() {
    let parents = forest(&[(9_999, true), (0, false)]);
    let layout = build(&parents);
    assert_is_permutation(&layout, parents.len());
    assert_intervals_match_ancestor_walk(&parents, &layout);
}

/// A long chain must not recurse — this is the shape that would overflow
/// the stack if `walk` were written recursively.
#[test]
fn a_deep_chain_does_not_overflow_the_stack() {
    const DEPTH: u32 = 200_000;
    let mut parents = vec![pack_parent(PARENT_NONE, true)];
    for index in 1..DEPTH {
        parents.push(pack_parent(index - 1, true));
    }
    let layout = build(&parents);
    assert_is_permutation(&layout, parents.len());
    assert_eq!(layout.subtree_ends[0] - layout.positions[0], DEPTH);
    assert_eq!(layout.subtree_ends[(DEPTH - 1) as usize], DEPTH);
}

/// Recursive size over nested directories: a subtree total must equal
/// the sum of every record beneath it, itself included.
#[test]
fn prefix_sums_give_correct_subtree_totals_for_nested_dirs() {
    // 0 root, 1 and 2 children of 0, 3 and 4 children of 1.
    let parents = forest(&[
        (PARENT_NONE, true),
        (0, true),
        (0, false),
        (1, false),
        (1, false),
    ]);
    let sizes = [0u32, 0, 5, 3, 7]; // dirs carry no own size in this model
    let layout = build(&parents);
    let prefix = prefix_sums_u64(&layout.records, &sizes);
    assert_eq!(prefix.len(), parents.len() + 1);

    let subtree_total = |record: usize| {
        prefix[layout.subtree_ends[record] as usize] - prefix[layout.positions[record] as usize]
    };
    assert_eq!(subtree_total(0), 15, "root covers every record");
    assert_eq!(subtree_total(1), 10, "record 1 plus its two children");
    assert_eq!(subtree_total(2), 5, "a leaf's subtree is itself");
    assert_eq!(subtree_total(3), 3);
    assert_eq!(subtree_total(4), 7);
}

/// A parent cycle still yields a total permutation, so the prefix-sum
/// invariant (subtree total is a valid, in-bounds subtraction) must keep
/// holding even when cycle members are pseudo-roots.
#[test]
fn prefix_sums_stay_valid_across_a_parent_cycle() {
    // 0 -> 1 -> 2 -> 0, plus an unrelated root with a child.
    let parents = forest(&[
        (1, true),
        (2, true),
        (0, true),
        (PARENT_NONE, true),
        (3, false),
    ]);
    let sizes = [1u32, 2, 3, 4, 5];
    let layout = build(&parents);
    assert_is_permutation(&layout, parents.len());
    let prefix = prefix_sums_u64(&layout.records, &sizes);
    assert_eq!(prefix.len(), parents.len() + 1);
    // Total across the whole tree order must equal the sum of every value,
    // regardless of how cycle members were rooted.
    assert_eq!(
        *prefix.last().unwrap(),
        sizes.iter().map(|&s| s as u64).sum::<u64>()
    );
    for record in 0..parents.len() {
        let total = prefix[layout.subtree_ends[record] as usize]
            - prefix[layout.positions[record] as usize];
        // Every record's own value is included in its own subtree total,
        // cycle member or not.
        assert!(total >= sizes[record] as u64);
    }
}

/// Randomised forests, checked against the independent ancestor walk.
#[test]
fn random_forests_match_an_ancestor_walk() {
    let mut state = 0x243F_6A88_85A3_08D3u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..200 {
        let n = 2 + (next() % 60) as usize;
        let parents: Vec<u32> = (0..n)
            .map(|index| {
                if index == 0 || next() % 4 == 0 {
                    pack_parent(PARENT_NONE, true)
                } else {
                    // Parent strictly earlier, so the graph stays acyclic
                    // and the converse of the interval property holds.
                    pack_parent((next() % index as u64) as u32, next() % 2 == 0)
                }
            })
            .collect();
        let layout = build(&parents);
        assert_is_permutation(&layout, n);
        assert_intervals_match_ancestor_walk(&parents, &layout);
    }
}

fn assert_file_backed_matches(parents: &[u32]) {
    let dir = tempfile::tempdir().unwrap();
    let expected = build(parents);
    let actual = build_file_backed(parents, dir.path(), &|_: &File| {}).unwrap();
    assert_eq!(actual.positions.as_slice(), expected.positions.as_slice());
    assert_eq!(actual.records.as_slice(), expected.records.as_slice());
    assert_eq!(
        actual.subtree_ends.as_slice(),
        expected.subtree_ends.as_slice()
    );
}

#[test]
fn file_backed_matches_the_in_memory_builder_on_an_empty_column() {
    assert_file_backed_matches(&[]);
}

#[test]
fn file_backed_matches_the_in_memory_builder_on_a_nested_forest() {
    let parents = forest(&[
        (PARENT_NONE, true),
        (0, true),
        (0, false),
        (1, false),
        (1, false),
    ]);
    assert_file_backed_matches(&parents);
}

#[test]
fn file_backed_matches_the_in_memory_builder_across_a_parent_cycle() {
    let parents = forest(&[
        (1, true),
        (2, true),
        (0, true),
        (PARENT_NONE, true),
        (3, false),
    ]);
    assert_file_backed_matches(&parents);
}

#[test]
fn file_backed_matches_the_in_memory_builder_with_a_self_parent_and_a_dangling_parent() {
    assert_file_backed_matches(&forest(&[(0, true), (0, false)]));
    assert_file_backed_matches(&forest(&[(9_999, true), (0, false)]));
}

/// Same shape as `a_deep_chain_does_not_overflow_the_stack`: the
/// file-backed traversal must stay iterative too.
#[test]
fn file_backed_handles_a_deep_chain_without_overflow() {
    const DEPTH: u32 = 200_000;
    let mut parents = vec![pack_parent(PARENT_NONE, true)];
    for index in 1..DEPTH {
        parents.push(pack_parent(index - 1, true));
    }
    assert_file_backed_matches(&parents);
}

#[test]
fn file_backed_random_forests_match_the_in_memory_builder() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..50 {
        let n = 2 + (next() % 60) as usize;
        let parents: Vec<u32> = (0..n)
            .map(|index| {
                if index == 0 || next() % 4 == 0 {
                    pack_parent(PARENT_NONE, true)
                } else {
                    pack_parent((next() % index as u64) as u32, next() % 2 == 0)
                }
            })
            .collect();
        assert_file_backed_matches(&parents);
    }
}

#[test]
fn file_backed_prefix_sums_match_the_in_memory_version() {
    let parents = forest(&[
        (PARENT_NONE, true),
        (0, true),
        (0, false),
        (1, false),
        (1, false),
    ]);
    let sizes = [0u32, 0, 5, 3, 7];
    let dir = tempfile::tempdir().unwrap();
    let expected_layout = build(&parents);
    let expected = prefix_sums_u64(&expected_layout.records, &sizes);

    let actual_layout = build_file_backed(&parents, dir.path(), &|_: &File| {}).unwrap();
    let mut sizes_spool: Spool<u32> =
        Spool::create(&dir.path().join("sizes.spool"), sizes.len(), |_| {}).unwrap();
    for &s in &sizes {
        sizes_spool.push(s);
    }
    let actual = prefix_sums_u64_file_backed(
        &actual_layout.records,
        &sizes_spool,
        &dir.path().join("prefix.spool"),
        &|_: &File| {},
    )
    .unwrap();
    assert_eq!(actual.as_slice(), expected.as_slice());
}
