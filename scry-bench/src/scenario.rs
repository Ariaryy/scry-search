//! Frozen single/multiple-volume scenarios, including static live-delta overlays.
use anyhow::{ensure, Context, Result};
use scry_core::{view::SearchOptions, ArenaStore, IndexView, Query};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub fn load(paths: &[(PathBuf, Option<PathBuf>)]) -> Result<(Vec<IndexView>, Option<String>)> {
    ensure!(paths.len() <= 16, "at most sixteen reference volumes");
    let mut views = Vec::new();
    let mut identity = Sha256::new();
    let mut plain = None;
    for (path, overlay) in paths {
        let mut view = IndexView::new(Arc::new(ArenaStore::open(path)?));
        let sha = crate::fingerprint::sha256_snapshot(&view.base)?;
        identity.update(sha.as_bytes());
        plain = Some(sha);
        if let Some(path) = overlay {
            use std::io::Read;
            let mut bytes = Vec::new();
            std::fs::File::open(path)?
                .take(64 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() <= 64 * 1024 * 1024,
                "reference overlay exceeds 64 MiB"
            );
            identity.update((bytes.len() as u64).to_le_bytes());
            identity.update(&bytes);
            view.delta = Arc::new(
                scry_core::delta::Delta::decode_query_overlay(&bytes, view.base.archived().len())
                    .context("invalid reference overlay")?,
            );
        } else {
            identity.update(0u64.to_le_bytes());
        }
        views.push(view);
    }
    let sha = if paths.is_empty() {
        None
    } else if paths.len() == 1 && paths[0].1.is_none() {
        plain
    } else {
        Some(format!("{:x}", identity.finalize()))
    };
    Ok((views, sha))
}

/// Keep at most volumes * limit integer keys and construct only the final
/// limit paths. The rank-bit/slot/record tuple is the cross-volume ordering.
pub fn reference(
    views: &[IndexView],
    query: &Query,
    options: SearchOptions,
) -> Vec<scry_client::ResultEntry> {
    let mut retained = BTreeMap::new();
    for (slot, view) in views.iter().enumerate() {
        for hit in view.search_hits(query, options) {
            retained.insert((hit.rank_bits, slot, hit.record), hit);
        }
    }
    retained
        .into_iter()
        .take(options.limit)
        .map(|((_, slot, _), hit)| views[slot].materialize_one(&hit))
        .collect()
}

pub fn command() -> Result<()> {
    let mut output = PathBuf::from(".scratch/benchmark/scenario");
    let mut records = 10000;
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!("scry-bench scenario --output .scratch/... [--records 10000..5000000]\nCreates two synthetic snapshots plus bounded addition/tombstone overlays and a private workload. Use repeated --snapshot/--overlay in the runner and repeated --benchmark-snapshot/--benchmark-overlay in scryd.");
            return Ok(());
        }
        let value = args.next().context("scenario option needs a value")?;
        match arg.as_str() {
            "--output" => output = value.into(),
            "--records" => records = value.parse()?,
            _ => anyhow::bail!("unknown scenario option"),
        }
    }
    let mut manifest = crate::scratch_writer(&output.join("scenario.json"))?;
    let mut identities = Vec::new();
    for slot in 0..2 {
        let path = output.join(format!("volume-{slot}.rkyv"));
        ensure!(!path.exists(), "scenario snapshot already exists");
        crate::synthetic::build(&path, records, 0x5eed034d + slot, &output)?;
        let store = ArenaStore::open(&path)?;
        let arena = store.archived();
        let mut delta = scry_core::delta::Delta::new(arena.len());
        let root = arena.dfs_records[0];
        let mut removed = 0;
        arena.for_each_name(|record, _| {
            if !arena.is_dir(record) && removed < records / 100 {
                delta.tombstones.set(record);
                removed += 1;
            }
            std::ops::ControlFlow::Continue(())
        });
        for i in 0..records / 100 {
            delta.added.push(scry_core::delta::DeltaRecord {
                name: format!("report_delta_{slot}_{i:08}.txt"),
                parent: scry_core::delta::ParentRef::Base(root),
                mtime_secs: 1_700_000_100 + i as u32,
                is_dir: false,
                size_bytes: 4096 + i as u64,
                size_exact: true,
                live: true,
            });
        }
        let overlay = delta.encode_query_overlay();
        let mut out = crate::scratch_writer(&output.join(format!("volume-{slot}.overlay")))?;
        use std::io::Write;
        out.write_all(&overlay)?;
        identities.push(serde_json::json!({"slot":slot,"base_records":arena.len(),"additions":delta.added.len(),"tombstones":removed,"snapshot_sha256":crate::fingerprint::sha256_snapshot(&store)?,"overlay_sha256":format!("{:x}",Sha256::digest(&overlay))}));
    }
    let mut workload = crate::scratch_writer(&output.join("workload.json"))?;
    serde_json::to_writer_pretty(
        &mut workload,
        &serde_json::json!({"corpus":{"label":"synthetic-multi-volume-delta","records":records*2,"volumes":2,"delta_state":"frozen 1% additions and 1% tombstones per volume","cache_state":"uncontrolled"},"cases":[{"label":"delta-positive","shape":"static-overlay","kind":"substring","order":"relevance","patterns":["report_delta"]},{"label":"delta-largest","shape":"static-overlay-filter","kind":"path_terms","order":"largest","patterns":["ext:txt"]},{"label":"negative","shape":"negative","kind":"substring","order":"relevance","patterns":["zqxjv_absent"]},{"label":"broad","shape":"broad","kind":"prefix","order":"recent","patterns":["a"]}]}),
    )?;
    serde_json::to_writer_pretty(
        &mut manifest,
        &serde_json::json!({"schema":1,"volumes":identities,"privacy":"all names and roots are synthetic","shared_policy":"one volume supports shared overlay; multiple volumes must explicitly fall back to RPC"}),
    )?;
    Ok(())
}
