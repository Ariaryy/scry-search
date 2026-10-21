use super::*;

/// `and_into`/`or_into` chunk 8 bytes at a time and handle any remainder
/// byte-wise; an off-by-one there silently drops trigram candidate blocks
/// rather than panicking, so this compares the vectorized path against a
/// naive byte-at-a-time reference across every length that exercises a
/// non-empty remainder.
#[test]
fn and_or_into_match_a_naive_byte_at_a_time_reference() {
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut next_byte = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state & 0xFF) as u8
    };
    for len in 0..40usize {
        let dst: Vec<u8> = (0..len).map(|_| next_byte()).collect();
        let src: Vec<u8> = (0..len).map(|_| next_byte()).collect();

        let mut got_and = dst.clone();
        and_into(&mut got_and, &src);
        let want_and: Vec<u8> = dst.iter().zip(&src).map(|(d, s)| d & s).collect();
        assert_eq!(got_and, want_and, "and_into mismatch at len {len}");

        let mut got_or = dst.clone();
        or_into(&mut got_or, &src);
        let want_or: Vec<u8> = dst.iter().zip(&src).map(|(d, s)| d | s).collect();
        assert_eq!(got_or, want_or, "or_into mismatch at len {len}");
    }
}

#[test]
fn varint_round_trips() {
    for v in [0u32, 1, 127, 128, 300, 70000, u32::MAX] {
        let mut buf = Vec::new();
        write_varint(&mut buf, v);
        let mut pos = 0;
        assert_eq!(read_varint(&buf, &mut pos), v);
        assert_eq!(pos, buf.len());
    }
}

