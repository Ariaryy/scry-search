//! Seeded, private corpus-derived workloads. Counts are exhaustive and do not
//! use a truncated top-k page. The only per-record scratch is file-backed.
use anyhow::{ensure, Context, Result};
use scry_core::{spool::Spool, ArenaStore, PARENT_NONE};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::ControlFlow,
    path::PathBuf,
};

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn contains(name: &[u8], term: &[u8]) -> bool {
    term.is_empty()
        || name
            .windows(term.len())
            .any(|w| w.eq_ignore_ascii_case(term))
}

/// Counts supported generated shapes independently of ranked search. Path
/// masks propagate through canonical DFS, avoiding path construction per hit.
pub fn count(
    store: &ArenaStore,
    kind: &str,
    pattern: &str,
    scratch: &std::path::Path,
) -> Result<u64> {
    ensure!(
        matches!(kind, "prefix" | "substring" | "path_terms"),
        "unsupported generated query kind"
    );
    let arena = store.archived();
    let mut total = 0;
    if kind == "path_terms" && !pattern.starts_with("ext:") {
        let scry_core::Query::PathTerms(terms) = scry_core::terms::parse_query(pattern, 0)
            .map_err(|_| anyhow::anyhow!("invalid generated terms"))?
        else {
            anyhow::bail!("unsupported generated metadata predicate");
        };
        ensure!(terms.len() == 2, "expected two generated terms");
        let terms: Vec<_> = terms
            .iter()
            .map(|s| s.to_ascii_lowercase().into_bytes())
            .collect();
        let temp = tempfile::tempdir_in(scratch)?;
        let mut masks = Spool::<u8>::zeroed(&temp.path().join("masks"), arena.len(), |_| {})?;
        let mut name = Vec::new();
        for record in arena.dfs_records.iter() {
            let record = *record;
            let parent = arena.tree_parent(record);
            let mut mask = if parent == PARENT_NONE {
                0
            } else {
                masks.get(parent as usize)
            };
            arena.name_into(record, &mut name);
            for (i, term) in terms.iter().enumerate() {
                if contains(&name, term) {
                    mask |= 1 << i;
                }
            }
            masks.set(record as usize, mask);
            total += u64::from(mask == 3);
        }
    } else {
        if kind == "path_terms" {
            let extension = pattern.strip_prefix("ext:").unwrap();
            ensure!(
                !extension.is_empty() && extension.bytes().all(|b| b.is_ascii_alphanumeric()),
                "unsupported generated extension predicate"
            );
        }
        let needle = pattern
            .strip_prefix("ext:")
            .unwrap_or(pattern)
            .to_ascii_lowercase();
        arena.for_each_name(|record, name| {
            let matches = match kind {
                "prefix" => name
                    .get(..needle.len())
                    .is_some_and(|p| p.eq_ignore_ascii_case(needle.as_bytes())),
                "substring" => contains(name, needle.as_bytes()),
                "path_terms" => {
                    !arena.is_dir(record)
                        && name.rsplit(|b| *b == b'.').next().is_some_and(|e| {
                            e.len() < name.len() && e.eq_ignore_ascii_case(needle.as_bytes())
                        })
                }
                _ => unreachable!(),
            };
            total += u64::from(matches);
            ControlFlow::Continue(())
        });
    }
    Ok(total)
}

pub fn bucket(count: u64) -> &'static str {
    match count {
        0..=9 => "lt10",
        10..=999 => "lt1k",
        1000..=99999 => "lt100k",
        _ => "ge100k",
    }
}

