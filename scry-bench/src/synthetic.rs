//! Additional calibrated profiles; historical Criterion generators stay intact.
//! Generation, DFS and snapshot serialization use mapped/file-backed columns.
use crate::generate::Rng;
use anyhow::{ensure, Context, Result};
use scry_core::{
    arena::SpooledArenaBuilder, spool::Spool, store::ArenaColumns, ArenaStore, PARENT_NONE,
};
use serde_json::json;
use std::path::{Path, PathBuf};

fn weight(i: usize) -> u64 {
    match i % 100 {
        0..=49 => 2,
        50..=89 => 6,
        90..=98 => 25,
        _ => 180,
    }
}

fn select(prefix: &Spool<u64>, start: usize, end: usize, rng: &mut Rng) -> u32 {
    let base = prefix.get(start);
    let value = base + rng.next() % (prefix.get(end) - base);
    let (mut lo, mut hi) = (start, end);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if prefix.get(mid + 1) <= value {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo as u32
}

pub fn build(path: &Path, records: usize, seed: u64, scratch: &Path) -> Result<()> {
    build_profile(path, records, seed, scratch, false)
}

fn build_profile(
    path: &Path,
    records: usize,
    seed: u64,
    scratch: &Path,
    stress: bool,
) -> Result<()> {
    ensure!(
        (10_000..=5_000_000).contains(&records),
        "records must be 10000..5000000"
    );
    ensure!(seed != 0, "seed must be nonzero");
    let temp = tempfile::tempdir_in(scratch)?;
    let scratch = temp.path();
    let dirs = records * 1078 / 10000;
    let files = records - dirs;
    let unique_files = records * 328 / 1000 - dirs;
    let mut rng = Rng(seed);
    let mut builder = SpooledArenaBuilder::new(records, scratch, &|_| {})?;
    let mut prefix = Spool::<u64>::zeroed(&scratch.join("weights"), dirs + 1, |_| {})?;
    for i in 0..dirs {
        prefix.set(i + 1, prefix.get(i) + weight(i));
    }
    // Directory depth histogram. Files inherit this distribution plus one;
    // weighted attachment adds a long fanout tail without index-sized heaps.
    let weights = [
        0, 1, 1, 1, 1, 1, 100, 200, 500, 1200, 3500, 1500, 1000, 1000, 500, 250, 150, 50, 25, 15, 5,
    ];
    let sum: usize = weights.iter().sum();
    let mut levels = [(0usize, 0usize); 21];
    levels[0] = (0, 1);
    let mut at = 1;
    for depth in 1..21 {
        let count = if depth == 20 {
            dirs - at
        } else {
            ((dirs - 1) * weights[depth] / sum).max(1).min(dirs - at)
        };
        levels[depth] = (at, at + count);
        at += count;
    }
    builder.push(
        b"0_branch_00000000",
        1_700_000_000,
        true,
        0,
        true,
        PARENT_NONE,
    );
    for depth in 1..21 {
        let (start, end) = levels[depth];
        let (ps, pe) = levels[depth - 1];
        for i in start..end {
            builder.push(
                format!("0_branch_{i:08}").as_bytes(),
                1_700_000_000,
                true,
                0,
                true,
                if stress && i % 1000 == 0 {
                    PARENT_NONE
                } else {
                    select(&prefix, ps, pe, &mut rng)
                },
            );
        }
    }
    for group in 0..unique_files {
        // Fixed-width base-26 key preserves ASCII-CI sorted input while
        // distributing names across the alphabet, rather than one prefix.
        let mut key = [b'a'; 6];
        let mut ordinal = group * 26usize.pow(6) / unique_files;
        for byte in key.iter_mut().rev() {
            *byte += (ordinal % 26) as u8;
            ordinal /= 26;
        }
        let roll = rng.below(10000);
        let extension = if roll < 6256 {
            format!("e{:03}", rng.below(10))
        } else if roll < 8835 {
            format!("e{:03}", 10 + rng.below(624))
        } else {
            String::new()
        };
        let target = match rng.below(100) {
            0..=49 => 17,
            50..=84 => 25,
            85..=97 => 35,
            _ => 74,
        };
        let mut name = String::from_utf8(key.to_vec()).unwrap();
        name.push('_');
        const WORDS: [&str; 8] = [
            "archive", "report", "summary", "document", "notes", "image", "package", "record",
        ];
        let word = WORDS[rng.below(WORDS.len())];
        let suffix = if extension.is_empty() {
            String::new()
        } else {
            format!(".{extension}")
        };
        while name.len() + suffix.len() < target {
            name.push_str(word);
        }
        name.truncate(target - suffix.len());
        if stress && group % 64 == 0 {
            while name.len() < 190 {
                name.push('x');
            }
        }
        if stress && group % 16 == 0 {
            name.push_str("_café_数据");
        }
        name.push_str(&suffix);
        let copies = files / unique_files + usize::from(group < files % unique_files);
        for _ in 0..copies {
            let parent = if stress && group % 33 == 0 {
                0
            } else {
                select(&prefix, 0, dirs, &mut rng)
            };
            let mtime = 1_700_000_000 + (rng.next() % 10000) as u32;
            let size = rng.next() % 10_000_000;
            let exact = !stress || group % 100 != 0;
            builder.push(
                name.as_bytes(),
                mtime,
                false,
                if exact && (!stress || group % 111 != 0) {
                    size
                } else {
                    0
                },
                exact,
                parent,
            );
        }
    }
    let cols = builder.finish();
    let dfs = scry_core::dfs::build_file_backed(cols.parents.as_slice(), scratch, &|_| {})?;
    let exact = scry_core::arena::derive_spooled_size_exact(
        cols.parents.as_slice(),
        &cols.size_exact_inputs,
        &dfs,
        scratch,
        &|_| {},
    )?;
    let sizes = scry_core::dfs::prefix_sums_u64_file_backed(
        &dfs.records,
        &cols.sizes,
        &scratch.join("prefix-sizes"),
        &|_| {},
    )?;
    scry_core::store::save_columns_with(
        &ArenaColumns {
            format_version: scry_core::record::FORMAT_VERSION,
            journal_id: 0,
            next_usn: 0,
            volume_serial: 0,
            names: cols.names.as_slice(),
            bucket_offsets: cols.bucket_offsets.as_slice(),
            parents: cols.parents.as_slice(),
            mtimes: cols.mtimes.as_slice(),
            sizes: cols.sizes.as_slice(),
            size_exact_bits: exact.as_slice(),
            trigram_index: cols.trigram_index.as_slice(),
            dfs_positions: dfs.positions.as_slice(),
            dfs_records: dfs.records.as_slice(),
            dfs_ends: dfs.subtree_ends.as_slice(),
            dfs_size_prefix: sizes.as_slice(),
        },
        path,
        |_| {},
    )?;
    Ok(())
}

pub fn command() -> Result<()> {
    let mut output = PathBuf::from(".scratch/benchmark/calibrated-v1.rkyv");
    let (mut records, mut seed) = (440_000, 0x5eed_034c_0000_0001);
    let mut profile = "calibrated-v1".to_owned();
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!("scry-bench synthesize --output .scratch/...rkyv [--records 10000..5000000 --seed U64 --profile calibrated-v1|stress-v1]\nHistorical generators stay unchanged. stress-v1 adds wide fanout, multiple roots, Unicode/long names and unknown/empty sizes. Writes an aggregate fingerprint.");
            return Ok(());
        }
        let value = args.next().context("synthesis option needs a value")?;
        match arg.as_str() {
            "--output" => output = value.into(),
            "--records" => records = value.parse()?,
            "--seed" => seed = value.parse()?,
            "--profile" => profile = value,
            _ => anyhow::bail!("unknown synthesis option"),
        }
    }
    // Reserve new destinations using the same canonical scratch-only policy
    // as raw captures. The store writes atomically over this empty reservation.
    let reservation = crate::scratch_writer(&output)?;
    drop(reservation);
    let mut report = crate::scratch_writer(&output.with_extension("report.json"))?;
    ensure!(
        matches!(profile.as_str(), "calibrated-v1" | "stress-v1"),
        "unknown synthetic profile"
    );
    build_profile(
        &output,
        records,
        seed,
        output.parent().unwrap(),
        profile == "stress-v1",
    )?;
    let store = ArenaStore::open(&output)?;
    let mut fp = crate::fingerprint::compute(&store, output.parent().unwrap())?;
    fp["snapshot_sha256"] = json!(crate::fingerprint::sha256_snapshot(&store)?);
    serde_json::to_writer_pretty(
        &mut report,
        &json!({"profile":profile,"seed":format!("{seed:016x}"),"records":records,"fingerprint":fp,"limitations":"aggregate calibration or explicit stress coverage; not a replica of a real filesystem"}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn stress_profile_covers_unicode_forest_wide_fanout_and_size_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("stress.rkyv");
        super::build_profile(&path, 10000, 42, temp.path(), true).unwrap();
        let store = scry_core::ArenaStore::open(&path).unwrap();
        let report = crate::fingerprint::compute(&store, temp.path()).unwrap();
        assert!(report["shape"]["canonical_roots"].as_u64().unwrap() > 1);
        assert!(report["shape"]["directory_fanout"]["max"].as_u64().unwrap() > 200);
        assert!(report["names"]["non_ascii_name_share"].as_f64().unwrap() > 0.0);
        assert!(report["names"]["utf8_byte_length"]["max"].as_u64().unwrap() > 190);
        assert!(report["sizes"]["exact_size_share"].as_f64().unwrap() < 1.0);
    }
}