#[test]
fn front_coding_round_trips_every_name() {
    // 200 names: IMG series, z series, short, long
    let mut raw: Vec<String> = (0..100).map(|i| format!("IMG_{i:04}.JPG")).collect();
    raw.extend((0..80).map(|i| format!("z{i}")));
    raw.push("a".to_string());
    raw.push("".to_string());
    raw.push("b".to_string());
    raw.extend((0..17).map(|i| format!("node_modules_{i:03}")));
    assert!(raw.len() >= 200); // sanity

    let mut b = ArenaBuilder::default();
    for name in &raw {
        b.push(name, 0, false);
    }
    let arena = b.build().0;

    // Reconstruct what the sorted order should be.
    let mut expected: Vec<String> = raw.clone();
    expected.sort_by(|a, b| {
        a.to_ascii_lowercase()
            .cmp(&b.to_ascii_lowercase())
            .then(a.cmp(b))
    });
    // deduplicate sort stably for tie-breaking by index (arena does it by original index)
    // Actually just check by name content not by index since we only care names decode correctly.

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.rkyv");
    save(&arena, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    assert_eq!(archived.len(), raw.len());
    for i in 0..archived.len() {
        let decoded = archived.name(i as u32);
        // Must be a valid name from our input set.
        assert!(
            raw.iter().any(|n| n == &decoded),
            "decoded name {decoded:?} not in input set (index {i})"
        );
    }

    // Also verify all names are present (no duplicates lost).
    let mut all_decoded: Vec<String> = (0..archived.len() as u32)
        .map(|i| archived.name(i))
        .collect();
    let mut all_raw = raw.clone();
    all_decoded.sort();
    all_raw.sort();
    assert_eq!(all_decoded, all_raw);
}

#[test]
fn bucket_offsets_are_monotonic_with_sentinel() {
    let arena = simple_arena(&["alpha", "beta", "gamma"]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.rkyv");
    save(&arena, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    let offsets: Vec<u32> = archived.bucket_offsets.iter().copied().collect();
    for w in offsets.windows(2) {
        assert!(w[0] <= w[1], "offsets not monotonic: {:?}", offsets);
    }
    assert_eq!(*offsets.last().unwrap() as usize, archived.names.len());
}

#[test]
fn prefix_range_matches_a_brute_force_scan() {
    let names: Vec<String> = (0..100)
        .flat_map(|i| {
            vec![
                format!("img_{i:04}.jpg"),
                format!("IMG archive {i:04}.png"),
                format!("readme_{i}.txt"),
                format!("readme-{i:03}-final.md"),
                format!("alpha.beta.{i:03}"),
                format!("alphabet_{i:03}"),
                format!("z_{i}"),
                format!("éclair_{i:03}"),
            ]
        })
        .collect();

    let mut b = ArenaBuilder::default();
    for n in &names {
        b.push(n, 0, false);
    }
    let arena = b.build().0;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.rkyv");
    save(&arena, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    for prefix in &[
        "img",
        "IMG ",
        "readme",
        "README_0",
        "alpha.",
        "alphabet_09",
        "z_",
        "éclair_",
        "x",
        "",
    ] {
        let range = archived.prefix_range(prefix);
        let mut range_names: Vec<String> = range.map(|i| archived.name(i)).collect();
        range_names.sort();

        let prefix_lower = prefix.to_ascii_lowercase();
        let mut brute: Vec<String> = (0..archived.len() as u32)
            .map(|i| archived.name(i))
            .filter(|n| n.to_ascii_lowercase().starts_with(&prefix_lower))
            .collect();
        brute.sort();

        assert_eq!(
            range_names, brute,
            "prefix_range mismatch for prefix {:?}",
            prefix
        );
    }
}

fn large_arena(count: usize) -> Arena {
    let mut builder = ArenaBuilder::default();
    for i in 0..count {
        builder.push(&format!("family_{:02}_file_{i:05}.dat", i % 37), 0, false);
    }
    builder.build().0
}

#[test]
fn small_arena_has_empty_trigram_index() {
    assert!(large_arena(100).trigram_index.is_empty());
}

#[test]
fn trigram_index_has_no_false_negatives() {
    let arena = large_arena(5_000);
    let bytes = row_bytes(num_blocks(arena.len()));
    for sample in 0..200 {
        let source_idx = sample * 23 % arena.len();
        let source = name_from_owned_arena(&arena, source_idx as u32);
        let length = 3 + sample % 6;
        let start = sample % (source.len() - length + 1);
        let needle = &source[start..start + length];
        for idx in 0..arena.len() {
            let name = name_from_owned_arena(&arena, idx as u32);
            if crate::ascii::contains_ci(&name, needle) {
                for_each_trigram(needle, |hash| {
                    let block = idx / TRIGRAM_BLOCK;
                    assert_ne!(
                        arena.trigram_index[hash as usize * bytes + block / 8] & (1 << (block % 8)),
                        0,
                        "missing block {block} for needle {:?}",
                        String::from_utf8_lossy(needle)
                    );
                });
            }
        }
    }
}

fn name_from_owned_arena(arena: &Arena, idx: u32) -> Vec<u8> {
    let bucket = idx as usize / BUCKET_SIZE;
    let position = idx as usize % BUCKET_SIZE;
    let blob = &arena.names
        [arena.bucket_offsets[bucket] as usize..arena.bucket_offsets[bucket + 1] as usize];
    let mut cursor = 0;
    let len = read_varint(blob, &mut cursor) as usize;
    let mut name = blob[cursor..cursor + len].to_vec();
    cursor += len;
    for _ in 0..position {
        let shared = read_varint(blob, &mut cursor) as usize;
        let suffix = read_varint(blob, &mut cursor) as usize;
        name.truncate(shared);
        name.extend_from_slice(&blob[cursor..cursor + suffix]);
        cursor += suffix;
    }
    name
}

#[test]
fn candidate_blocks_and_ranged_scan_are_sound() {
    let arena = large_arena(5_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trigram.rkyv");
    save(&arena, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    for needle in [b"fam".as_slice(), b"file_01", b".dat"] {
        let candidates = archived.candidate_blocks(needle).unwrap();
        archived.for_each_name(|idx, name| {
            if crate::ascii::contains_ci(name, needle) {
                assert!(candidates.contains(&(idx / TRIGRAM_BLOCK as u32)));
            }
            std::ops::ControlFlow::Continue(())
        });
    }
    assert!(archived.candidate_blocks(b"").is_none());
    assert!(archived.candidate_blocks(b"a").is_none());
    assert!(archived.candidate_blocks(b"ab").is_none());

    let mut full = Vec::new();
    archived.for_each_name(|idx, name| {
        full.push((idx, name.to_vec()));
        std::ops::ControlFlow::Continue(())
    });
    let mut ranged = Vec::new();
    archived.for_each_name_in(0..archived.len() as u32, |idx, name| {
        ranged.push((idx, name.to_vec()));
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(ranged, full);

    let mut mid = Vec::new();
    archived.for_each_name_in(5..45, |idx, name| {
        mid.push((idx, name.to_vec()));
        std::ops::ControlFlow::Continue(())
    });
    assert_eq!(mid, full[5..45]);
}

#[test]
fn repeat_flags_match_independent_names_and_reset_at_boundaries() {
    let mut builder = ArenaBuilder::default();
    for name in ["", "a", "A", "alpha", "alphabet", "réport_数据"] {
        for _ in 0..70 {
            builder.push(name, 0, false);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repeated-names.rkyv");
    save(&builder.build().0, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let arena = store.archived();
    let names: Vec<_> = (0..arena.len() as u32)
        .map(|record| {
            let mut name = Vec::new();
            arena.name_into(record, &mut name);
            name
        })
        .collect();
    let mut full = Vec::new();
    arena.for_each_name_with_repeats(|record, name, repeated| {
        full.push((record, name.to_vec(), repeated));
        std::ops::ControlFlow::Continue(())
    });
    let mut buffer = vec![b'x'; 100];
    for start in [0, 1, 31, 32, 33, 63, 64, 65, arena.len() - 1, arena.len()] {
        for end in [start, (start + 35).min(arena.len()), arena.len() + 10] {
            let mut actual = Vec::new();
            arena.for_each_name_in_buf_with_repeats(
                start as u32..end as u32,
                &mut buffer,
                |record, name, repeated| {
                    actual.push((record, name.to_vec(), repeated));
                    std::ops::ControlFlow::Continue(())
                },
            );
            assert_eq!(actual.len(), end.min(arena.len()) - start);
            for (record, name, repeated) in actual {
                let index = record as usize;
                assert_eq!(name, names[index]);
                assert_eq!(
                    repeated,
                    index > start
                        && !index.is_multiple_of(BUCKET_SIZE)
                        && names[index] == names[index - 1]
                );
            }
        }
    }
    assert_eq!(full.len(), names.len());
    for (index, (_, name, repeated)) in full.iter().enumerate() {
        assert_eq!(name, &names[index]);
        assert_eq!(
            *repeated,
            !index.is_multiple_of(BUCKET_SIZE) && names[index] == names[index - 1]
        );
    }
    let mut visited = 0;
    arena.for_each_name_with_repeats(|_, _, _| {
        visited += 1;
        std::ops::ControlFlow::Break(())
    });
    assert_eq!(visited, 1);
}
