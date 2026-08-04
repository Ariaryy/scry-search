//! Cooperative cancellation for the O(n) scan paths in `query.rs` and
//! `view.rs`'s path-term matcher.
//!
//! The daemon uses "a newer request arrived on this connection" as the
//! cancel signal (see `scry-daemon`), so this is a generation check, not a
//! boolean flag: a query started against generation 3 must keep scanning if
//! it is re-checked while the connection is still on generation 3, and stop
//! the moment the connection has moved to generation 4, regardless of how
//! many more requests arrive after that.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub struct Cancellation<'a> {
    generation: &'a AtomicU64,
    expected: u64,
}

impl<'a> Cancellation<'a> {
    pub fn new(generation: &'a AtomicU64, expected: u64) -> Self {
        Self {
            generation,
            expected,
        }
    }

    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.generation.load(Ordering::Relaxed) != self.expected
    }
}

/// How often the O(n) scan loops re-check cancellation. Checking every
/// record would add an atomic load to the hottest loop in the crate; checking
/// this rarely still bounds the worst-case overrun to a few hundred
/// microseconds even at the measured million-record corpus.
pub const CHECK_INTERVAL: u32 = 1 << 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_trips_only_after_the_generation_moves() {
        let generation = AtomicU64::new(3);
        let cancel = Cancellation::new(&generation, 3);
        assert!(!cancel.is_cancelled());
        generation.store(4, Ordering::Relaxed);
        assert!(cancel.is_cancelled());
    }
}
