//! SDK for talking to a running `scryd` daemon over its named pipe. This is
//! the whole "public API surface" other apps integrate against — the CLI is
//! just this crate with a formatter on top, and a future C ABI layer would
//! be a thin wrapper around the same `Client::query` call.

use scry_core::protocol::{decode_results, decode_shared_index, encode_request, Request};
pub use scry_core::protocol::{Order, QueryKind, ResultEntry};
use scry_core::view::SearchOptions;

struct LocalIndex {
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
const LOCAL_HANDSHAKE_STALENESS_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);

pub struct Client {
    pipe: scry_ipc::Pipe,
    pipe_name: String,
    local: Option<LocalIndex>,
    /// When the last `ShareIndex` handshake completed, for rate-limiting
    /// against [`LOCAL_HANDSHAKE_STALENESS_WINDOW`].
    last_handshake: Option<std::time::Instant>,
    /// Requests written by `search_interactive` that haven't had their
    /// response frame read yet. Framing guarantees exactly one response per
    /// request, in send order, so draining this many frames always lands on
    /// the response to the request just sent — anything read before that is
    /// a stale answer to an earlier, since-superseded keystroke.
    pending_interactive: u64,
}

/// Long-lived, latest-wins query stream for interactive applications.
pub struct SearchSession {
    client: Client,
    limit: u32,
    order: Order,
}

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

    pub fn connect() -> anyhow::Result<Self> {
        Self::connect_to(scry_ipc::PIPE_NAME)
    }

    pub fn connect_to(pipe_name: &str) -> anyhow::Result<Self> {
        let pipe = scry_ipc::connect_client(pipe_name)
            .map_err(|e| anyhow::anyhow!("connecting to scryd at {pipe_name}: {e}"))?;
        Ok(Self {
            pipe,
            pipe_name: pipe_name.to_owned(),
            local: None,
            last_handshake: None,
            pending_interactive: 0,
        })
    }

    pub fn into_search_session(self, limit: u32) -> SearchSession {
        SearchSession {
            client: self,
            limit,
            order: Order::default(),
        }
    }

    pub fn query(
        &self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        self.query_ordered(kind, pattern, limit, Order::default())
    }

    /// As [`Self::query`], but with an explicit result ordering.
    pub fn query_ordered(
        &self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
        order: Order,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        let req = Request {
            kind,
            pattern: pattern.to_string(),
            limit,
            order,
        };
        self.pipe.write_frame(&encode_request(&req))?;
        let resp = self.pipe.read_frame()?;
        decode_results(&resp).ok_or_else(|| anyhow::anyhow!("malformed response from scryd"))
    }

    /// Read daemon-side query timings and process memory counters.
    pub fn stats(&self) -> anyhow::Result<String> {
        let req = Request {
            kind: QueryKind::QueryStats,
            pattern: String::new(),
            limit: 0,
            order: Order::default(),
        };
        self.pipe.write_frame(&encode_request(&req))?;
        String::from_utf8(self.pipe.read_frame()?)
            .map_err(|_| anyhow::anyhow!("malformed statistics response from scryd"))
    }

    /// Blocking convenience call for an interactive query. Use
    /// [`SearchSession`] when input and result rendering must stay responsive.
    pub fn search_interactive(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        self.send_interactive(kind, pattern, limit)?;
        self.recv_interactive()
    }

    /// Writes an as-you-type request without waiting for its response.
    pub fn send_interactive(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<()> {
        self.send_interactive_ordered(kind, pattern, limit, Order::default())
    }

    pub fn send_interactive_ordered(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
        order: Order,
    ) -> anyhow::Result<()> {
        let req = Request {
            kind,
            pattern: pattern.to_string(),
            limit,
            order,
        };
        self.pipe.write_frame(&encode_request(&req))?;
        self.pending_interactive += 1;
        Ok(())
    }

    /// Blocks until the response to the most recent `send_interactive` call
    /// arrives, discarding any older buffered responses along the way.
    /// Panics if called with no outstanding `send_interactive` request —
    /// that's a caller bug, not a runtime condition to recover from.
    pub fn recv_interactive(&mut self) -> anyhow::Result<Vec<ResultEntry>> {
        assert!(
            self.pending_interactive > 0,
            "recv_interactive called with no outstanding send_interactive request"
        );
        let mut latest = None;
        while self.pending_interactive > 0 {
            latest = Some(self.pipe.read_frame()?);
            self.pending_interactive -= 1;
        }
        let resp = latest.expect("pending_interactive was > 0 above");
        decode_results(&resp).ok_or_else(|| anyhow::anyhow!("malformed response from scryd"))
    }

    /// Reads completed interactive replies without waiting for an in-flight
    /// query. Older replies are discarded; only the newest may be returned.
    pub fn try_recv_interactive(&mut self) -> anyhow::Result<Option<Vec<ResultEntry>>> {
        while self.pending_interactive > 0 && self.pipe.pending_bytes()? > 0 {
            let response = self.pipe.read_frame()?;
            self.pending_interactive -= 1;
            if self.pending_interactive == 0 {
                return decode_results(&response)
                    .map(Some)
                    .ok_or_else(|| anyhow::anyhow!("malformed response from scryd"));
            }
        }
        Ok(None)
    }

    pub fn has_pending_interactive(&self) -> bool {
        self.pending_interactive > 0
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
                    return self.query_ordered(kind, pattern, limit, order);
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
                        return self.query_ordered(kind, pattern, limit, order);
                    }
                }
            } else if self
                .local
                .as_ref()
                .is_none_or(|local| local.generation != shared.generation)
            {
                self.local = None;
                return self.query_ordered(kind, pattern, limit, order);
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
            QueryKind::PathTerms => scry_core::Query::PathTerms(
                scry_core::terms::parse_terms(pattern).unwrap_or_default(),
            ),
            QueryKind::ShareIndex => unreachable!(),
            QueryKind::QueryStats => unreachable!(),
        };
        let options = SearchOptions::ordered(limit as usize, order);
        if let scry_core::Query::PathTerms(terms) = &query {
            let hits = scry_core::view::search_path_terms(arena, &local.delta, terms, options);
            return Ok(scry_core::view::materialize_hits(
                arena,
                &local.delta,
                &hits,
            ));
        }
        Ok(scry_core::view::search_archived_with_delta(
            arena,
            &local.delta,
            &query,
            options,
        ))
    }

    pub fn prefix(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::Prefix, pattern, limit)
    }

    pub fn substring(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::Substring, pattern, limit)
    }

    pub fn wildcard(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::Wildcard, pattern, limit)
    }

    pub fn path_terms(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::PathTerms, pattern, limit)
    }
}

