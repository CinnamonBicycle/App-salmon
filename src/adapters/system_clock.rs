use chrono::{DateTime, Utc};

use crate::ports::clock::Clock;

/// Real [`Clock`] implementation backed by the system wall clock. Zero-sized; the only production
/// implementation of `Clock` (tests use `FakeClock` instead, for deterministic TTL-expiry tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    /// Returns the current system time.
    ///
    /// # Returns
    ///
    /// The current UTC time, as reported by [`chrono::Utc::now`].
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(test)]
mod tests {
    use super::SystemClock;
    use crate::ports::clock::Clock;
    use chrono::Utc;

    #[test]
    fn now_is_close_to_real_time() {
        let before = Utc::now();
        let reported = SystemClock.now();
        let after = Utc::now();
        assert!(reported >= before && reported <= after);
    }
}
