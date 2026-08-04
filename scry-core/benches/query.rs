//! Query-path benchmarks over a synthetic, deterministic corpus.
//!
//! The corpus is generated from a fixed seed rather than read from a real
//! volume, so numbers are reproducible across machines and the repository
//! carries no filesystem layout of whoever ran it last.
//!
//! The variable these benchmarks are built around is **trigram block
//! selectivity**: the fraction of the volume's 1024-record blocks that survive
//! a term's trigram filter. It, not the hit count and not the term length,
//! is what decides how long a `PathTerms` query takes — a term matching two
//! files can still cost a near-full scan if its trigrams are spread thinly
//! across every block. Each search benchmark therefore names the selectivity
//! it is measuring, and `selectivity` reports the corpus's actual figures so a
//! change in the corpus generator can't silently invalidate the labels.
//!
//! Run with `cargo bench -p scry-core`.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};

use scry_core::pathindex::PathIndex;
use scry_core::store::ArenaStore;
use scry_core::view::IndexView;
use scry_core::{Arena, Query};

/// xorshift64*, so the corpus is identical on every machine and every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

const FILES: usize = 400_000;
const DIRS: usize = 40_000;

/// Tokens placed at a controlled rate, chosen to span the selectivity range.
///
/// Selectivity is driven by how a token is *distributed* over name-sorted
/// order, not by how often it occurs. A token that appears as an infix lands
/// in names scattered across the whole alphabet, so once it occurs in more
/// than roughly one name per block it lights up nearly every block. A token
/// that only ever appears as a prefix clusters into a contiguous run of
/// blocks and stays selective even when it is common. Both shapes are here
/// because real queries hit both.
const INFIX_COMMON: &str = "one";
const INFIX_MID: &str = "report";
const INFIX_RARE: &str = "zqxjv";
const PREFIX_CLUSTERED: &str = "archive";

const STEMS: [&str; 12] = [
    "budget", "invoice", "letter", "memo", "notes", "photo", "recipe", "sketch", "summary",
    "ticket", "video", "draft",
];
const EXTS: [&str; 8] = ["txt", "pdf", "png", "mkv", "docx", "rs", "log", "zip"];

fn synth_name(rng: &mut Rng, ordinal: usize) -> String {
    let stem = STEMS[rng.below(STEMS.len())];
    let ext = EXTS[rng.below(EXTS.len())];
    // Rates chosen to land the tokens at distinct selectivities; `selectivity`
    // prints what they actually achieve.
    let roll = rng.below(1000);
    if roll < 8 {
        // Clustered: always leads the name, so it occupies a contiguous run
        // of name-sorted blocks.
        return format!("{PREFIX_CLUSTERED}_{stem}_{ordinal}.{ext}");
    }
    if roll < 400 {
        return format!("{stem}_{INFIX_COMMON}_{ordinal}.{ext}");
    }
    if roll < 460 {
        return format!("{stem}_{INFIX_MID}_{ordinal}.{ext}");
    }
    if roll < 462 {
        return format!("{stem}_{INFIX_RARE}_{ordinal}.{ext}");
    }
    format!("{stem}_{ordinal}.{ext}")
}

