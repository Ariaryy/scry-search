//! SDK for talking to a running `scryd` daemon over its named pipe. This is
//! the whole "public API surface" other apps integrate against — the CLI is
//! just this crate with a formatter on top, and a future C ABI layer would
//! be a thin wrapper around the same `Client::query` call.

pub use scry_core::protocol::ResultEntry;
use scry_core::protocol::{decode_results, encode_request, QueryKind, Request};

pub struct Client {
    pipe: scry_ipc::Pipe,
}

impl Client {
    pub fn connect() -> anyhow::Result<Self> {
        Self::connect_to(scry_ipc::PIPE_NAME)
    }

    pub fn connect_to(pipe_name: &str) -> anyhow::Result<Self> {
        let pipe = scry_ipc::connect_client(pipe_name)
            .map_err(|e| anyhow::anyhow!("connecting to scryd at {pipe_name}: {e}"))?;
        Ok(Self { pipe })
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

    pub fn prefix(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::Prefix, pattern, limit)
    }

    pub fn substring(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::Substring, pattern, limit)
    }

    pub fn wildcard(&self, pattern: &str, limit: u32) -> anyhow::Result<Vec<ResultEntry>> {
        self.query(QueryKind::Wildcard, pattern, limit)
    }
}
