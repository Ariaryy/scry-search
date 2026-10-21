use super::*;

/// Ignored release benchmark for `full_path` cache-locality.
/// Run with: cargo test -p scry-core --release -- --ignored --nocapture bench_full_path_locality
#[test]
#[ignore = "benchmark; run with --release --ignored"]
fn bench_full_path_locality() {
    use std::time::Instant;
    const N: usize = 100_000;
    let mut builder = ArenaBuilder::default();
    let root = builder.push("C:", 0, true);
    for i in 0..N {
        let entry = builder.push(&format!("file_{i:06}.txt"), 0, false);
        builder.set_parent(entry, root);
    }
    let (arena, _) = builder.build();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench_locality.rkyv");
    crate::store::save(&arena, &path).unwrap();
    let store = crate::store::ArenaStore::open(&path).unwrap();
    let archived = store.archived();

    // Warm up.
    for i in 0..archived.len() as u32 {
        let _ = archived.full_path(i, '\\');
    }
    // Measure.
    let t0 = Instant::now();
    const ITERS: usize = 10;
    for _ in 0..ITERS {
        for i in 0..archived.len() as u32 {
            let _ = archived.full_path(i, '\\');
        }
    }
    let elapsed = t0.elapsed();
    println!(
        "bench_full_path_locality: {} entries × {} iters = {:.2} µs/entry",
        archived.len(),
        ITERS,
        elapsed.as_secs_f64() * 1e6 / (archived.len() * ITERS) as f64
    );
}
