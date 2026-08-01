use crate::arena::Arena;
use crate::query::{search, Query};
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
    let hits = search(store.archived(), &Query::Prefix("nonsense".into()), 10);
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

    let hits = search(archived, &Query::Prefix("report".into()), 10);
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

    let hits = search(archived, &Query::wildcard("*.docx"), 10);
    assert_eq!(hits.len(), 2);

    let hits = search(archived, &Query::wildcard("readme.*"), 10);
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

    let hits = search(archived, &Query::Substring("FINAL".into()), 10);
    assert_eq!(hits.len(), 1);
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
        let filtered = search(archived, &Query::Substring(needle.clone()), usize::MAX);
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

    let limited = search(archived, &Query::Substring("node".into()), 17);
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
        let _ = search(archived, &Query::Substring(needle.clone()), usize::MAX);
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
