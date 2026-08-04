//! SDK for talking to a running `scryd` daemon over its named pipe. This is
//! the whole "public API surface" other apps integrate against — the CLI is
//! just this crate with a formatter on top, and a future C ABI layer would
//! be a thin wrapper around the same `Client::query` call.

use scry_core::protocol::{decode_results, decode_shared_index, encode_request, Request};
pub use scry_core::protocol::{QueryKind, ResultEntry};

struct LocalIndex {
    view: scry_ipc::SectionView,
    delta: scry_core::delta::Delta,
    path_index: Option<scry_core::pathindex::PathIndex>,
    generation: u64,
}

pub struct Client {
    pipe: scry_ipc::Pipe,
    pipe_name: String,
    local: Option<LocalIndex>,
    /// Requests written by `search_interactive` that haven't had their
    /// response frame read yet. Framing guarantees exactly one response per
    /// request, in send order, so draining this many frames always lands on
    /// the response to the request just sent — anything read before that is
    /// a stale answer to an earlier, since-superseded keystroke.
    pending_interactive: u64,
}

impl Client {
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
            pending_interactive: 0,
        })
    }

    pub fn query(
        &self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        let req = Request {
            kind,
            pattern: pattern.to_string(),
            limit,
        };
        self.pipe.write_frame(&encode_request(&req))?;
        let resp = self.pipe.read_frame()?;
        decode_results(&resp).ok_or_else(|| anyhow::anyhow!("malformed response from scryd"))
    }

    /// As-you-type querying over a single pipelined connection: writes the
    /// request immediately (even if an earlier call's response hasn't been
    /// read yet), then reads and discards every response older than this
    /// one before returning it. A caller that fires a request per keystroke
    /// without waiting for each response therefore always sees the answer to
    /// its latest keystroke, never a stale one — at the cost of a stale
    /// (possibly empty) result briefly reaching the caller for the discarded
    /// requests, since each still yields exactly one response frame.
    pub fn search_interactive(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        self.send_interactive(kind, pattern, limit)?;
        self.recv_interactive()
    }

    /// Writes an as-you-type request without blocking for its response —
    /// see `search_interactive`. Split out so a caller can echo the typed
    /// pattern immediately and only block on `recv_interactive` once it has
    /// nothing left to send, instead of round-tripping per keystroke.
    pub fn send_interactive(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<()> {
        let req = Request {
            kind,
            pattern: pattern.to_string(),
            limit,
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

    /// Search a coherent base+delta generation in this process, falling back
    /// to daemon RPC when section sharing is unavailable.
    pub fn search_local(
        &mut self,
        kind: QueryKind,
        pattern: &str,
        limit: u32,
    ) -> anyhow::Result<Vec<ResultEntry>> {
        if kind == QueryKind::ShareIndex {
            return Err(anyhow::anyhow!("invalid search kind"));
        }
        let request = Request {
            kind: QueryKind::ShareIndex,
            pattern: self
                .local
                .as_ref()
                .map(|local| local.generation.to_string())
                .unwrap_or_default(),
            limit: 0,
        };
        let shared = (|| -> anyhow::Result<_> {
            self.pipe.write_frame(&encode_request(&request))?;
            let frame = self.pipe.read_frame()?;
            decode_shared_index(&frame).ok_or_else(|| anyhow::anyhow!("sharing unsupported"))
        })();
        let shared = match shared {
            Ok(shared) => shared,
            Err(_) => {
                self.pipe = scry_ipc::connect_client(&self.pipe_name)?;
                return self.query(kind, pattern, limit);
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
                    path_index: None,
                    generation: shared.generation,
                })
            })();
            match mapped {
                Ok(local) => self.local = Some(local),
                Err(_) => return self.query(kind, pattern, limit),
            }
        } else if self
            .local
            .as_ref()
            .is_none_or(|local| local.generation != shared.generation)
        {
            return self.query(kind, pattern, limit);
        }

        let local = self.local.as_mut().unwrap();
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
        };
        if let scry_core::Query::PathTerms(terms) = &query {
            let path_index = local
                .path_index
                .get_or_insert_with(|| scry_core::pathindex::PathIndex::build(arena, &local.delta));
            return Ok(scry_core::view::search_path_terms(
                arena,
                &local.delta,
                path_index,
                terms,
                limit as usize,
            ));
        }
        Ok(scry_core::view::search_archived_with_delta(
            arena,
            &local.delta,
            &query,
            limit as usize,
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

#[cfg(test)]
mod tests {
    use super::*;
    use scry_core::protocol::encode_results;

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
}
