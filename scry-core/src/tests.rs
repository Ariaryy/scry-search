use crate::arena::Arena;
use crate::query::{search_base, Query};
use crate::store::{save, ArenaStore};

fn sample_arena() -> Arena {
    let mut b = Arena::builder();
    let root = b.push("C:", 0, true);
    let docs = b.push("Documents", 0, true);
    b.set_parent(docs, root);
    let r1 = b.push("report.docx", 0, false);
    b.set_parent(r1, docs);
    let r2 = b.push("report_final.docx", 0, false);
    b.set_parent(r2, docs);
    let readme = b.push("readme.txt", 0, false);
    b.set_parent(readme, root);
    b.build().0
}

fn open_sample_store(dir: &tempfile::TempDir) -> ArenaStore {
    let arena = sample_arena();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    ArenaStore::open(&path).unwrap()
}

#[test]
fn prefix_search_finds_no_match_for_unknown_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_sample_store(&dir);
    let hits = search_base(store.archived(), &Query::Prefix("nonsense".into()), 10);
    assert!(hits.is_empty());
}

#[test]
fn round_trip_through_mmap_store_preserves_data() {
    let arena = sample_arena();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();

    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();
    assert_eq!(archived.len(), 5);

    let hits = search_base(archived, &Query::Prefix("report".into()), 10);
    assert_eq!(hits.len(), 2);
    let mut names: Vec<String> = hits.iter().map(|&i| archived.name(i)).collect();
    names.sort();
    assert_eq!(names, vec!["report.docx", "report_final.docx"]);

    for &hit in &hits {
        let full = archived.full_path(hit, '\\');
        assert!(full.starts_with("C:\\Documents\\report"));
    }
}

#[test]
fn wildcard_query_matches_extension() {
    let arena = sample_arena();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    let hits = search_base(archived, &Query::wildcard("*.docx"), 10);
    assert_eq!(hits.len(), 2);

    let hits = search_base(archived, &Query::wildcard("readme.*"), 10);
    assert_eq!(hits.len(), 1);
}

#[test]
fn substring_query_is_case_insensitive() {
    let arena = sample_arena();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    let hits = search_base(archived, &Query::Substring("FINAL".into()), 10);
    assert_eq!(hits.len(), 1);
}

#[test]
fn substring_query_matches_regardless_of_needle_or_name_casing() {
    let mut b = Arena::builder();
    let root = b.push("C:", 0, true);
    let mixed = b.push("LeDgEr_Report.pdf", 0, false);
    b.set_parent(mixed, root);
    let arena = b.build().0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    // Mixed-case needle against a mixed-case name, and the fully lowercase
    // and fully uppercase forms of the same needle, all find the one record.
    for needle in ["lEdGeR", "ledger", "LEDGER"] {
        let hits = search_base(archived, &Query::Substring(needle.into()), 10);
        assert_eq!(hits.len(), 1, "needle {needle:?} should match");
    }
}

fn generated_arena(count: usize) -> Arena {
    let mut builder = Arena::builder();
    for i in 0..count {
        let name = match i % 4 {
            0 => format!("IMG_{i:06}.JPG"),
            1 => format!("node_modules_package_{:03}_index_{i}.js", i % 113),
            2 => format!("{:08x}-{:04x}-{:04x}.dat", i, i % 65536, i * 17 % 65536),
            _ => format!("short_{:05}", i * 7919 % 100_000),
        };
        builder.push(&name, 0, false);
    }
    builder.build().0
}

