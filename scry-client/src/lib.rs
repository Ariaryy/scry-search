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
