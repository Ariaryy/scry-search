//! B2: an aggregate vector describing a snapshot, not a universal complexity
//! score. Per-record scratch is mapped; names/extensions never enter output.
use anyhow::{ensure, Context, Result};
use scry_core::spool::Spool;
use scry_core::{ArchivedArena, ArenaStore, PARENT_NONE};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::ops::ControlFlow;
use std::path::Path;

const NAME_LIMIT: usize = 65_536;
const DRAWS: usize = 2_000;
const SEED: u64 = 0xd1b5_4a32_d192_ed03;

pub fn sha256_snapshot(store: &ArenaStore) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    // Hash the opened file, not the pathname: an atomic snapshot replacement
    // may change what the path refers to while this mapping stays valid.
    let mut file = store.snapshot_file().try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 65_536];
    loop {
        let read = file.read(&mut bytes)?;
        if read == 0 {
            break;
        }
        hash.update(&bytes[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn ratio(part: u64, whole: u64) -> Option<f64> {
    (whole != 0).then(|| part as f64 / whole as f64)
}

fn distribution(counts: &[u32], total: u64) -> Value {
    if total == 0 {
        return json!({"p50":null,"p90":null,"p99":null,"max":null});
    }
    let quantile = |percent: u64| {
        let rank = (total * percent).div_ceil(100);
        let mut cumulative = 0;
        counts
            .iter()
            .position(|count| {
                cumulative += u64::from(*count);
                cumulative >= rank
            })
            .unwrap()
    };
    json!({"p50":quantile(50),"p90":quantile(90),"p99":quantile(99),
        "max":counts.iter().rposition(|&count| count != 0).unwrap()})
}

pub fn compute(store: &ArenaStore, scratch: &Path) -> Result<Value> {
    let arena = store.archived();
    let n = arena.len();
    ensure!(
        n <= u32::MAX as usize && n.checked_add(1).and_then(|v| v.checked_mul(4)).is_some(),
        "snapshot is too large"
    );
    ensure!(
        [
            arena.dfs_records.len(),
            arena.dfs_positions.len(),
            arena.dfs_ends.len(),
            arena.mtimes.len(),
            arena.sizes.len()
        ]
        .iter()
        .all(|&len| len == n),
        "invalid snapshot column lengths"
    );
    let temp = tempfile::Builder::new()
        .prefix("fingerprint-")
        .tempdir_in(scratch)?;
    let mut depths = Spool::<u32>::zeroed(&temp.path().join("depths.spool"), n, |_| {})?;
    let mut children = Spool::<u32>::zeroed(&temp.path().join("children.spool"), n, |_| {})?;
    let mut depth_hist =
        Spool::<u32>::zeroed(&temp.path().join("depth-hist.spool"), n + 1, |_| {})?;
    let mut fanout_hist =
        Spool::<u32>::zeroed(&temp.path().join("fanout-hist.spool"), n + 1, |_| {})?;
    let (mut roots, mut largest_nonroot, mut directories, mut exact) = (0u64, 0u64, 0u64, 0u64);
    for position in 0..n {
        let record = arena.dfs_record(position as u32);
        ensure!(
            (record as usize) < n && arena.dfs_position(record) == position as u32,
            "invalid DFS permutation"
        );
        let subtree = arena.subtree(record);
        ensure!(
            subtree.start == position as u32
                && subtree.end > subtree.start
                && subtree.end as usize <= n,
            "invalid subtree interval"
        );
        let parent = arena.tree_parent(record);
        let depth = if parent == PARENT_NONE {
            roots += 1;
            0
        } else {
            ensure!(
                (parent as usize) < n && arena.dfs_position(parent) < position as u32,
                "invalid canonical parent"
            );
            children.set(parent as usize, children.get(parent as usize) + 1);
            largest_nonroot = largest_nonroot.max(u64::from(subtree.end - subtree.start));
            depths.get(parent as usize) + 1
        };
        depths.set(record as usize, depth);
        depth_hist.set(depth as usize, depth_hist.get(depth as usize) + 1);
        directories += u64::from(arena.is_dir(record));
        exact += u64::from(arena.size_exact(record));
    }
    for record in 0..n {
        if arena.is_dir(record as u32) {
            let fanout = children.get(record) as usize;
            fanout_hist.set(fanout, fanout_hist.get(fanout) + 1);
        }
    }

    let mut lengths = vec![0u32; NAME_LIMIT + 1];
    let mut byte_counts = [0u64; 256];
    let (mut name_bytes, mut distinct, mut non_ascii, mut with_extension) =
        (0u64, 0u64, 0u64, 0u64);
    let mut previous = Vec::new();
    let mut names_seen = 0usize;
    let mut extensions =
        crate::extension_counts::ExtensionCounts::new(temp.path(), 4 * 1024 * 1024);
    let mut failure = None;
    arena.for_each_name(|record, name| {
        names_seen += 1;
        if name.len() > NAME_LIMIT {
            failure = Some(anyhow::anyhow!(
                "fingerprinting supports names up to 65536 bytes"
            ));
            return ControlFlow::Break(());
        }
        lengths[name.len()] += 1;
        non_ascii += u64::from(!name.is_ascii());
        name_bytes += name.len() as u64;
        for &byte in name {
            byte_counts[byte as usize] += 1;
        }
        if record == 0 || !name.eq_ignore_ascii_case(&previous) {
            distinct += 1;
            previous.clear();
            previous.extend_from_slice(name);
        }
        if !arena.is_dir(record) {
            if let Some(dot) = name
                .iter()
                .rposition(|&b| b == b'.')
                .filter(|&i| i > 0 && i + 1 < name.len())
            {
                with_extension += 1;
                if let Err(error) = extensions.add(&name[dot + 1..]) {
                    failure = Some(error);
                    return ControlFlow::Break(());
                }
            }
        }
        ControlFlow::Continue(())
    });
    if let Some(error) = failure {
        return Err(error);
    }
    ensure!(names_seen == n, "incomplete name stream");
    let (extension_count, top10) = extensions.finish()?;
    let entropy: f64 = byte_counts
        .into_iter()
        .filter(|&count| count != 0)
        .map(|count| {
            let p = count as f64 / name_bytes as f64;
            -p * p.log2()
        })
        .sum();
    let files = n as u64 - directories;
    let trigram = trigram_report(arena)?;
    Ok(json!({"schema":1,"format_version":arena.format_version(),
        "size":{"records":n,"directories":directories,"files":files,"file_to_directory_ratio":ratio(files,directories),
            "snapshot_bytes":store.snapshot_file().metadata()?.len(),"snapshot_bytes_per_record":ratio(store.snapshot_file().metadata()?.len(),n as u64)},
        "shape":{"canonical_roots":roots,"depth":distribution(depth_hist.as_slice(),n as u64),
            "directory_fanout":distribution(fanout_hist.as_slice(),directories),"largest_nonroot_subtree_records":largest_nonroot,
            "largest_nonroot_subtree_share":ratio(largest_nonroot,n as u64)},
        "names":{"utf8_byte_length":distribution(&lengths,n as u64),"distinct_ascii_ci_names":distinct,
            "distinct_ascii_ci_name_ratio":ratio(distinct,n as u64),"byte_entropy_bits_per_byte":entropy,
            "non_ascii_name_share":ratio(non_ascii,n as u64)},
        "extensions":{"distinct_ascii_ci_extensions":extension_count,"files_with_extension":with_extension,
            "extensionless_files":files-with_extension,"top10_file_count":top10,"top10_share_of_all_files":ratio(top10,files),
            "top10_share_of_extension_files":ratio(top10,with_extension)},
        "sizes":{"records_with_exact_size":exact,"exact_size_share":ratio(exact,n as u64)},
        "trigrams":trigram,
        "method":{"tree":"canonical tree_parent; roots have depth zero; roots excluded from largest subtree",
            "names":"ASCII case-insensitive equivalence; original UTF-8 byte length and entropy; no Unicode folding",
            "extensions":"files only; final dot; leading-dot-only and trailing-dot names have no extension; ASCII case-insensitive",
            "private_memory":"fixed histograms/buffers and 4 MiB extension map budget; per-record columns are file-backed; scratch deleted on return",
            "interpretation":"descriptive vector, not a hardware-normalized score or a prediction of latency"}}))
}

fn trigram_report(arena: &ArchivedArena) -> Result<Value> {
    use scry_core::trigram::{for_each_trigram, num_blocks, row_bytes, TRIGRAM_ROWS};
    let blocks = num_blocks(arena.len());
    let stride = row_bytes(blocks);
    let table = arena.trigram_index.as_slice();
    ensure!(
        table.is_empty() || table.len() == TRIGRAM_ROWS * stride,
        "invalid trigram matrix size"
    );
    let used = if table.is_empty() {
        0
    } else {
        table
            .chunks_exact(stride)
            .filter(|row| row.iter().any(|&b| b != 0))
            .count()
    };
    let mut state = SEED;
    let mut name = Vec::new();
    let mut observations: [Vec<u32>; 3] = std::array::from_fn(|_| Vec::with_capacity(DRAWS));
    if !arena.is_empty() {
        for _ in 0..DRAWS {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            arena.name_into((state % arena.len() as u64) as u32, &mut name);
            ensure!(
                name.len() <= NAME_LIMIT,
                "sample name exceeds fingerprint limit"
            );
            for width in 3..=5 {
                if name.len() < width {
                    continue;
                }
                let start = (name.len() - width) / 2;
                let needle = &name[start..start + width];
                if std::str::from_utf8(needle).is_err() {
                    continue;
                }
                let mut hashes = [0usize; 3];
                let mut count = 0;
                for_each_trigram(needle, |hash| {
                    let hash = hash as usize;
                    if !hashes[..count].contains(&hash) {
                        hashes[count] = hash;
                        count += 1;
                    }
                });
                let survivors = if table.is_empty() {
                    blocks as u32
                } else {
                    (0..stride)
                        .map(|byte| {
                            let mut bits = u8::MAX;
                            for hash in &hashes[..count] {
                                bits &= table[hash * stride + byte];
                            }
                            if byte + 1 == stride && !blocks.is_multiple_of(8) {
                                bits &= (1u8 << (blocks % 8)) - 1;
                            }
                            bits.count_ones()
                        })
                        .sum()
                };
                observations[width - 3].push(survivors);
            }
        }
    }
    let survival: Vec<_> = observations.into_iter().enumerate().map(|(i,mut values)| {
        values.sort_unstable();
        let quantile = |percent: usize| values.get((values.len() * percent).div_ceil(100).saturating_sub(1)).and_then(|&n| ratio(n as u64, blocks as u64));
        json!({"needle_bytes":i+3,"samples":values.len(),"p50_block_survival":quantile(50),"p90_block_survival":quantile(90),"p99_block_survival":quantile(99)})
    }).collect();
    Ok(
        json!({"hashed_rows_used":used,"rows":TRIGRAM_ROWS,"blocks":blocks,"block_survival":survival,
        "sampling":{"seed":format!("{SEED:016x}"),"record_draws":DRAWS,"method":"record-weighted draws with replacement; middle 3-5-byte valid UTF-8 slices; invalid boundaries skipped; duplicates retained"}}),
    )
}

pub fn command() -> Result<()> {
    let mut snapshot = None;
    let mut output = std::path::PathBuf::from(".scratch/benchmark/fingerprint.json");
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if matches!(arg.as_str(), "--help" | "-h") {
            println!("scry-bench fingerprint --snapshot FILE [--output .scratch/benchmark/fingerprint.json]\nReports aggregate shape, names, extensions, size provenance, and seeded trigram survival; no names or paths are emitted.");
            return Ok(());
        }
        let value = args.next().context("fingerprint option needs a value")?;
        match arg.as_str() {
            "--snapshot" => snapshot = Some(std::path::PathBuf::from(value)),
            "--output" => output = value.into(),
            _ => anyhow::bail!("unknown fingerprint option"),
        }
    }
    let path = snapshot.context("fingerprint requires --snapshot FILE")?;
    let mut out = crate::scratch_writer(&output)?;
    let scratch = output.parent().unwrap();
    let store = ArenaStore::open(&path)?;
    let mut report = compute(&store, scratch)?;
    report["snapshot_sha256"] = json!(sha256_snapshot(&store)?);
    serde_json::to_writer_pretty(&mut out, &report)?;
    use std::io::Write;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}
