//! SDK for talking to a running `scryd` daemon over its named pipe. This is
//! the whole "public API surface" other apps integrate against — the CLI is
//! just this crate with a formatter on top, and a future C ABI layer would
//! be a thin wrapper around the same `Client::query` call.

use scry_core::protocol::{decode_results, encode_request, Request};
pub use scry_core::protocol::{DaemonStatus, Order, QueryKind, ResultEntry};
mod local;
mod session;

use local::LocalIndex;
pub use session::SearchSession;

/// The daemon recognizes discovery but cannot serve the requested schema.
#[derive(Debug)]
pub struct UnsupportedHandshake;

impl std::fmt::Display for UnsupportedHandshake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("daemon does not support status schema 1")
    }
}
impl std::error::Error for UnsupportedHandshake {}

pub struct Client {
    pipe: scry_ipc::Pipe,
    pipe_name: String,
    local: Option<LocalIndex>,
    /// When the last `ShareIndex` handshake completed, for rate-limiting
    /// against the local handshake staleness window.
    last_handshake: Option<std::time::Instant>,
    /// Requests written by `search_interactive` that haven't had their
    /// response frame read yet. Framing guarantees exactly one response per
    /// request, in send order, so draining this many frames always lands on
    /// the response to the request just sent — anything read before that is
    /// a stale answer to an earlier, since-superseded keystroke.
    pending_interactive: u64,
}

/// Where a local-first search actually executed, including automatic fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchExecution {
    SharedIndex,
    RpcFallback,
}

impl Client {
    /// The PID reported by Windows for the server end of this connection.
    pub fn server_process_id(&self) -> anyhow::Result<u32> {
        Ok(self.pipe.server_process_id()?)
    }
    /// Probe on a separate connection so a legacy daemon ignoring kind 6 cannot
    /// desynchronize an existing search session. Unsupported discovery times out;
    /// callers must distinguish that from a successful incompatible response.
    pub fn daemon_status(timeout: std::time::Duration) -> anyhow::Result<DaemonStatus> {
        Self::daemon_status_with_server(timeout).map(|(status, _)| status)
    }

    /// Returns the pipe-authenticated server PID alongside its status.
    pub fn daemon_status_with_server(
        timeout: std::time::Duration,
    ) -> anyhow::Result<(DaemonStatus, u32)> {
        let pipe = scry_ipc::connect_client(scry_ipc::PIPE_NAME)?;
        let pid = pipe.server_process_id()?;
        Ok((Self::read_daemon_status(&pipe, timeout)?, pid))
    }

    /// Discovery for an explicitly selected pipe, with the same bounded contract.
    pub fn daemon_status_at(
        pipe_name: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<DaemonStatus> {
        let pipe = scry_ipc::connect_client(pipe_name)?;
        Self::read_daemon_status(&pipe, timeout)
    }

    fn read_daemon_status(
        pipe: &scry_ipc::Pipe,
        timeout: std::time::Duration,
    ) -> anyhow::Result<DaemonStatus> {
        pipe.write_frame(&encode_request(&Request {
            kind: QueryKind::Status,
            pattern: "SCRYSTA1".to_owned(),
            limit: 0,
            order: Order::default(),
        }))?;
        let bytes = pipe.read_frame_timeout(timeout, 85)?;
        if bytes == b"SCRYUNS1" {
            return Err(UnsupportedHandshake.into());
        }
        scry_core::protocol::decode_status(&bytes)
            .ok_or_else(|| anyhow::anyhow!("invalid or unsupported daemon status response"))
    }

    /// Connect only when the daemon supports this client's RPC protocol.
    /// Discovery and subsequent searches use the same server connection.
    pub fn connect_compatible(timeout: std::time::Duration) -> anyhow::Result<Self> {
        let client = Self::connect()?;
        let status = Self::read_daemon_status(&client.pipe, timeout)?;
        anyhow::ensure!(
            status.rpc_compatible(),
            "incompatible daemon IPC version {}",
            status.ipc_version
        );
        Ok(client)
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
mod tests;