/// Builds the corpus and persists it, because `IndexView` searches an
/// `ArchivedArena` and the archived accessors are a different code path from
/// the builder-side ones. Benchmarking the in-memory `Arena` would measure
/// something the daemon never runs.
fn corpus() -> (IndexView, tempfile::TempDir) {
    let mut rng = Rng(0x5EED_1234_5678_9ABC);
    let mut builder = Arena::builder();
    let root = builder.push("V:", 0, true);

    // Level-by-level with a fixed branching factor, so depth stays near a
    // real volume's (7 levels, ~8 children each). Drawing parents from a
    // sliding window of recently-created directories instead produces a
    // caterpillar thousands of levels deep, which inflates every `full_path`
    // and quietly turns a path-reconstruction benchmark into a tree-depth one.
    const BRANCHING: usize = 8;
    let mut dirs = Vec::with_capacity(DIRS + 1);
    dirs.push(root);
    let mut level_start = 0usize;
    let mut index = 0usize;
    while index < DIRS {
        let level_end = dirs.len();
        for slot in level_start..level_end {
            for _ in 0..BRANCHING {
                if index >= DIRS {
                    break;
                }
                let stem = STEMS[rng.below(STEMS.len())];
                let name = if rng.below(100) < 5 {
                    format!("{PREFIX_CLUSTERED}_{stem}_{index}")
                } else {
                    format!("{stem}_dir_{index}")
                };
                let node = builder.push(&name, 0, true);
                builder.set_parent(node, dirs[slot]);
                dirs.push(node);
                index += 1;
            }
        }
        level_start = level_end;
    }

    for index in 0..FILES {
        let parent = dirs[rng.below(dirs.len())];
        let name = synth_name(&mut rng, index);
        let node = builder.push(&name, 1_700_000_000, false);
        builder.set_parent(node, parent);
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench.rkyv");
    scry_core::store::save(&builder.build().0, &path).expect("save corpus");
    let store = Arc::new(ArenaStore::open(&path).expect("open corpus"));
    (IndexView::new(store), dir)
}

/// Not a timing benchmark: prints the corpus's actual trigram selectivity and
/// shape, so the labels the search benchmarks use stay honest if the
/// generator changes.
fn selectivity(c: &mut Criterion) {
    let (view, _dir) = corpus();
    let arena = view.base.archived();
    let blocks = arena.len().div_ceil(1024);
    eprintln!(
        "\ncorpus: {} records, {} directories, {} blocks",
        arena.len(),
        view.path_index.directory_count(),
        blocks,
    );
    // Both drivers, because they are independent and the slower one wins:
    // block selectivity bounds the name scan, while the number of matching
    // *directories* bounds the ancestor-closure and inherited-mask work.
    for term in [INFIX_COMMON, INFIX_MID, INFIX_RARE, PREFIX_CLUSTERED] {
        let selected = arena
            .candidate_blocks(term.as_bytes())
            .map_or(blocks, |b| b.len());
        let mut matching_dirs = 0usize;
        let lowered = term.to_ascii_lowercase();
        arena.for_each_name(|record, name| {
            if arena.is_dir(record) {
                let name = name.to_ascii_lowercase();
                if name
                    .windows(lowered.len())
                    .any(|window| window == lowered.as_bytes())
                {
                    matching_dirs += 1;
                }
            }
            std::ops::ControlFlow::Continue(())
        });
        eprintln!(
            "  {term:10} -> {selected:>5}/{blocks} blocks ({:5.1}% selectivity), \
             {matching_dirs:>5} matching dirs",
            100.0 * selected as f64 / blocks as f64,
        );
    }

    let mut depth_total = 0usize;
    let mut deepest = 0usize;
    for record in 0..arena.len() as u32 {
        let depth = arena.full_path(record, '\\').matches('\\').count();
        depth_total += depth;
        deepest = deepest.max(depth);
    }
    eprintln!(
        "  mean depth {:.1}, max depth {deepest}\n",
        depth_total as f64 / arena.len() as f64,
    );

    // Keeps criterion from warning about an empty group.
    c.bench_function("corpus/open", |b| {
        b.iter(|| black_box(view.base.archived().len()))
    });
}

fn path_terms(c: &mut Criterion) {
    let (view, _dir) = corpus();
    let mut group = c.benchmark_group("path_terms");
    let cases: [(&str, Vec<&str>); 6] = [
        ("rare", vec![INFIX_RARE]),
        ("clustered", vec![PREFIX_CLUSTERED]),
        ("mid", vec![INFIX_MID]),
        ("common", vec![INFIX_COMMON]),
        // The point of these two: a query costs what its *least* selective
        // term costs, because candidate blocks are unioned across terms. If
        // that ever stops being true, these two converge on `rare`.
        ("rare+common", vec![INFIX_RARE, INFIX_COMMON]),
        ("mid+clustered", vec![INFIX_MID, PREFIX_CLUSTERED]),
    ];
    for (label, terms) in cases {
        let query = Query::PathTerms(terms.iter().map(|t| (*t).to_string()).collect());
        group.bench_function(label, |b| {
            b.iter(|| black_box(view.search(black_box(&query), 200)))
        });
    }
    group.finish();
}

fn other_query_kinds(c: &mut Criterion) {
    let (view, _dir) = corpus();
    let mut group = c.benchmark_group("query_kind");
    let cases = [
        ("prefix", Query::Prefix("budget".to_string())),
        ("substring_mid", Query::Substring(INFIX_MID.to_string())),
        ("substring_rare", Query::Substring(INFIX_RARE.to_string())),
        ("wildcard", Query::wildcard("*.mkv")),
    ];
    for (label, query) in cases {
        group.bench_function(label, |b| {
            b.iter(|| black_box(view.search(black_box(&query), 200)))
        });
    }
    group.finish();
}

/// The per-query fixed cost that no amount of filtering avoids today: the
/// closure pass over every directory, and rebuilding the derived index.
fn fixed_costs(c: &mut Criterion) {
    let (view, _dir) = corpus();
    let mut group = c.benchmark_group("fixed");
    group.bench_function("path_index_build", |b| {
        b.iter(|| black_box(PathIndex::build(view.base.archived(), &view.delta)))
    });
    group.bench_function("closure_all_dirs", |b| {
        let mut mask = vec![0u16; view.path_index.directory_count()];
        b.iter(|| {
            mask.fill(0);
            view.path_index.closure(black_box(&mut mask));
        })
    });
    group.finish();
}

/// Path reconstruction, the cost any consumer that only wants to *count* or
/// *aggregate* records should never have to pay.
fn materialize(c: &mut Criterion) {
    let (view, _dir) = corpus();
    let arena = view.base.archived();
    let mut group = c.benchmark_group("materialize");
    for count in [200usize, 20_000] {
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut total = 0usize;
                for record in 0..count as u32 {
                    total += arena.full_path(record, '\\').len();
                }
                black_box(total)
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    selectivity,
    path_terms,
    other_query_kinds,
    fixed_costs,
    materialize
);
criterion_main!(benches);
