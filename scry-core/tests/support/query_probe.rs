// Shared by the core (System allocator) and daemon (mimalloc) ignored probes.
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default)]
struct ProcessMemory {
    size: u32,
    faults: u32,
    values: [usize; 9],
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn K32GetProcessMemoryInfo(
        handle: *mut std::ffi::c_void,
        counters: *mut ProcessMemory,
        size: u32,
    ) -> i32;
}

fn private_bytes() -> usize {
    let mut memory = ProcessMemory {
        size: std::mem::size_of::<ProcessMemory>() as u32,
        ..Default::default()
    };
    assert_ne!(
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, memory.size) },
        0
    );
    memory.values[8]
}

pub fn run<T: PartialEq>(allocator: &str, records: usize, mut execute: impl FnMut() -> T) {
    let samples: usize =
        std::env::var("SCRY_PROBE_SAMPLES").map_or(40, |value| value.parse().unwrap());
    assert!((1..=10_000).contains(&samples));
    let cold = private_bytes();
    let first = Instant::now();
    let expected = execute();
    let first_ns = first.elapsed().as_nanos();
    for _ in 0..5 {
        assert!(execute() == expected, "probe answer changed during warmup");
    }
    let warm = private_bytes();
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        let actual = execute();
        timings.push(started.elapsed().as_nanos());
        assert!(actual == expected, "probe answer changed during timing");
    }
    drop(expected);
    let after = private_bytes();
    std::thread::sleep(Duration::from_secs(2));
    let settled = private_bytes();
    println!("query_probe={{\"allocator\":\"{allocator}\",\"records\":{records},\"limit\":200,\"samples\":{samples},\"warmups\":5,\"first_ns\":{first_ns},\"cold_private\":{cold},\"warm_private\":{warm},\"after_private\":{after},\"settled_private\":{settled},\"latency_ns\":{timings:?}}}");
}
