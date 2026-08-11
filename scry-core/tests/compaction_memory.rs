#![cfg(windows)]

use std::ffi::c_void;
use std::sync::Arc;

use scry_core::arena::{derive_spooled_size_exact, SpooledArenaBuilder};
use scry_core::delta::{Delta, DeltaRecord, ParentRef};
use scry_core::dfs;
use scry_core::store::{self, ArenaColumns, ArenaStore};
use scry_core::{IndexView, PARENT_NONE};

#[test]
#[ignore = "release-mode compaction memory probe"]
fn compaction_private_usage_stays_bounded() {
    const BASE: usize = 224_231;
    const ADDED: usize = 12_000;
    const DELETED: usize = 3_000;

    let dir = tempfile::tempdir().unwrap();
    let base_scratch = dir.path().join("base-scratch");
    std::fs::create_dir(&base_scratch).unwrap();
    let mut builder = SpooledArenaBuilder::new(BASE, &base_scratch, &|_| {}).unwrap();
    builder.push(b"000_root", 0, true, 0, true, PARENT_NONE);
    for index in 1..BASE {
        let name = format!("item_{index:06}.bin");
        builder.push(name.as_bytes(), 0, false, 4096, true, 0);
    }
    let columns = builder.finish();
    let layout =
        dfs::build_file_backed(columns.parents.as_slice(), &base_scratch, &|_| {}).unwrap();
    let exact = derive_spooled_size_exact(
        columns.parents.as_slice(),
        &columns.size_exact_inputs,
        &layout,
        &base_scratch,
        &|_| {},
    )
    .unwrap();
    let prefix = dfs::prefix_sums_u64_file_backed(
        &layout.records,
        &columns.sizes,
        &base_scratch.join("base-prefix.spool"),
        &|_| {},
    )
    .unwrap();
    let base_path = dir.path().join("base.rkyv");
    store::save_columns_with(
        &ArenaColumns {
            format_version: scry_core::record::FORMAT_VERSION,
            journal_id: 0,
            next_usn: 0,
            volume_serial: 0,
            names: columns.names.as_slice(),
            bucket_offsets: columns.bucket_offsets.as_slice(),
            parents: columns.parents.as_slice(),
            mtimes: columns.mtimes.as_slice(),
            sizes: columns.sizes.as_slice(),
            size_exact_bits: exact.as_slice(),
            trigram_index: columns.trigram_index.as_slice(),
            dfs_positions: layout.positions.as_slice(),
            dfs_records: layout.records.as_slice(),
            dfs_ends: layout.subtree_ends.as_slice(),
            dfs_size_prefix: prefix.as_slice(),
        },
        &base_path,
        |_| {},
    )
    .unwrap();
    drop((columns, layout, exact, prefix));

    let mut view = IndexView::new(Arc::new(ArenaStore::open(&base_path).unwrap()));
    let mut delta = Delta::new(BASE);
    for record in 1..=DELETED as u32 {
        delta.tombstones.set(record);
    }
    for index in 0..ADDED {
        delta.added.push(DeltaRecord {
            name: format!("zzz_added_{index:05}.bin"),
            parent: ParentRef::Base(0),
            mtime_secs: 0,
            is_dir: false,
            size_bytes: 4096,
            size_exact: index % 17 != 0,
            live: true,
        });
    }
    view.delta = Arc::new(delta);

    let compact_scratch = dir.path().join("compact-scratch");
    std::fs::create_dir(&compact_scratch).unwrap();
    let output = dir.path().join("compacted.rkyv");
    let start = private_usage();
    let mut peak = start;
    view.compact_to_snapshot(&compact_scratch, &output, &|_| {}, &mut |_| {
        peak = peak.max(private_usage());
    })
    .unwrap();
    peak = peak.max(private_usage());
    println!("start={start} peak={peak} growth={}", peak - start);
    assert!(peak <= 30 * 1024 * 1024, "private usage exceeded 30 MiB");
    assert!(std::fs::read_dir(&compact_scratch)
        .unwrap()
        .next()
        .is_none());
}

fn private_usage() -> u64 {
    let mut counters = ProcessMemoryCountersEx {
        cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut ProcessMemoryCountersEx).cast(),
            counters.cb,
        )
    };
    assert_ne!(ok, 0);
    counters.private_usage as u64
}

#[repr(C)]
#[derive(Default)]
struct ProcessMemoryCountersEx {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_usage: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
}

#[link(name = "psapi")]
extern "system" {
    fn GetProcessMemoryInfo(process: *mut c_void, counters: *mut c_void, size: u32) -> i32;
}
