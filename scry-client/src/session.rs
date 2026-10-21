//! Latest-wins request submission and response draining for interactive clients.

use super::{Client, Order, QueryKind, ResultEntry};
use scry_core::protocol::{decode_results, encode_request, Request};

/// Long-lived, latest-wins query stream for interactive applications.
pub struct SearchSession {
    client: Client,
    limit: u32,
    order: Order,
}

impl Client {
    pub fn into_search_session(self, limit: u32) -> SearchSession {
        SearchSession {
            client: self,
            limit,
            order: Order::default(),
        }
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
}

impl SearchSession {
    /// Read diagnostics on this session after all outstanding replies have
    /// been drained. This is outside the measured query path.
    pub fn stats(&self) -> anyhow::Result<String> {
        anyhow::ensure!(
            !self.is_pending(),
            "drain pending searches before reading statistics"
        );
        self.client.stats()
    }
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

    pub fn set_limit(&mut self, limit: u32) {
        self.limit = limit;
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
