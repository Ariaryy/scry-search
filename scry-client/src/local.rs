//! Validated shared-index execution with automatic RPC fallback.

use super::{Client, Order, QueryKind, ResultEntry, SearchExecution};
use scry_core::protocol::{decode_shared_index, encode_request, Request};
use scry_core::view::SearchOptions;

pub(super) struct LocalIndex {
    view: scry_ipc::SectionView,
    delta: scry_core::delta::Delta,
    generation: u64,
}

/// How long `search_local_ordered` will keep answering from its cached
/// mapping before re-asking the daemon whether a new generation exists.
/// The client cannot learn about a new generation except by asking, so this
/// trades up to this much staleness for skipping the handshake round trip on
/// every call — the same tradeoff the client already makes by querying an
/// immutable snapshot at all. 250ms comfortably covers as-you-type keystroke
/// cadence while still catching a compaction within about a second.
pub(super) const LOCAL_HANDSHAKE_STALENESS_WINDOW: std::time::Duration =
    std::time::Duration::from_millis(250);

impl Client {
    /// A daemon creates the next named-pipe instance immediately after
    /// accepting the previous one, but there is a small interval where
    /// `CreateFileW` reports ERROR_FILE_NOT_FOUND rather than PIPE_BUSY.
    /// Initial connections should still fail fast when no daemon exists;
    /// only this known-live fallback path retries that transient gap.
    fn reconnect_for_rpc_fallback(&mut self) -> anyhow::Result<()> {
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
        loop {
            match scry_ipc::connect_client(&self.pipe_name) {
                Ok(pipe) => {
                    self.pipe = pipe;
                    return Ok(());
                }
                Err(error)
                    if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND)
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Search a coherent base+delta generation in this process, falling back
    /// to daemon RPC when section sharing is unavailable.
    pub fn search_local(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        self.search_local_ordered(kind, pattern, limit, Order::default())
    }

    /// As [`Self::search_local`], but with an explicit result ordering.
    pub fn search_local_ordered(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
        order: Order,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        self.search_local_ordered_with_execution(kind, pattern, limit, order)
            .map(|(entries, _)| entries)
    }

    /// Local-first search with the actual execution route. The route describes
    /// this call, not whether a mapping was available on an earlier request.
    /// Sharing, validation, and fallback follow [`Self::search_local_ordered`].
    pub fn search_local_ordered_with_execution(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
        order: Order,
    ) -> anyhow::Result<(Vec<ResultEntry>, SearchExecution)> {
        if matches!(kind, QueryKind::ShareIndex | QueryKind::QueryStats) {
            return Err(anyhow::anyhow!("invalid search kind"));
        }

        // The client can only learn whether a new generation exists by
        // asking, so a fresh mapping always requires the round trip. Once one
        // is established, re-asking on every call defeats the point of
        // answering in-process — rate-limit to the staleness window instead.
        let should_handshake = self.local.is_none()
            || self
                .last_handshake
                .is_none_or(|last| last.elapsed() >= LOCAL_HANDSHAKE_STALENESS_WINDOW);

        if should_handshake {
            let request = Request {
                kind: QueryKind::ShareIndex,
                pattern: self
                    .local
                    .as_ref()
                    .map(|local| local.generation.to_string())
                    .unwrap_or_default(),
                limit: 0,
                order: Order::default(),
            };
            let shared = (|| -> anyhow::Result<_> {
                self.pipe.write_frame(&encode_request(&request))?;
                let frame = self.pipe.read_frame()?;
                decode_shared_index(&frame).ok_or_else(|| anyhow::anyhow!("sharing unsupported"))
            })();
            let shared = match shared {
                Ok(shared) => shared,
                Err(_) => {
                    self.reconnect_for_rpc_fallback()?;
                    return self
                        .query_ordered(kind, pattern, limit, order)
                        .map(|entries| (entries, SearchExecution::RpcFallback));
                }
            };
            if shared.handle != 0 {
                let mapped = (|| -> anyhow::Result<LocalIndex> {
                    let incoming = scry_ipc::SectionView::map(shared.handle, shared.len as usize)?;
                    let arena = scry_core::store::archived_bytes(incoming.as_bytes())?;
                    let delta =
                        scry_core::delta::Delta::decode_query_overlay(&shared.overlay, arena.len())
                            .ok_or_else(|| anyhow::anyhow!("malformed shared delta"))?;
                    Ok(LocalIndex {
                        view: incoming,
                        delta,
                        generation: shared.generation,
                    })
                })();
                match mapped {
                    Ok(local) => self.local = Some(local),
                    Err(_) => {
                        // The daemon announced a newer generation that this
                        // client could not validate. Do not knowingly serve
                        // the previously cached generation during the retry
                        // window; fall back and retry sharing next call.
                        self.local = None;
                        return self
                            .query_ordered(kind, pattern, limit, order)
                            .map(|entries| (entries, SearchExecution::RpcFallback));
                    }
                }
            } else if self
                .local
                .as_ref()
                .is_none_or(|local| local.generation != shared.generation)
            {
                self.local = None;
                return self
                    .query_ordered(kind, pattern, limit, order)
                    .map(|entries| (entries, SearchExecution::RpcFallback));
            }
            self.last_handshake = Some(std::time::Instant::now());
        }

        let local = self
            .local
            .as_mut()
            .expect("should_handshake is true whenever self.local is None");
        // Safety: this immutable mapping was validated when this generation
        // was installed above and remains owned by `local.view`.
        let arena = unsafe { scry_core::store::archived_bytes_validated(local.view.as_bytes()) };
        let query = match kind {
            QueryKind::Prefix => scry_core::Query::Prefix(pattern.to_owned()),
            QueryKind::Substring => scry_core::Query::Substring(pattern.to_owned()),
            QueryKind::Wildcard => scry_core::Query::wildcard(pattern),
            QueryKind::PathTerms => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs()
                    .min(u32::MAX as u64) as u32;
                scry_core::terms::parse_query(pattern, now)
                    .unwrap_or_else(|_| scry_core::Query::PathTerms(Vec::new()))
            }
            QueryKind::ShareIndex => unreachable!(),
            QueryKind::QueryStats => unreachable!(),
        };
        let options = SearchOptions::ordered(limit as usize, order);
        let entries =
            scry_core::view::search_archived_with_delta(arena, &local.delta, &query, options);
        Ok((entries, SearchExecution::SharedIndex))
    }
}
