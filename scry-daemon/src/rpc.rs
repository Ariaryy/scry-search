//! Named-pipe request handling and per-connection cancellation.

use scry_core::metrics::QuerySpans;
use scry_core::protocol::{
    decode_request, encode_results, encode_shared_index, QueryKind, SharedIndexResponse,
};
use scry_core::view::SearchOptions;
use scry_core::Query;
use std::sync::Arc;

use crate::metrics::query_metrics;
use crate::search::{search_indexes_with_cache_with_spans, RefinementCache};
use crate::sections::shared_section;
use crate::VolumeIndexes;

pub(super) fn serve_indexes(indexes: VolumeIndexes, pipe_name: &str) -> std::io::Result<()> {
    eprintln!("scryd: listening on {pipe_name}");
    let server = scry_ipc::PipeServer::new(pipe_name)?;
    loop {
        match server.accept() {
            Ok(pipe) => {
                let indexes = indexes.clone();
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(pipe, &indexes) {
                        eprintln!("scryd: connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("scryd: accept error: {e}"),
        }
    }
}

/// A connection does its read/search/write cycle on one thread — the pipe's
/// synchronous handle deadlocks if a blocking read on one thread and a
/// blocking write on another are both in flight at once (see the `Sync`
/// safety note on `scry_ipc::Pipe`) — while a second, poll-only thread bumps
/// `generation` whenever it sees unread bytes already sitting in the pipe.
/// That's the cancellation signal an in-flight search checks periodically
/// (via `Cancellation`) to abandon a stale scan once a newer request has
/// already arrived, while this thread still writes exactly one response
/// frame per request, so framing stays 1:1 even for a superseded (empty)
/// result.
#[derive(Default)]
pub(super) struct ConnectionCancellation {
    pub(super) generation: std::sync::atomic::AtomicU64,
    // A pending-byte observation and its publication are one operation with
    // respect to starting a request. Otherwise a peek of this request's own
    // bytes can publish after its generation snapshot and cancel it spuriously.
    pub(super) observation: std::sync::Mutex<()>,
}

impl ConnectionCancellation {
    pub(super) fn observe_pending(
        &self,
        peek: impl FnOnce() -> std::io::Result<u32>,
    ) -> std::io::Result<()> {
        let _observation = self.observation.lock().unwrap();
        if peek()? != 0 {
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    pub(super) fn begin_request(&self) -> scry_core::Cancellation<'_> {
        let _observation = self.observation.lock().unwrap();
        scry_core::Cancellation::new(
            &self.generation,
            self.generation.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

pub(super) fn handle_connection(
    pipe: scry_ipc::Pipe,
    indexes: &VolumeIndexes,
) -> std::io::Result<()> {
    let pipe = Arc::new(pipe);
    let cancellation = Arc::new(ConnectionCancellation::default());
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let peek_pipe = Arc::clone(&pipe);
    let peek_cancellation = Arc::clone(&cancellation);
    let peek_stop = Arc::clone(&stop);
    let peeker = std::thread::spawn(move || {
        while !peek_stop.load(std::sync::atomic::Ordering::Relaxed) {
            if peek_cancellation
                .observe_pending(|| peek_pipe.pending_bytes())
                .is_err()
            {
                return; // client disconnected
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });

    let mut cache = RefinementCache::default();
    let result = (|| -> std::io::Result<()> {
        loop {
            let req_bytes = match pipe.read_frame() {
                Ok(b) => b,
                Err(_) => return Ok(()), // client disconnected
            };
            let Some(req) = decode_request(&req_bytes) else {
                continue;
            };
            let cancel = cancellation.begin_request();
            let mut spans = QuerySpans::default();
            let response = handle_request(&req, indexes, &pipe, cancel, &mut cache, &mut spans)?;
            let write_started = (req.kind != QueryKind::QueryStats).then(std::time::Instant::now);
            pipe.write_frame(&response)?;
            if let Some(started) = write_started {
                spans.encode_ns = spans
                    .encode_ns
                    .saturating_add(started.elapsed().as_nanos() as u64);
                if req.kind != QueryKind::ShareIndex {
                    query_metrics().lock().unwrap().record(spans);
                }
            }
        }
    })();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = peeker.join();
    result
}

/// Builds the response frame for one request. Returns an empty results frame
/// for a search request that `cancel` reports as superseded before or during
/// the scan; `ShareIndex` requests are never cancellable (they're answered
/// from an already-published snapshot, not a scan).
pub(super) fn handle_request(
    req: &scry_core::protocol::Request,
    indexes: &VolumeIndexes,
    pipe: &scry_ipc::Pipe,
    cancel: scry_core::Cancellation,
    cache: &mut RefinementCache,
    spans: &mut QuerySpans,
) -> std::io::Result<Vec<u8>> {
    if req.kind == QueryKind::Status {
        if req.pattern != "SCRYSTA1" || req.limit != 0 {
            return Ok(b"SCRYUNS1".to_vec());
        }
        return Ok(scry_core::protocol::encode_status(
            &scry_core::protocol::DaemonStatus::current(indexes.len() == 1),
        ));
    }
    // `load_full` clones the Arc, keeping this index alive for the whole
    // query even if the reindex thread swaps in a new one mid-search.
    if req.kind == QueryKind::ShareIndex && indexes.len() != 1 {
        return Ok(encode_results(&[]));
    }
    if req.kind == QueryKind::ShareIndex {
        let snapshot = indexes[0].store.load_full();
        if req.pattern.parse::<u64>().ok() == Some(snapshot.generation) {
            return Ok(encode_shared_index(&SharedIndexResponse {
                handle: 0,
                len: 0,
                generation: snapshot.generation,
                overlay: Vec::new(),
            }));
        }
        let section = shared_section(&snapshot)?;
        let handle = section.duplicate_for(pipe.client_process_id()?)?;
        return Ok(encode_shared_index(&SharedIndexResponse {
            handle,
            len: section.len() as u64,
            generation: snapshot.generation,
            overlay: snapshot.delta.encode_query_overlay(),
        }));
    }
    if req.kind == QueryKind::QueryStats {
        let mut report = query_metrics().lock().unwrap().report();
        report.push_str(&format!(
            "\nindex_volumes={} index_records={} index_delta_events={}",
            indexes.len(),
            indexes
                .iter()
                .map(|index| index.store.load().len())
                .sum::<usize>(),
            indexes
                .iter()
                .map(|index| {
                    let view = index.store.load();
                    view.delta.added.len() + view.delta.tombstones.count_ones() as usize
                })
                .sum::<usize>()
        ));
        report.push_str(&format!("\nrefinement_diagnostics=1 last_reused_volumes={} last_scanned_volumes={} last_bypassed={} last_cancelled={}",cache.last_reused,cache.last_scanned,cache.last_bypassed,cache.last_cancelled));
        return Ok(report.into_bytes());
    }
    let query = match req.kind {
        QueryKind::Prefix => Query::Prefix(req.pattern.clone()),
        QueryKind::Substring => Query::Substring(req.pattern.clone()),
        QueryKind::Wildcard => Query::wildcard(&req.pattern),
        QueryKind::PathTerms => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .min(u32::MAX as u64) as u32;
            scry_core::terms::parse_query(&req.pattern, now)
                .unwrap_or_else(|_| Query::PathTerms(Vec::new()))
        }
        QueryKind::ShareIndex => unreachable!(),
        QueryKind::QueryStats | QueryKind::Status => unreachable!(),
    };
    let entries = search_indexes_with_cache_with_spans(
        indexes,
        req.kind,
        &query,
        SearchOptions::ordered(req.limit as usize, req.order),
        cancel,
        cache,
        spans,
    );
    cache.last_cancelled = cancel.is_cancelled();
    let started = std::time::Instant::now();
    let response = encode_results(&entries);
    spans.encode_ns = spans
        .encode_ns
        .saturating_add(started.elapsed().as_nanos() as u64);
    Ok(response)
}