#[test]
fn substring_results_match_brute_force_and_preserve_limits() {
    let arena = generated_arena(5_000);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("filtered.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    for sample in 0..200u32 {
        let name = archived.name(sample * 23 % archived.len() as u32);
        let bytes = name.as_bytes();
        let length = 1 + sample as usize % 8;
        let start = sample as usize % (bytes.len() - length + 1);
        let needle = String::from_utf8(bytes[start..start + length].to_vec()).unwrap();
        let filtered = search_base(archived, &Query::Substring(needle.clone()), usize::MAX);
        let needle_lower = needle.to_ascii_lowercase();
        let mut brute = Vec::new();
        archived.for_each_name(|idx, candidate| {
            if crate::ascii::contains_ci(candidate, needle_lower.as_bytes()) {
                brute.push(idx);
            }
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(filtered, brute, "needle {needle:?}");
    }

    let limited = search_base(archived, &Query::Substring("node".into()), 17);
    assert_eq!(limited.len(), 17);
    assert!(limited.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
#[ignore = "benchmark; run with --release --ignored"]
fn bench_substring_filtered_vs_scan() {
    use std::time::Instant;

    let arena = generated_arena(500_000);
    let archive = rkyv::to_bytes::<_, 1024>(&arena).unwrap();
    let trigram_bytes = arena.trigram_index.len();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("benchmark.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();
    let needles: Vec<String> = (0..100)
        .map(|i| format!("{:04x}", (i * 997 + 12345) % 65536))
        .collect();

    let started = Instant::now();
    for needle in &needles {
        let _ = search_base(archived, &Query::Substring(needle.clone()), usize::MAX);
    }
    let filtered = started.elapsed();

    let started = Instant::now();
    for needle in &needles {
        let lower = needle.as_bytes();
        let mut hits = 0;
        archived.for_each_name(|_, name| {
            hits += usize::from(crate::ascii::contains_ci(name, lower));
            std::ops::ControlFlow::Continue(())
        });
        std::hint::black_box(hits);
    }
    let scanned = started.elapsed();

    let mean_block_fraction = needles
        .iter()
        .map(|needle| {
            archived.candidate_blocks(needle.as_bytes()).unwrap().len() as f64
                / crate::trigram::num_blocks(archived.len()) as f64
        })
        .sum::<f64>()
        / needles.len() as f64;
    println!("filtered={filtered:?} full_scan={scanned:?}");
    println!(
        "speedup={:.2}x",
        scanned.as_secs_f64() / filtered.as_secs_f64()
    );
    println!("mean_blocks_scanned={mean_block_fraction:.4}");
    println!(
        "trigram_bytes={trigram_bytes} archive_bytes={} fraction={:.2}%",
        archive.len(),
        trigram_bytes as f64 * 100.0 / archive.len() as f64
    );
}

/// A synthetic volume at 10M records — an order of magnitude past any corpus
/// used elsewhere in this suite — loads, queries, and its recursive
/// directory sizes (a `u64` prefix sum over per-record `u32` KiB values, see
/// `Arena::dfs_size_prefix`) don't overflow even when every file is given a
/// near-`u32::MAX` size, which is the scenario that would overflow a `u32`
/// running total well before 10M records are reached.
#[test]
#[ignore = "slow; run with --release --ignored"]
fn ten_million_record_arena_loads_queries_and_recursive_sizes_dont_overflow() {
    const COUNT: usize = 10_000_000;
    let mut builder = Arena::builder();
    let provisional_root = builder.push("V:", 0, true);
    for i in 0..COUNT {
        let name = format!("file_{i:08}.dat");
        let node = builder.push_bytes_with_metadata(
            name.as_bytes(),
            1_700_000_000,
            false,
            None,
            u32::MAX as u64,
        );
        builder.set_parent(node, provisional_root);
    }
    let arena = builder.build().0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ten_million.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();
    assert_eq!(archived.len(), COUNT + 1);

    let hits = search_base(archived, &Query::Substring("file_00099999".into()), 10);
    assert_eq!(hits.len(), 1);

    // `build()` reorders records into name-sorted order, so `provisional_root`
    // is no longer a valid index into `archived` — look the root back up by
    // name, the same way callers outside this builder always must.
    let root = archived.prefix_range("V:").start;

    // Every KiB-rounded per-file size saturates at `u32::MAX`, and there are
    // 10M of them, so the true recursive total is far past `u32::MAX` —
    // proving the result is the saturated `u32`, not a silently wrapped one.
    assert_eq!(archived.recursive_size_kib(root), u32::MAX);
}

/// Selectivity distribution for realistic query prefixes: what fraction of
/// records a needle matches, and what fraction of trigram blocks survive the
/// filter. This is the number that gates whether a rank-ordered scan (a
/// prospective format change) is worth its risk — that bet only pays off if
/// most real needles are already highly selective, so a full scan is rare.
///
/// Not a deterministic-corpus criterion benchmark (see `benches/query.rs`):
/// it samples names out of the corpus it builds, so its numbers describe a
/// distribution rather than gate a regression. Run with
/// `cargo test -p scry-core --release -- --ignored selectivity --nocapture`.
#[test]
#[ignore = "prints a distribution table; not a pass/fail gate"]
fn selectivity_distribution_over_realistic_query_prefixes() {
    use crate::query::search_base;

    const RECORDS: usize = 500_000;
    const SAMPLES: usize = 2_000;

    let arena = generated_arena(RECORDS);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("selectivity.rkyv");
    save(&arena, &path).unwrap();
    let store = ArenaStore::open(&path).unwrap();
    let archived = store.archived();
    let blocks = crate::trigram::num_blocks(archived.len());

    // xorshift64*, deterministic so a failing run is reproducible.
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    // Short prefixes repeat heavily across 2000 samples (there are only so
    // many 1- and 2-byte prefixes in the corpus), and each distinct needle's
    // scan cost dwarfs the sampling loop, so dedupe per length before scoring
    // rather than rescoring the same needle hundreds of times.
    let mut prefixes_by_len: [std::collections::BTreeSet<String>; 5] = Default::default();
    for _ in 0..SAMPLES {
        let record = (next_rand() as usize) % archived.len();
        let name = archived.name(record as u32);
        let bytes = name.as_bytes();
        for len in 1..=5.min(bytes.len()) {
            if let Ok(prefix) = std::str::from_utf8(&bytes[..len]) {
                prefixes_by_len[len - 1].insert(prefix.to_string());
            }
        }
    }

    let hardcoded = [
        ".pdf",
        ".log",
        "invoice",
        "readme",
        "node_modules",
        ".dll",
        ".exe",
        "config",
        "test",
        "img",
    ];

    let selectivity_of = |needle: &str| -> (f64, f64) {
        let matches = search_base(archived, &Query::Substring(needle.to_string()), usize::MAX);
        let survived = archived
            .candidate_blocks(needle.as_bytes())
            .map_or(blocks, |b| b.len());
        (
            matches.len() as f64 / archived.len() as f64,
            survived as f64 / blocks as f64,
        )
    };

    let percentile = |values: &mut [f64], p: f64| -> f64 {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        values[((values.len() - 1) as f64 * p).round() as usize]
    };

    println!("\ncorpus: {} records, {blocks} blocks", archived.len());
    let mut p50_3plus = Vec::new();
    for (len, prefixes) in prefixes_by_len.iter().enumerate() {
        let len = len + 1;
        let mut selectivities: Vec<f64> = prefixes
            .iter()
            .map(|prefix| selectivity_of(prefix).0)
            .collect();
        if selectivities.is_empty() {
            continue;
        }
        let p10 = percentile(&mut selectivities.clone(), 0.10);
        let p50 = percentile(&mut selectivities.clone(), 0.50);
        let p90 = percentile(&mut selectivities, 0.90);
        println!(
            "  prefix len {len}: n={:5} p10={:6.2}% p50={:6.2}% p90={:6.2}%",
            prefixes.len(),
            p10 * 100.0,
            p50 * 100.0,
            p90 * 100.0,
        );
        if len >= 3 {
            p50_3plus.push(p50);
        }
    }

    println!("  hardcoded realistic queries:");
    for needle in hardcoded {
        let (selectivity, block_survival) = selectivity_of(needle);
        println!(
            "    {needle:14} -> {:6.2}% of records, {:6.2}% of blocks survive filter",
            selectivity * 100.0,
            block_survival * 100.0,
        );
    }

    let median_of_p50s = {
        let mut v = p50_3plus;
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
    };
    println!(
        "\n  verdict input: median p50 selectivity across 3+ byte prefixes = {:.2}%",
        median_of_p50s * 100.0
    );
    if median_of_p50s < 0.05 {
        println!("  -> below 5%: a rank-ordered scan over 3+ byte needles looks worth building.");
    } else if median_of_p50s > 0.20 {
        println!(
            "  -> above 20%: a rank-ordered scan's budget policy would fall back to a full \
             scan most of the time on this corpus; the bet is weak here."
        );
    } else {
        println!("  -> between 5% and 20%: inconclusive on this synthetic corpus.");
    }
}

/// Live-volume counterpart to the synthetic distribution above. Set
/// `SCRY_LIVE_SNAPSHOT` to an existing snapshot path. All sampled names stay
/// private: output contains only aggregate percentiles and the generic fixed
/// queries below.
///
/// Exact match counts are collected in one archive pass with a multi-pattern
/// automaton rather than rescanning a multi-million-record snapshot once per
/// sampled prefix.
#[test]
#[ignore = "requires SCRY_LIVE_SNAPSHOT; prints aggregate live-index data"]
fn live_selectivity_distribution() {
    use aho_corasick::{AhoCorasickBuilder, MatchKind};
    use std::ops::ControlFlow;

    const SAMPLES: usize = 2_000;
    let path = std::env::var_os("SCRY_LIVE_SNAPSHOT")
        .map(std::path::PathBuf::from)
        .expect("set SCRY_LIVE_SNAPSHOT to an existing .rkyv snapshot");
    let store = ArenaStore::open(&path).expect("open live snapshot");
    let archived = store.archived();
    assert!(!archived.is_empty(), "live snapshot is empty");
    let total_blocks = crate::trigram::num_blocks(archived.len());

    let mut rng: u64 = 0xD1B5_4A32_D192_ED03;
    let mut next_rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut sampled: [std::collections::BTreeSet<String>; 5] = Default::default();
    for _ in 0..SAMPLES {
        let record = (next_rand() as usize) % archived.len();
        let name = archived.name(record as u32);
        for len in 1..=5.min(name.len()) {
            if let Some(prefix) = name.get(..len) {
                sampled[len - 1].insert(prefix.to_ascii_lowercase());
            }
        }
    }

    let fixed = [
        ".pdf",
        ".log",
        "document",
        "readme",
        "node_modules",
        ".dll",
        ".exe",
        "config",
        "test",
        "image",
    ];
    let mut all = std::collections::BTreeSet::new();
    for group in &sampled {
        all.extend(group.iter().cloned());
    }
    all.extend(fixed.iter().map(|needle| (*needle).to_string()));
    let patterns: Vec<String> = all.into_iter().collect();
    let pattern_index: std::collections::BTreeMap<&str, usize> = patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| (pattern.as_str(), index))
        .collect();
    let automaton = AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::Standard)
        .build(&patterns)
        .expect("build live-prefix automaton");

    let mut match_counts = vec![0u32; patterns.len()];
    let mut seen_at_record = vec![u32::MAX; patterns.len()];
    archived.for_each_name(|record, name| {
        for matched in automaton.find_overlapping_iter(name) {
            let pattern = matched.pattern().as_usize();
            if seen_at_record[pattern] != record {
                seen_at_record[pattern] = record;
                match_counts[pattern] += 1;
            }
        }
        ControlFlow::Continue(())
    });

    let score = |needle: &str| -> (f64, f64) {
        let index = pattern_index[needle];
        let surviving = archived
            .candidate_blocks(needle.as_bytes())
            .map_or(total_blocks, |blocks| blocks.len());
        (
            match_counts[index] as f64 / archived.len() as f64,
            surviving as f64 / total_blocks as f64,
        )
    };
    let percentile = |values: &mut [f64], p: f64| -> f64 {
        values.sort_by(|a, b| a.partial_cmp(b).unwrap());
        values[((values.len() - 1) as f64 * p).round() as usize]
    };

    println!(
        "\nlive corpus: {} records, {total_blocks} blocks, {} distinct sampled/fixed needles",
        archived.len(),
        patterns.len()
    );
    let mut all_3plus = Vec::new();
    for (offset, group) in sampled.iter().enumerate() {
        let len = offset + 1;
        let scores: Vec<(f64, f64)> = group.iter().map(|needle| score(needle)).collect();
        let selectivity: Vec<f64> = scores.iter().map(|score| score.0).collect();
        let block_survival: Vec<f64> = scores.iter().map(|score| score.1).collect();
        if len >= 3 {
            all_3plus.extend(selectivity.iter().copied());
        }
        println!(
            "  prefix len {len}: n={:5} selectivity p10={:6.2}% p50={:6.2}% p90={:6.2}% | blocks p50={:6.2}% p90={:6.2}%",
            group.len(),
            percentile(&mut selectivity.clone(), 0.10) * 100.0,
            percentile(&mut selectivity.clone(), 0.50) * 100.0,
            percentile(&mut selectivity.clone(), 0.90) * 100.0,
            percentile(&mut block_survival.clone(), 0.50) * 100.0,
            percentile(&mut block_survival.clone(), 0.90) * 100.0,
        );
    }

    println!("  fixed generic queries:");
    for needle in fixed {
        let (selectivity, block_survival) = score(needle);
        println!(
            "    {needle:14} -> {:6.2}% of records, {:6.2}% of blocks",
            selectivity * 100.0,
            block_survival * 100.0
        );
    }
    let p50_3plus = percentile(&mut all_3plus, 0.50);
    println!(
        "\n  live gate: p50 selectivity across all sampled 3–5 byte needles = {:.2}%",
        p50_3plus * 100.0
    );
    if p50_3plus < 0.05 {
        println!("  -> below 5%; the selectivity gate alone supports further investigation.");
    } else if p50_3plus > 0.20 {
        println!("  -> above 20%; a rank-ordered budget would usually fall back; reject the bet.");
    } else {
        println!("  -> between 5% and 20%; the live selectivity gate is inconclusive.");
    }
}
