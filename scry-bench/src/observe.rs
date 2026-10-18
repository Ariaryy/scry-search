//! External process counters: no stats connection or disk writes during idle.
use anyhow::{ensure, Context, Result};
use scry_client::{Client, Order, QueryKind};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[repr(C)]
#[derive(Default)]
struct Memory {
    size: u32,
    faults: u32,
    values: [usize; 9],
}
#[repr(C)]
#[derive(Default)]
struct Io {
    values: [u64; 6],
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn GetProcessAffinityMask(
        handle: *mut std::ffi::c_void,
        process_mask: *mut usize,
        system_mask: *mut usize,
    ) -> i32;
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    fn K32GetProcessMemoryInfo(
        handle: *mut std::ffi::c_void,
        memory: *mut Memory,
        size: u32,
    ) -> i32;
    fn GetProcessTimes(
        handle: *mut std::ffi::c_void,
        created: *mut u64,
        exited: *mut u64,
        kernel: *mut u64,
        user: *mut u64,
    ) -> i32;
    fn GetProcessIoCounters(handle: *mut std::ffi::c_void, io: *mut Io) -> i32;
    fn GetExitCodeProcess(handle: *mut std::ffi::c_void, code: *mut u32) -> i32;
}

pub fn current_affinity() -> Result<u64> {
    let (mut process, mut system) = (0usize, 0usize);
    ensure!(
        unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) } != 0,
        "cannot read client affinity"
    );
    Ok(process as u64)
}
struct Process(*mut std::ffi::c_void);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
#[derive(serde::Serialize)]
struct Sample {
    phase: usize,
    elapsed_ns: u64,
    private_bytes: u64,
    working_set: u64,
    lifetime_peak_working_set: u64,
    page_faults: u32,
    cpu_100ns: u64,
    read_bytes: u64,
    write_bytes: u64,
    other_bytes: u64,
    read_operations: u64,
    write_operations: u64,
    other_operations: u64,
}
impl Process {
    fn open(pid: u32) -> Result<Self> {
        let handle = unsafe { OpenProcess(0x1000, 0, pid) };
        ensure!(!handle.is_null(),"cannot read target process counters; elevated targets may require an elevated observer");
        Ok(Self(handle))
    }
    fn sample(&self, phase: usize, start: Instant) -> Result<Sample> {
        let mut memory = Memory {
            size: std::mem::size_of::<Memory>() as u32,
            ..Memory::default()
        };
        let mut io = Io::default();
        let (mut created, mut exited, mut kernel, mut user, mut code) = (0, 0, 0, 0, 0);
        // Handles have query-only rights. The repr(C) buffers match the Win32
        // structures; all API results are checked before consuming output.
        ensure!(
            unsafe {
                K32GetProcessMemoryInfo(self.0, &mut memory, std::mem::size_of::<Memory>() as u32)
            } != 0,
            "memory counter read failed"
        );
        ensure!(
            unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }
                != 0,
            "CPU counter read failed"
        );
        ensure!(
            unsafe { GetProcessIoCounters(self.0, &mut io) } != 0,
            "I/O counter read failed"
        );
        ensure!(
            unsafe { GetExitCodeProcess(self.0, &mut code) } != 0 && code == 259,
            "observed process exited"
        );
        Ok(Sample {
            phase,
            elapsed_ns: start.elapsed().as_nanos() as u64,
            private_bytes: memory.values[8] as u64,
            working_set: memory.values[1] as u64,
            lifetime_peak_working_set: memory.values[0] as u64,
            page_faults: memory.faults,
            cpu_100ns: kernel + user,
            read_bytes: io.values[3],
            write_bytes: io.values[4],
            other_bytes: io.values[5],
            read_operations: io.values[0],
            write_operations: io.values[1],
            other_operations: io.values[2],
        })
    }
}

fn connected(pipe: &str) -> Result<Client> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Client::connect_to(pipe) {
            Ok(c) => return Ok(c),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => anyhow::bail!("observer daemon unavailable"),
        }
    }
}

