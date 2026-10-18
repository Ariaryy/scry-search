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
