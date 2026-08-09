/// Per-request timings and counters collected by the daemon on demand.
///
/// The search APIs accept this behind an `Option`; callers that do not need
/// instrumentation do not take timestamps on the search path.
#[derive(Default, Clone, Copy, Debug)]
pub struct QuerySpans {
    /// Select matching base records and retain the best base hits. Streaming
    /// queries fuse those operations, so they deliberately share one span.
    pub select_ns: u64,
    /// Merge live delta additions into the bounded heap and drain final hits.
    pub finalize_ns: u64,
    /// Re-rank and truncate the already-bounded per-volume hit sets.
    pub merge_ns: u64,
    pub materialize_ns: u64,
    pub encode_ns: u64,
    pub candidates: u64,
    pub emitted: u64,
    pub blocks_scanned: u64,
    pub blocks_total: u64,
}
