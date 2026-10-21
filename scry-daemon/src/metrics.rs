//! Bounded query metrics and process memory sampling.

use crate::ffi;
use scry_core::metrics::QuerySpans;

use crate::search::{query_thread_count, refinement_cache_enabled};
use crate::BENCHMARK_FROZEN;

#[derive(Default, Clone, Copy, Debug)]
pub(super) struct MemorySample {
    private_usage: u64,
    working_set: u64,
    peak_working_set: u64,
    page_faults: u32,
}

pub(super) struct QueryMetrics {
    count: u64,
    total: QuerySpans,
    max: QuerySpans,
    samples: [QuerySpans; 1024],
    next: usize,
    sample_count: usize,
    memory: MemorySample,
    last_memory: std::time::Instant,
}

impl QueryMetrics {
    fn new() -> Self {
        Self {
            count: 0,
            total: QuerySpans::default(),
            max: QuerySpans::default(),
            samples: [QuerySpans::default(); 1024],
            next: 0,
            sample_count: 0,
            memory: MemorySample::default(),
            last_memory: std::time::Instant::now() - std::time::Duration::from_secs(1),
        }
    }

    pub(super) fn record(&mut self, spans: QuerySpans) {
        self.count += 1;
        add_spans(&mut self.total, spans);
        max_spans(&mut self.max, spans);
        self.samples[self.next] = spans;
        self.next = (self.next + 1) % self.samples.len();
        self.sample_count = (self.sample_count + 1).min(self.samples.len());
        self.sample_memory(false);
    }

    fn sample_memory(&mut self, force: bool) {
        if force || self.last_memory.elapsed() >= std::time::Duration::from_secs(1) {
            self.memory = process_memory();
            self.last_memory = std::time::Instant::now();
        }
    }

    pub(super) fn report(&mut self) -> String {
        self.sample_memory(true);
        let mut out = format!("queries: {}\n", self.count);
        for (label, get) in [
            ("select_ns", span_select as fn(QuerySpans) -> u64),
            ("finalize_ns", span_finalize),
            ("merge_ns", span_merge),
            ("materialize_ns", span_materialize),
            ("encode_ns", span_encode),
            ("candidates", span_candidates),
            ("emitted", span_emitted),
            ("blocks_scanned", span_blocks_scanned),
            ("blocks_total", span_blocks_total),
        ] {
            let mut values: Vec<u64> = self.samples[..self.sample_count]
                .iter()
                .copied()
                .map(get)
                .collect();
            values.sort_unstable();
            let p50 = values.get(values.len() / 2).copied().unwrap_or_default();
            let p99 = values
                .get(values.len().saturating_mul(99) / 100)
                .copied()
                .unwrap_or_default();
            out.push_str(&format!(
                "{label}: sum={} max={} p50={p50} p99={p99}\n",
                get(self.total),
                get(self.max)
            ));
        }
        out.push_str(&format!(
            "private_usage={} working_set={} peak_working_set={} page_faults={}",
            self.memory.private_usage,
            self.memory.working_set,
            self.memory.peak_working_set,
            self.memory.page_faults,
        ));
        out.push_str(&format!(
            "\nquery_threads={} refinement_cache={} benchmark_frozen={}",
            query_thread_count(),
            refinement_cache_enabled(),
            BENCHMARK_FROZEN.load(std::sync::atomic::Ordering::Relaxed)
        ));
        let (mut affinity, mut system) = (0usize, 0usize);
        let affinity = if unsafe {
            ffi::GetProcessAffinityMask(ffi::GetCurrentProcess(), &mut affinity, &mut system)
        } != 0
        {
            affinity.to_string()
        } else {
            "unknown".into()
        };
        out.push_str(&format!(" affinity_mask={affinity}"));
        out
    }
}

static QUERY_METRICS: std::sync::OnceLock<std::sync::Mutex<QueryMetrics>> =
    std::sync::OnceLock::new();

pub(super) fn query_metrics() -> &'static std::sync::Mutex<QueryMetrics> {
    QUERY_METRICS.get_or_init(|| std::sync::Mutex::new(QueryMetrics::new()))
}

macro_rules! span_fields {
    ($target:expr, $source:expr, $op:expr) => {{
        $op(&mut $target.select_ns, $source.select_ns);
        $op(&mut $target.finalize_ns, $source.finalize_ns);
        $op(&mut $target.merge_ns, $source.merge_ns);
        $op(&mut $target.materialize_ns, $source.materialize_ns);
        $op(&mut $target.encode_ns, $source.encode_ns);
        $op(&mut $target.candidates, $source.candidates);
        $op(&mut $target.emitted, $source.emitted);
        $op(&mut $target.blocks_scanned, $source.blocks_scanned);
        $op(&mut $target.blocks_total, $source.blocks_total);
    }};
}

fn add_spans(target: &mut QuerySpans, source: QuerySpans) {
    span_fields!(target, source, |field: &mut u64, value| *field =
        field.saturating_add(value));
}

fn max_spans(target: &mut QuerySpans, source: QuerySpans) {
    span_fields!(target, source, |field: &mut u64, value| *field =
        (*field).max(value));
}

fn span_select(spans: QuerySpans) -> u64 {
    spans.select_ns
}
fn span_finalize(spans: QuerySpans) -> u64 {
    spans.finalize_ns
}
fn span_merge(spans: QuerySpans) -> u64 {
    spans.merge_ns
}
fn span_materialize(spans: QuerySpans) -> u64 {
    spans.materialize_ns
}
fn span_encode(spans: QuerySpans) -> u64 {
    spans.encode_ns
}
fn span_candidates(spans: QuerySpans) -> u64 {
    spans.candidates
}
fn span_emitted(spans: QuerySpans) -> u64 {
    spans.emitted
}
fn span_blocks_scanned(spans: QuerySpans) -> u64 {
    spans.blocks_scanned
}
fn span_blocks_total(spans: QuerySpans) -> u64 {
    spans.blocks_total
}

pub(super) fn process_memory() -> MemorySample {
    let mut counters: ffi::ProcessMemoryCountersEx = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<ffi::ProcessMemoryCountersEx>() as u32;
    if unsafe { ffi::GetProcessMemoryInfo(ffi::GetCurrentProcess(), &mut counters, counters.cb) }
        == 0
    {
        return MemorySample::default();
    }
    MemorySample {
        private_usage: counters.PrivateUsage as u64,
        working_set: counters.WorkingSetSize as u64,
        peak_working_set: counters.PeakWorkingSetSize as u64,
        page_faults: counters.PageFaultCount,
    }
}
