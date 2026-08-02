//! Process-wide index-read governor shared by every indexed volume.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const DEFAULT_BYTES_PER_SECOND: u64 = 128 * 1024 * 1024;

#[derive(Clone, Copy)]
struct State {
    bytes_per_second: u64,
    next: Instant,
}

impl State {
    fn new(bytes_per_second: u64) -> Self {
        Self {
            bytes_per_second,
            next: Instant::now(),
        }
    }

    fn reserve(&mut self, bytes: usize, now: Instant) -> Duration {
        if self.bytes_per_second == 0 || bytes == 0 {
            return Duration::ZERO;
        }

        let start = self.next.max(now);
        let nanos = (bytes as u128)
            .saturating_mul(Duration::from_secs(1).as_nanos())
            .div_ceil(self.bytes_per_second as u128);
        self.next = start + Duration::from_nanos(nanos.min(u64::MAX as u128) as u64);
        start.saturating_duration_since(now)
    }
}

static INDEX_READ_LIMIT: OnceLock<Mutex<State>> = OnceLock::new();

/// Configures the aggregate read cap for all indexers in this process.
/// A value of zero disables throttling.
pub fn configure(bytes_per_second: u64) {
    let state = INDEX_READ_LIMIT.get_or_init(|| Mutex::new(State::new(bytes_per_second)));
    *state.lock().expect("index read throttle poisoned") = State::new(bytes_per_second);
}

/// Waits until `bytes` can be read under the process-wide cap.
pub fn acquire(bytes: usize) {
    let delay = {
        let state =
            INDEX_READ_LIMIT.get_or_init(|| Mutex::new(State::new(DEFAULT_BYTES_PER_SECOND)));
        state
            .lock()
            .expect("index read throttle poisoned")
            .reserve(bytes, Instant::now())
    };
    if !delay.is_zero() {
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let mut state = State::new(0);
        assert!(state.reserve(1024, Instant::now()).is_zero());
    }

    #[test]
    fn reservations_are_aggregate() {
        let now = Instant::now();
        let mut state = State::new(100);
        assert!(state.reserve(100, now).is_zero());
        assert_eq!(state.reserve(100, now), Duration::from_secs(1));
    }
}