pub fn build(
    store: &ArenaStore,
    seed: u64,
    draws: usize,
    scratch: &std::path::Path,
) -> Result<(Value, Value, Value)> {
    let arena = store.archived();
    ensure!(!arena.is_empty(), "cannot sample an empty snapshot");
    let mut rng = Rng(seed);
    let mut candidates = BTreeSet::new();
    let mut future = Vec::new();
    let mut name = Vec::new();
    let mut ancestor = Vec::new();
    for _ in 0..draws {
        let record = rng.below(arena.len()) as u32;
        arena.name_into(record, &mut name);
        let Ok(text) = std::str::from_utf8(&name) else {
            continue;
        };
        for bytes in 1..=5 {
            if let Some(prefix) = text.get(..bytes).filter(|s| !s.is_empty()) {
                candidates.insert((
                    format!("prefix-{bytes}"),
                    "prefix".to_owned(),
                    prefix.to_owned(),
                ));
            }
        }
        let start = text.len() / 3;
        if let Some(middle) = text
            .get(start..(start + 5).min(text.len()))
            .filter(|s| !s.is_empty())
        {
            candidates.insert(("middle-substring".into(), "substring".into(), middle.into()));
        }
        if !arena.is_dir(record) {
            if let Some((_, ext)) = text.rsplit_once('.').filter(|(stem, ext)| {
                !stem.is_empty()
                    && !ext.is_empty()
                    && ext.bytes().all(|b| b.is_ascii_alphanumeric())
            }) {
                candidates.insert((
                    "extension".into(),
                    "path_terms".into(),
                    format!("ext:{ext}"),
                ));
            }
        }
        let parent = arena.tree_parent(record);
        if parent != PARENT_NONE {
            arena.name_into(parent, &mut ancestor);
            if let Ok(parent_name) = std::str::from_utf8(&ancestor) {
                // Quotes preserve spaces. Skip syntax with ambiguous path or
                // quote characters rather than silently changing its meaning.
                if [text, parent_name]
                    .iter()
                    .all(|s| !s.is_empty() && !s.contains(['"', '/', '\\', ':']))
                {
                    let pattern = format!("\"{parent_name}\" \"{text}\"");
                    if pattern.len() <= 256 {
                        candidates.insert(("ancestor-leaf".into(), "path_terms".into(), pattern));
                    }
                }
            }
        }
        // F is not implemented: preserve private future inputs, never label
        // current literal-search timings as fuzzy/acronym measurements.
        let chars: Vec<_> = text.chars().take(128).collect();
        if chars.len() > 2 {
            let mut typo = chars.clone();
            typo.swap(0, 1);
            let subsequence: String = chars.iter().step_by(2).collect();
            let acronym: String = text
                .split(|c: char| !c.is_alphanumeric())
                .filter_map(|s| s.chars().next())
                .collect();
            future.push(json!({"typo":typo.iter().collect::<String>(),"subsequence":subsequence,"acronym":acronym}));
        }
    }
    let mut groups: BTreeMap<(String, String), Vec<Value>> = BTreeMap::new();
    let mut evaluated = 0;
    for (shape, kind, pattern) in candidates {
        if pattern.len() > 256 {
            continue;
        }
        let hits = count(store, &kind, &pattern, scratch)?;
        evaluated += 1;
        let group = groups.entry((shape, bucket(hits).into())).or_default();
        // Bounded fixed panel per shape/bucket; lexicographic candidate order
        // is deterministic. Missing cells stay missing, never relabelled.
        if group.len() < 2 {
            group.push(json!({"kind":kind,"pattern":pattern,"matches":hits}));
        }
    }
    let mut cases = Vec::new();
    let mut counts = Vec::new();
    for ((shape, bucket), queries) in groups {
        for (index, query) in queries.iter().enumerate() {
            let label = format!("{shape}-{bucket}-{index}");
            cases.push(json!({"label":label,"shape":format!("{shape}/{bucket}"),"kind":query["kind"],"order":"relevance","patterns":[query["pattern"]]}));
            counts.push(
                json!({"case":label,"shape":shape,"bucket":bucket,"matches":query["matches"]}),
            );
        }
    }
    ensure!(
        !cases.is_empty(),
        "snapshot yielded no supported candidates"
    );
    let sha = crate::fingerprint::sha256_snapshot(store)?;
    Ok((
        json!({"corpus":{"label":"seeded-snapshot-workload","records":arena.len(),"volumes":1,"delta_state":"frozen empty delta","cache_state":"uncontrolled"},"cases":cases,"generation":{"seed":format!("{seed:016x}"),"snapshot_sha256":sha,"cases":counts}}),
        json!({"schema":1,"seed":format!("{seed:016x}"),"record_draws":draws,"snapshot_sha256":sha,"candidates_evaluated":evaluated,"selection":"at most two lexicographically ordered candidates per shape/bucket; record draws with replacement","count_method":"exhaustive name scan or canonical DFS term masks; no result limit","buckets":{"lt10":[0,9],"lt1k":[10,999],"lt100k":[1000,99999],"ge100k":[100000,null]},"cases":counts,"future_variants":{"status":"deferred until fuzzy execution exists","draws":future.len()}}),
        json!({"status":"not executable until fuzzy support exists","variants":future}),
    ))
}

pub fn command() -> Result<()> {
    let mut snapshot = None;
    let mut output = PathBuf::from(".scratch/benchmark/generated-workload.json");
    let (mut seed, mut draws) = (0x5eed_034b_0000_0001, 16);
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        if arg == "--help" {
            println!("scry-bench generate-workload --snapshot FILE --output .scratch/...json [--seed U64 --draws 1..256]\nCreates a private workload, aggregate .report.json, and private deferred .future.json; no query strings are printed.");
            return Ok(());
        }
        let value = args.next().context("generator option needs a value")?;
        match arg.as_str() {
            "--snapshot" => snapshot = Some(PathBuf::from(value)),
            "--output" => output = value.into(),
            "--seed" => seed = value.parse()?,
            "--draws" => draws = value.parse()?,
            _ => anyhow::bail!("unknown generator option"),
        }
    }
    ensure!(
        seed != 0 && (1..=256).contains(&draws),
        "nonzero seed and 1..256 draws required"
    );
    let mut out = crate::scratch_writer(&output)?;
    let mut report = crate::scratch_writer(&output.with_extension("report.json"))?;
    let mut future = crate::scratch_writer(&output.with_extension("future.json"))?;
    let store = ArenaStore::open(&snapshot.context("requires --snapshot")?)?;
    let (workload, counts, variants) = build(&store, seed, draws, output.parent().unwrap())?;
    let parsed: crate::workload::Workload = serde_json::from_value(workload.clone())?;
    parsed.validate()?;
    serde_json::to_writer_pretty(&mut out, &workload)?;
    serde_json::to_writer_pretty(&mut report, &counts)?;
    serde_json::to_writer_pretty(&mut future, &variants)?;
    Ok(())
}
