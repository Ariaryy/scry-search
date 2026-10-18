//! Exercise the compiled runner against real pipes and sections, without
//! touching a user volume or depending on an installed daemon.
use scry_core::protocol::{
    decode_request, encode_results, encode_shared_index, QueryKind, SharedIndexResponse,
};
use scry_core::{
    delta::{Delta, DeltaRecord, ParentRef},
    view::SearchOptions,
    Arena, Query,
};
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};
use std::time::{Duration, Instant};

struct Fixture {
    pipe: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn connect(pipe: &str) -> scry_ipc::Pipe {
    let start = Instant::now();
    loop {
        if let Ok(pipe) = scry_ipc::connect_client(pipe) {
            return pipe;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Fixture {
    fn new(sharing: bool) -> Self {
        Self::with_corruption(sharing, false)
    }

    fn with_corruption(sharing: bool, corrupt: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let pipe = format!(
            r"\\.\pipe\scry-bench-fixture-{}-{}-{}-{}",
            std::process::id(),
            sharing,
            corrupt,
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let server = scry_ipc::PipeServer::new(&pipe).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut builder = Arena::builder();
            let root = builder.push("C:", 0, true);
            let directory = builder.push("archive", 0, true);
            builder.set_parent(directory, root);
            for i in 0..128 {
                let file = builder.push(&format!("report_{i:04}.txt"), i * 1024, false);
                builder.set_parent(file, directory);
            }
            let arena = builder.build().0;
            let bytes = Arc::new(scry_core::store::to_bytes(&arena).unwrap());
            let archived = scry_core::store::archived_bytes(&bytes).unwrap();
            let mut delta = Delta::new(arena.len());
            delta
                .tombstones
                .set(archived.prefix_range("report_0000").start);
            delta.added.push(DeltaRecord {
                name: "report_delta.txt".into(),
                parent: ParentRef::Base(archived.prefix_range("archive").start),
                mtime_secs: 1,
                is_dir: false,
                size_bytes: 4096,
                size_exact: true,
                live: true,
            });
            let delta = Arc::new(delta);
            let mut handlers = Vec::new();
            let mut connection_number = 0;
            while let Ok(pipe) = server.accept() {
                if stopped.load(Ordering::Relaxed) {
                    break;
                }
                let bytes = Arc::clone(&bytes);
                let delta = Arc::clone(&delta);
                connection_number += 1;
                // Stats, preflight runner, and oracle connect first. Only
                // corrupt the subsequent timed runner, preserving result count.
                let corrupt_timed = corrupt && connection_number == 4;
                handlers.push(std::thread::spawn(move || {
                    let arena = scry_core::store::archived_bytes(&bytes).unwrap();
                    while let Ok(frame) = pipe.read_frame() {
                        let request = decode_request(&frame).unwrap();
                        let response = match request.kind {
                            QueryKind::ShareIndex if sharing => {
                                let section = scry_ipc::Section::create(&bytes).unwrap();
                                encode_shared_index(&SharedIndexResponse {
                                    handle: section.duplicate_for(pipe.client_process_id().unwrap()).unwrap(),
                                    len: bytes.len() as u64,
                                    generation: 1,
                                    overlay: delta.encode_query_overlay(),
                                })
                            }
                            QueryKind::ShareIndex => encode_results(&[]),
                            QueryKind::QueryStats => b"queries: 0\nprivate_usage=0 working_set=0 peak_working_set=0 page_faults=0".to_vec(),
                            kind => {
                                let query = match kind {
                                    QueryKind::Prefix => Query::Prefix(request.pattern),
                                    QueryKind::Substring => Query::Substring(request.pattern),
                                    QueryKind::Wildcard => Query::wildcard(&request.pattern),
                                    QueryKind::PathTerms => scry_core::terms::parse_query(&request.pattern, 0).unwrap(),
                                    _ => unreachable!(),
                                };
                                let mut entries = scry_core::view::search_archived_with_delta(
                                    arena, &delta, &query,
                                    SearchOptions::ordered(request.limit as usize, request.order),
                                );
                                if corrupt_timed { if let Some(first) = entries.first_mut() { first.path.push_str("_wrong"); } }
                                encode_results(&entries)
                            }
                        };
                        if pipe.write_frame(&response).is_err() { break; }
                    }
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            pipe,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        drop(connect(&self.pipe));
        self.thread.take().unwrap().join().unwrap();
    }
}

fn exercise(sharing: bool) {
    let fixture = Fixture::new(sharing);
    let temporary = tempfile::tempdir().unwrap();
    let mut workload: serde_json::Value =
        serde_json::from_str(include_str!("../workload.example.json")).unwrap();
    workload["cases"].as_array_mut().unwrap().push(serde_json::json!({
        "label":"overlay", "shape":"delta-addition", "kind":"substring", "order":"relevance", "patterns":["report_delta"]
    }));
    std::fs::write(
        temporary.path().join("workload.json"),
        serde_json::to_vec(&workload).unwrap(),
    )
    .unwrap();
    let output = temporary.path().join(".scratch/capture.jsonl");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_scry-bench"))
        .current_dir(temporary.path())
        .args([
            "--output",
            ".scratch/capture.jsonl",
            "--workload",
            "workload.json",
            "--samples",
            "2",
            "--warmups",
            "0",
            "--rounds",
            "2",
            "--pipe",
            &fixture.pipe,
        ])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(&output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.last().unwrap()["type"], "complete");
    assert!(rows.iter().any(|row| row["type"] == "verification"));
    let samples: Vec<_> = rows.iter().filter(|row| row["type"] == "sample").collect();
    assert_eq!(samples.len(), 282);
    assert!(samples
        .iter()
        .all(|row| row["validated"] == true && row["emitted"] == row["expected_emitted"]));
    assert!(samples
        .iter()
        .filter(|row| row["case"] == "overlay")
        .all(|row| row["emitted"] == 1));
    assert!(samples.iter().any(|row| row["phase"] == "first_request"));
    assert!(samples.iter().any(|row| row["supersede"] == true));
    let local: Vec<_> = samples
        .iter()
        .filter(|row| row["requested"] == "local_first")
        .collect();
    assert!(!local.is_empty());
    assert!(local.iter().all(|row| row["execution"]
        == if sharing {
            "shared_index"
        } else {
            "rpc_fallback"
        }));
    assert!(samples
        .iter()
        .any(|row| row["emitted"].as_u64().unwrap() > 0));
    assert!(samples.iter().any(|row| row["emitted"] == 0));
    // No paths or query strings leak into raw timing rows.
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(!text.contains("report_0001.txt"));
    assert!(!text.contains("zqxjv_no_match"));
    let original = std::fs::read(&output).unwrap();
    let retry = std::process::Command::new(env!("CARGO_BIN_EXE_scry-bench"))
        .current_dir(temporary.path())
        .args(["--output", ".scratch/capture.jsonl"])
        .output()
        .unwrap();
    assert!(!retry.status.success());
    assert_eq!(std::fs::read(&output).unwrap(), original);
}

#[test]
fn runner_measures_shared_sections_and_interactive_sequences() {
    exercise(true);
}

#[test]
fn runner_labels_every_fallback_as_rpc() {
    exercise(false);
}

#[test]
fn same_count_wrong_timed_identity_rejects_capture_after_successful_preflight() {
    let fixture = Fixture::with_corruption(false, true);
    let temporary = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_scry-bench"))
        .current_dir(temporary.path())
        .args([
            "--output",
            ".scratch/rejected.jsonl",
            "--samples",
            "1",
            "--rounds",
            "1",
            "--pipe",
            &fixture.pipe,
        ])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("timed answer differs"));
    let capture =
        std::fs::read_to_string(temporary.path().join(".scratch/rejected.jsonl")).unwrap();
    assert!(capture.contains("\"ordered_equal\":true"));
    assert!(!capture.contains("\"type\":\"complete\""));
}

#[test]
fn external_observer_reads_process_counters_and_times_disconnected_idle() {
    let fixture = Fixture::new(true);
    let temporary = tempfile::tempdir().unwrap();
    for idle_connections in ["0", "2"] {
        let output = format!(".scratch/observe-{idle_connections}.json");
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_scry-bench"))
            .current_dir(temporary.path())
            .args([
                "observe",
                "--pipe",
                &fixture.pipe,
                "--output",
                &output,
                "--idle-seconds",
                "1",
                "--settle-seconds",
                "0",
                "--visibility-samples",
                "0",
                "--query-repeats",
                "1",
                "--idle-connections",
                idle_connections,
            ])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let text = std::fs::read_to_string(temporary.path().join(&output)).unwrap();
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["complete"], true);
        assert!(report["idle"]["sampled_seconds"].as_f64().unwrap() >= 1.0);
        assert!(report["sampled_private_max"]["before"].as_u64().unwrap() > 0);
        assert_eq!(report["query_routes"]["rpc"], 6);
        assert_eq!(report["query_routes"]["shared_index"], 6);
        assert_eq!(
            report["idle_connections"],
            idle_connections.parse::<u64>().unwrap()
        );
        assert!(!text.contains("report_0001.txt"));
    }
}
