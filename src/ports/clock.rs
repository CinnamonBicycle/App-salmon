//! Time as a trait, so TTL-expiry logic is testable by advancing a fake clock instead of
//! sleeping in tests.

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    /// The current time, as observed by this clock.
    ///
    /// # Returns
    ///
    /// The current UTC timestamp. For the real implementation this is wall-clock time; a fake
    /// implementation may return a fixed or manually-advanced value instead.
    fn now(&self) -> DateTime<Utc>;
}

#[cfg(test)]
pub struct FakeClock {
    now: std::sync::Mutex<DateTime<Utc>>,
}

#[cfg(test)]
impl FakeClock {
    #[must_use]
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: std::sync::Mutex::new(start),
        }
    }

    pub fn advance(&self, delta: chrono::TimeDelta) {
        let mut now = self.now.lock().expect("lock");
        *now += delta;
    }
}

#[cfg(test)]
impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().expect("lock")
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, FakeClock};
    use chrono::{TimeDelta, Utc};

    #[test]
    fn now_returns_the_configured_start_time() {
        let start = Utc::now();
        let clock = FakeClock::new(start);
        assert_eq!(clock.now(), start);
    }

    #[test]
    fn advance_moves_time_forward() {
        let start = Utc::now();
        let clock = FakeClock::new(start);
        clock.advance(TimeDelta::seconds(30));
        assert_eq!(clock.now(), start + TimeDelta::seconds(30));
    }
}
