#[test]
fn bench_size() {
    use scry_core::Arena;
    let mut b = Arena::builder();
    for i in 0..1_000_000 {
        b.push(&format!("filename_somewhat_long_test_{i}.txt"), 0, false);
    }
    let arena = b.build().0;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.rkyv");
    scry_core::store::save(&arena, &path).unwrap();
    let len = std::fs::metadata(&path).unwrap().len();
    println!(
        "1M entries size: {} bytes, {} bytes/entry",
        len,
        len as f64 / 1_000_000.0
    );
}