fn path_matches(entry: &str, path: &std::path::Path) -> bool {
    entry
        .replace('/', "\\")
        .eq_ignore_ascii_case(path.to_string_lossy().trim_start_matches(r"\\?\"))
}

fn visible(
    rpc: &Client,
    local: &mut Client,
    name: &str,
    path: &std::path::Path,
    old: Option<&str>,
    started: Instant,
) -> Result<serde_json::Value> {
    let (mut rpc_ns, mut local_ns, mut route) = (None, None, "unknown");
    let deadline = started + Duration::from_secs(30);
    while Instant::now() < deadline && (rpc_ns.is_none() || local_ns.is_none()) {
        if rpc_ns.is_none() {
            let entries = rpc.query(QueryKind::Prefix, name, 50)?;
            let old_gone = match old {
                Some(old) => !rpc
                    .query(QueryKind::Prefix, old, 50)?
                    .iter()
                    .any(|e| e.path.ends_with(old)),
                None => true,
            };
            if old_gone && entries.iter().any(|e| path_matches(&e.path, path)) {
                rpc_ns = Some(started.elapsed().as_nanos() as u64);
            }
        }
        if local_ns.is_none() {
            let (entries, execution) = local.search_local_ordered_with_execution(
                QueryKind::Prefix,
                name,
                50,
                Order::Relevance,
            )?;
            let old_gone = match old {
                Some(old) => !local
                    .search_local(QueryKind::Prefix, old, 50)?
                    .iter()
                    .any(|e| e.path.ends_with(old)),
                None => true,
            };
            if old_gone && entries.iter().any(|e| path_matches(&e.path, path)) {
                local_ns = Some(started.elapsed().as_nanos() as u64);
                route = match execution {
                    scry_client::SearchExecution::SharedIndex => "shared_index",
                    _ => "rpc_fallback",
                };
            }
        }
        if rpc_ns.is_none() || local_ns.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(
        json!({"rpc_ns":rpc_ns,"local_ns":local_ns,"local_execution":route,"verified":rpc_ns.is_some()&&local_ns.is_some(),"timeout_seconds":30,"poll_ms":20}),
    )
}

pub fn command() -> Result<()> {
    let mut output = PathBuf::from(".scratch/benchmark/observe.json");
    let mut pipe = r"\\.\pipe\scry".to_owned();
    let (mut idle, mut settle, mut probes, mut repeats) = (600u64, 30u64, 10u32, 5u32);
    let mut idle_connections = 0usize;
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!("scry-bench observe --output .scratch/...json [--pipe NAME --idle-seconds 600 --settle-seconds 30 --visibility-samples 10 --query-repeats 5]\nExternal query-memory samples (20 ms), disconnected idle CPU/logical I/O (1 s), create/rename visibility (20 ms polling). Observer writes only after idle. Raw NTFS daemon and observation may need elevation.");
            return Ok(());
        }
        let value = args.next().context("observer option needs a value")?;
        match arg.as_str() {
            "--output" => output = value.into(),
            "--pipe" => pipe = value,
            "--idle-seconds" => idle = value.parse()?,
            "--settle-seconds" => settle = value.parse()?,
            "--visibility-samples" => probes = value.parse()?,
            "--query-repeats" => repeats = value.parse()?,
            "--idle-connections" => idle_connections = value.parse()?,
            _ => anyhow::bail!("unknown observer option"),
        }
    }
    ensure!(
        (1..=7200).contains(&idle)
            && settle <= 600
            && probes <= 100
            && (1..=100).contains(&repeats),
        "observer counts out of range"
    );
    ensure!(idle_connections <= 16, "at most sixteen idle connections");
    crate::prepare_scratch_output(&output)?;
    let client = connected(&pipe)?;
    let pid = client.server_process_id()?;
    let stats_before = client.stats()?;
    drop(client);
    let access = Process::open(pid)?;
    let start = Instant::now();
    let phase = Arc::new(AtomicUsize::new(0));
    let observed = phase.clone();
    let thread = std::thread::spawn(move || -> Result<Vec<Sample>> {
        let process = Process::open(pid)?;
        let mut samples = Vec::new();
        while observed.load(Ordering::Acquire) != 5 {
            let phase = observed.load(Ordering::Acquire);
            ensure!(samples.len() < 60_000, "observer sample budget exceeded");
            samples.push(process.sample(phase, start)?);
            std::thread::sleep(Duration::from_millis(if phase == 2 || phase == 4 {
                1000
            } else {
                20
            }));
        }
        Ok(samples)
    });
    // Always stop/join the observer even if a query or filesystem operation fails.
    let mut idle_boundaries = None;
    let mut routes = std::collections::BTreeMap::<String, u64>::new();
    let work = (|| -> Result<Vec<serde_json::Value>> {
        std::thread::sleep(Duration::from_secs(2));
        phase.store(1, Ordering::Release);
        let client = connected(&pipe)?;
        let mut local_queries = connected(&pipe)?;
        ensure!(
            client.server_process_id()? == pid,
            "daemon changed before stimulus"
        );
        for _ in 0..repeats {
            for (kind, pattern) in [
                (QueryKind::PathTerms, "a"),
                (QueryKind::Substring, "report"),
                (QueryKind::Prefix, "a"),
                (QueryKind::PathTerms, "ext:txt"),
                (QueryKind::Wildcard, "*.txt"),
                (QueryKind::PathTerms, "zqxjv_bench_absent"),
            ] {
                let expected = client.query(kind, pattern, 50)?;
                let (actual, route) = local_queries.search_local_ordered_with_execution(
                    kind,
                    pattern,
                    50,
                    Order::Relevance,
                )?;
                ensure!(
                    expected == actual,
                    "live query results changed between RPC and local measurement"
                );
                *routes.entry("rpc".into()).or_default() += 1;
                *routes
                    .entry(
                        match route {
                            scry_client::SearchExecution::SharedIndex => "shared_index",
                            _ => "rpc_fallback",
                        }
                        .into(),
                    )
                    .or_default() += 1;
            }
        }
        drop(client);
        drop(local_queries);
        let idle_clients = (0..idle_connections)
            .map(|_| connected(&pipe))
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            idle_clients
                .iter()
                .all(|c| c.server_process_id().ok() == Some(pid)),
            "daemon changed before idle"
        );
        phase.store(4, Ordering::Release);
        std::thread::sleep(Duration::from_secs(settle));
        phase.store(2, Ordering::Release);
        let first = access.sample(2, start)?;
        std::thread::sleep(Duration::from_secs(idle));
        let last = access.sample(2, start)?;
        idle_boundaries = Some((first, last));
        drop(idle_clients);
        // Visibility mutations happen after quiet idle so they cannot queue
        // persistence work inside the measured idle interval.
        phase.store(3, Ordering::Release);
        let mut visibility = Vec::new();
        if probes != 0 {
            let temp = tempfile::Builder::new()
                .prefix("scry-bench-probes-")
                .tempdir_in(output.parent().unwrap())?;
            let rpc = connected(&pipe)?;
            let mut local = connected(&pipe)?;
            ensure!(
                rpc.server_process_id()? == pid && local.server_process_id()? == pid,
                "daemon changed before visibility probe"
            );
            for i in 0..probes {
                let name = format!(
                    "scry_bench_create_{}_{}_{i}.txt",
                    std::process::id(),
                    start.elapsed().as_nanos()
                );
                let path = temp.path().join(&name);
                let at = Instant::now();
                std::fs::write(&path, b"synthetic benchmark probe")?;
                let mut row = visible(&rpc, &mut local, &name, &path, None, at)?;
                row["operation"] = json!("create");
                visibility.push(row);
                if visibility.last().unwrap()["verified"] != true {
                    break;
                }
                let renamed = format!(
                    "scry_bench_rename_{}_{}_{i}.txt",
                    std::process::id(),
                    start.elapsed().as_nanos()
                );
                let target = temp.path().join(&renamed);
                let at = Instant::now();
                std::fs::rename(&path, &target)?;
                let mut row = visible(&rpc, &mut local, &renamed, &target, Some(&name), at)?;
                row["operation"] = json!("rename");
                visibility.push(row);
                if visibility.last().unwrap()["verified"] != true {
                    break;
                }
            }
            drop(local);
            drop(rpc);
            drop(temp);
        }
        Ok(visibility)
    })();
    phase.store(5, Ordering::Release);
    let samples = thread
        .join()
        .map_err(|_| anyhow::anyhow!("observer thread panicked"))??;
    let visibility = work?;
    let idle_samples: Vec<_> = samples.iter().filter(|s| s.phase == 2).collect();
    let (first, last) = idle_boundaries.context("missing idle boundaries")?;
    let elapsed = (last.elapsed_ns - first.elapsed_ns) as f64 / 1e9;
    let cpu = (last.cpu_100ns - first.cpu_100ns) as f64 / 1e7;
    let peak = |phase| {
        samples
            .iter()
            .filter(|s| s.phase == phase)
            .map(|s| s.private_bytes)
            .max()
    };
    let verified = visibility.iter().all(|r| r["verified"] == true);
    let report = json!({
        "schema":1,"complete":verified,
        "profile":env!("SCRY_BENCH_PROFILE"),"source":env!("SCRY_BENCH_COMMIT"),
        "daemon_stats_before":stats_before,"query_routes":routes,
        "query_shapes":["one-byte-path","substring","prefix","extension-filter","wildcard","negative"],
        "phases":{"0":"before","1":"query_burst","2":if idle_connections==0 {"disconnected_idle"}else{"connected_idle"},"3":"visibility","4":"settle"},
        "query_repeats":repeats,"query_limit":50,"idle_requested_seconds":idle,"settle_seconds":settle,"idle_connections":idle_connections,
        "idle":{"sampled_seconds":elapsed,"samples":idle_samples.len(),"cpu_seconds":cpu,
            "one_logical_cpu_pct":if elapsed>0.0 {Some(cpu/elapsed*100.0)}else{None},
            "logical_read_bytes":last.read_bytes-first.read_bytes,"logical_write_bytes":last.write_bytes-first.write_bytes,"other_io_bytes":last.other_bytes-first.other_bytes,
            "read_operations":last.read_operations-first.read_operations,"write_operations":last.write_operations-first.write_operations,"other_operations":last.other_operations-first.other_operations},
        "idle_boundary_samples":[first,last],
        "sampled_private_max":{"before":peak(0),"query_burst":peak(1),"idle":peak(2)},
        "sampling":"20 ms before/query/visibility; 1 s settling/idle; sampled maxima can miss short peaks; lifetime working-set peak is not interval peak",
        "visibility":visibility,"samples":samples,
        "io_scope":"logical target-process counters, not physical disk traffic; no query requests during settle/idle; idle_connections records open sessions; no forced working-set trim"
    });
    let mut out = crate::scratch_writer(&output)?;
    serde_json::to_writer_pretty(&mut out, &report)?;
    use std::io::Write;
    out.flush()?;
    ensure!(
        verified,
        "visibility probe timed out; inspect aggregate report"
    );
    Ok(())
}