impl SearchSession {
    pub fn connect(limit: u32) -> anyhow::Result<Self> {
        Ok(Client::connect()?.into_search_session(limit))
    }

    pub fn submit(&mut self, kind: QueryKind, pattern: &str) -> anyhow::Result<()> {
        self.client
            .send_interactive_ordered(kind, pattern, self.limit, self.order)
    }

    pub fn set_order(&mut self, order: Order) {
        self.order = order;
    }

    pub fn poll_latest(&mut self) -> anyhow::Result<Option<Vec<ResultEntry>>> {
        self.client.try_recv_interactive()
    }

    pub fn wait_latest(&mut self) -> anyhow::Result<Vec<ResultEntry>> {
        self.client.recv_interactive()
    }

    pub fn is_pending(&self) -> bool {
        self.client.has_pending_interactive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scry_core::protocol::{
        decode_request, encode_results, encode_shared_index, SharedIndexResponse,
    };

    fn unique_pipe_name() -> String {
        format!(
            r"\\.\pipe\scry-client-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn connect(pipe_name: &str) -> Client {
        (0..100)
            .find_map(|_| {
                Client::connect_to(pipe_name).ok().or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    None
                })
            })
            .expect("test pipe did not become ready")
    }

    fn entry(name: &str) -> ResultEntry {
        ResultEntry {
            path: name.to_string(),
            size: 0,
            mtime: 0,
            is_dir: false,
        }
    }

