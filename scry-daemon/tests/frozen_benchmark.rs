//! Use the production executable and IPC path on a fixed, read-only corpus.
use scry_client::{Client, Order, QueryKind, SearchExecution};
use scry_core::{Arena, ArenaStore, IndexView, Query};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn connect(pipe: &str) -> Client {
    let start = Instant::now();
    loop {
        if let Ok(client) = Client::connect_to(pipe) {
            return client;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "frozen server did not become ready"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn frozen_server_keeps_serial_and_latest_wins_answers_correct_without_ingestion() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot = temp.path().join("fixture.rkyv");
    let mut builder = Arena::builder();
    let root = builder.push("C:", 0, true);
    for n in 0..2_000 {
        let record = builder.push(&format!("report_{n:04}.txt"), 0, false);
        builder.set_parent(record, root);
    }
    scry_core::store::save(&builder.build().0, &snapshot).unwrap();
    let before = std::fs::read(&snapshot).unwrap();
    let view = IndexView::new(Arc::new(ArenaStore::open(&snapshot).unwrap()));
    let expected = view.search(&Query::Substring("report".into()), 50);
    let pipe = format!(r"\\.\pipe\scry-bench-frozen-{}", std::process::id());
    let _daemon = Running(
        Command::new(env!("CARGO_BIN_EXE_scryd"))
            .arg("--benchmark-snapshot")
            .arg(&snapshot)
            .arg("--benchmark-pipe")
            .arg(&pipe)
            .env("SCRY_QUERY_THREADS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut session = connect(&pipe).into_search_session(50);
    for i in 0..1_000 {
        session
            .submit(QueryKind::Substring, ["rep", "repo", "report"][i % 3])
            .unwrap();
        assert_eq!(session.wait_latest().unwrap(), expected, "serial reply {i}");
    }
    for pattern in ["rep", "repo", "report"] {
        session.submit(QueryKind::Substring, pattern).unwrap();
    }
    assert_eq!(session.wait_latest().unwrap(), expected);
    let stats = session.stats().unwrap();
    assert!(stats.contains("last_reused_volumes=1"));
    assert!(stats.contains("last_scanned_volumes=0"));
    drop(session);
    let mut client = connect(&pipe);
    assert!(client.stats().unwrap().contains("benchmark_frozen=true"));
    let (actual, execution) = client
        .search_local_ordered_with_execution(QueryKind::Substring, "report", 50, Order::Relevance)
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(execution, SearchExecution::SharedIndex);
    assert_eq!(std::fs::read(&snapshot).unwrap(), before);
    assert_eq!(
        std::fs::read_dir(temp.path()).unwrap().count(),
        1,
        "read-only server created files"
    );
    let status = Command::new(env!("CARGO_BIN_EXE_scryd"))
        .arg("--benchmark-snapshot")
        .arg(&snapshot)
        .args(["--benchmark-pipe", r"\\.\pipe\scry"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        !status.success(),
        "benchmark mode must not claim the installed daemon pipe"
    );
}

#[test]
fn frozen_overlays_and_multiple_volumes_match_ranked_reference_and_report_fallback() {
    use scry_core::delta::{Delta, DeltaRecord, ParentRef};
    let temp = tempfile::tempdir().unwrap();
    let mut views = Vec::new();
    let mut paths = Vec::new();
    for slot in 0..2 {
        let mut builder = Arena::builder();
        let root = builder.push(if slot == 0 { "C:" } else { "D:" }, 0, true);
        for i in 0..100 {
            let record = builder.push(&format!("report_{i:04}.txt"), i, false);
            builder.set_parent(record, root);
        }
        let snapshot = temp.path().join(format!("volume-{slot}.rkyv"));
        scry_core::store::save(&builder.build().0, &snapshot).unwrap();
        let mut view = IndexView::new(Arc::new(ArenaStore::open(&snapshot).unwrap()));
        let mut delta = Delta::new(view.base.archived().len());
        delta
            .tombstones
            .set(view.base.archived().prefix_range("report_0000").start);
        delta.added.push(DeltaRecord {
            name: format!("report_delta_{slot}.txt"),
            parent: ParentRef::Base(
                view.base
                    .archived()
                    .prefix_range(if slot == 0 { "C:" } else { "D:" })
                    .start,
            ),
            mtime_secs: 1000,
            is_dir: false,
            size_bytes: 4096,
            size_exact: true,
            live: true,
        });
        let overlay = temp.path().join(format!("volume-{slot}.overlay"));
        std::fs::write(&overlay, delta.encode_query_overlay()).unwrap();
        view.delta = Arc::new(delta);
        views.push(view);
        paths.push((snapshot, overlay));
    }
    for volumes in [1, 2] {
        let pipe = format!(
            r"\\.\pipe\scry-bench-overlay-{}-{volumes}",
            std::process::id()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_scryd"));
        command.arg("--benchmark-pipe").arg(&pipe);
        for (snapshot, overlay) in &paths[..volumes] {
            command
                .arg("--benchmark-snapshot")
                .arg(snapshot)
                .arg("--benchmark-overlay")
                .arg(overlay);
        }
        let _daemon = Running(
            command
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut client = connect(&pipe);
        for order in [Order::Relevance, Order::Recent, Order::Largest] {
            let query = Query::Substring("report".into());
            let mut keys = std::collections::BTreeMap::new();
            for (slot, view) in views[..volumes].iter().enumerate() {
                for hit in
                    view.search_hits(&query, scry_core::view::SearchOptions::ordered(5, order))
                {
                    keys.insert((hit.rank_bits, slot, hit.record), hit);
                }
            }
            let expected: Vec<_> = keys
                .into_iter()
                .take(5)
                .map(|((_, slot, _), hit)| views[slot].materialize_one(&hit))
                .collect();
            let actual = client
                .query_ordered(QueryKind::Substring, "report", 5, order)
                .unwrap();
            assert_eq!(actual, expected);
            let (local, route) = client
                .search_local_ordered_with_execution(QueryKind::Substring, "report", 5, order)
                .unwrap();
            assert_eq!(local, expected);
            assert_eq!(
                route,
                if volumes == 1 {
                    SearchExecution::SharedIndex
                } else {
                    SearchExecution::RpcFallback
                }
            );
        }
        assert_eq!(
            client
                .query(QueryKind::Substring, "report_delta", 50)
                .unwrap()
                .len(),
            volumes
        );
        assert!(client
            .query(QueryKind::Prefix, "report_0000", 50)
            .unwrap()
            .is_empty());
    }
}

#[test]
fn live_benchmark_refuses_existing_or_external_cache_directories() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join(".scratch/cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("preserve"), b"existing user file").unwrap();
    for path in [&cache, &temp.path().join("outside")] {
        let status = Command::new(env!("CARGO_BIN_EXE_scryd"))
            .current_dir(temp.path())
            .args(["--benchmark-live-cache"])
            .arg(path)
            .args(["--benchmark-pipe", r"\\.\pipe\scry-bench-live-safety", "D:"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
    }
    assert_eq!(
        std::fs::read(cache.join("preserve")).unwrap(),
        b"existing user file"
    );
    assert!(!temp.path().join("outside").exists());
}
