/// Per-request timings and counters collected by the daemon on demand.
///
/// The search APIs accept this behind an `Option`; callers that do not need
/// instrumentation do not take timestamps on the search path.
#[derive(Default, Clone, Copy, Debug)]
pub struct QuerySpans {
    pub match_ns: u64,
    pub rank_ns: u64,
    pub materialize_ns: u64,
    pub encode_ns: u64,
    pub candidates: u64,
    pub emitted: u64,
    pub blocks_scanned: u64,
    pub blocks_total: u64,
}