    /// A response already sitting in the pipe when `search_interactive` is
    /// called — the answer to a keystroke the caller has since moved past —
    /// must be discarded in favor of the response to the request this call
    /// itself just wrote, never returned to the caller.
    #[test]
    fn search_interactive_discards_a_stale_buffered_response() {
        let pipe_name = unique_pipe_name();
        let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
        let thread = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();
            // Respond to both requests only after both have arrived, so the
            // client's write of the second request races ahead of it reading
            // the first response — exactly the scenario this method exists for.
            let _first = pipe.read_frame().unwrap();
            let _second = pipe.read_frame().unwrap();
            pipe.write_frame(&encode_results(&[entry("stale")]))
                .unwrap();
            pipe.write_frame(&encode_results(&[entry("fresh")]))
                .unwrap();
        });

        let mut client = connect(&pipe_name);
        // Simulate an earlier `search_interactive` call whose request was
        // written but whose response hasn't been read yet.
        client
            .pipe
            .write_frame(&encode_request(&Request {
                kind: QueryKind::Substring,
                pattern: "stale-query".to_string(),
                limit: 50,
                order: Order::default(),
            }))
            .unwrap();
        client.pending_interactive = 1;

        let result = client
            .search_interactive(QueryKind::Substring, "fresh-query", 50)
            .unwrap();
        assert_eq!(result, vec![entry("fresh")]);

        drop(client);
        thread.join().unwrap();
    }

    #[test]
    fn interactive_poll_never_waits_for_an_inflight_query() {
        let pipe_name = unique_pipe_name();
        let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();
            let _request = pipe.read_frame().unwrap();
            released.recv().unwrap();
            pipe.write_frame(&encode_results(&[entry("ready")]))
                .unwrap();
        });

        let mut session = connect(&pipe_name).into_search_session(50);
        session.submit(QueryKind::Substring, "query").unwrap();
        assert!(session.poll_latest().unwrap().is_none());
        assert!(session.is_pending());
        release.send(()).unwrap();

        let result = (0..100)
            .find_map(|_| {
                let result = session.poll_latest().unwrap();
                if result.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                result
            })
            .expect("response did not become readable");
        assert_eq!(result, vec![entry("ready")]);
        assert!(!session.is_pending());
        thread.join().unwrap();
    }

    /// Builds an in-memory, self-contained rkyv-archived arena and a real
    /// section mapping it, duplicated to this process — exactly what a real
    /// daemon hands back in a `ShareIndex` response, minus the file on disk.
    fn shared_index_response(generation: u64) -> SharedIndexResponse {
        let mut b = scry_core::Arena::builder();
        let root = b.push("C:", 0, true);
        let child = b.push("file.txt", 0, false);
        b.set_parent(child, root);
        let arena = b.build().0;
        let bytes = scry_core::store::to_bytes(&arena).unwrap();
        let section = scry_ipc::Section::create(&bytes).unwrap();
        // The duplicated handle is independent of `section`'s own handle
        // (closed when `section` drops at the end of this scope), exactly
        // as it would be once handed to a separate client process.
        let handle = section.duplicate_for(std::process::id()).unwrap();
        SharedIndexResponse {
            handle,
            len: bytes.len() as u64,
            generation,
            overlay: scry_core::delta::Delta::new(arena.len()).encode_query_overlay(),
        }
    }

    /// The whole point of `search_local_ordered` answering in-process is
    /// defeated if it re-asks the daemon for a shared mapping on every call.
    /// Once a mapping is established, repeated calls inside the staleness
    /// window must reuse it rather than sending another `ShareIndex` frame.
    #[test]
    fn local_search_reuses_mapping_within_staleness_window() {
        let pipe_name = unique_pipe_name();
        let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
        let thread = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();
            let mut share_index_requests = 0u32;
            // The client closes the pipe on drop; that ends this loop.
            while let Ok(request) = pipe.read_frame() {
                assert_eq!(
                    decode_request(&request).unwrap().kind,
                    QueryKind::ShareIndex
                );
                share_index_requests += 1;
                pipe.write_frame(&encode_shared_index(&shared_index_response(1)))
                    .unwrap();
            }
            share_index_requests
        });

        let mut client = connect(&pipe_name);
        for _ in 0..5 {
            let result = client
                .search_local(QueryKind::Substring, "file", 50)
                .unwrap();
            assert_eq!(result.len(), 1);
        }
        drop(client);

        let share_index_requests = thread.join().unwrap();
        assert_eq!(
            share_index_requests, 1,
            "repeated calls within the staleness window must not re-handshake"
        );
    }

    /// When the daemon can't or won't share a mapping, `search_local` must
    /// still answer — over a fresh RPC connection — rather than erroring out.
    #[test]
    fn local_search_falls_back_to_rpc_when_sharing_unavailable() {
        let pipe_name = unique_pipe_name();
        let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
        let thread = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();
            let request = pipe.read_frame().unwrap();
            assert_eq!(
                decode_request(&request).unwrap().kind,
                QueryKind::ShareIndex
            );
            // Sharing unavailable: not a valid `SharedIndexResponse` frame.
            pipe.write_frame(b"not a shared index").unwrap();
            drop(pipe);

            // The client reconnects to retry over plain RPC.
            let pipe = server.accept().unwrap();
            let request = pipe.read_frame().unwrap();
            let decoded = decode_request(&request).unwrap();
            assert_eq!(decoded.kind, QueryKind::Substring);
            pipe.write_frame(&encode_results(&[entry("file.txt")]))
                .unwrap();
        });

        let mut client = connect(&pipe_name);
        let result = client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap();
        assert_eq!(result, vec![entry("file.txt")]);

        drop(client);
        thread.join().unwrap();
    }

    /// A rejected replacement is evidence that the cached generation is no
    /// longer current. The fallback answer is safe, but retaining the old
    /// mapping or refreshing the handshake timer would make subsequent calls
    /// knowingly serve that stale generation.
    #[test]
    fn failed_replacement_mapping_discards_old_generation_and_keeps_retry_due() {
        let pipe_name = unique_pipe_name();
        let server = scry_ipc::PipeServer::new(&pipe_name).unwrap();
        let thread = std::thread::spawn(move || {
            let pipe = server.accept().unwrap();

            let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
            assert_eq!(request.kind, QueryKind::ShareIndex);
            pipe.write_frame(&encode_shared_index(&shared_index_response(1)))
                .unwrap();

            let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
            assert_eq!(request.kind, QueryKind::ShareIndex);
            pipe.write_frame(&encode_shared_index(&SharedIndexResponse {
                handle: u64::MAX,
                len: 4096,
                generation: 2,
                overlay: Vec::new(),
            }))
            .unwrap();

            let request = decode_request(&pipe.read_frame().unwrap()).unwrap();
            assert_eq!(request.kind, QueryKind::Substring);
            pipe.write_frame(&encode_results(&[entry("rpc-result")]))
                .unwrap();
        });

        let mut client = connect(&pipe_name);
        assert_eq!(
            client
                .search_local(QueryKind::Substring, "file", 50)
                .unwrap()
                .len(),
            1
        );
        client.last_handshake = Some(std::time::Instant::now() - LOCAL_HANDSHAKE_STALENESS_WINDOW);

        let result = client
            .search_local(QueryKind::Substring, "file", 50)
            .unwrap();
        assert_eq!(result, vec![entry("rpc-result")]);
        assert!(client.local.is_none());
        assert!(client.last_handshake.unwrap().elapsed() >= LOCAL_HANDSHAKE_STALENESS_WINDOW);

        drop(client);
        thread.join().unwrap();
    }
}
