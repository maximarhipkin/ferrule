//! Wall-clock time, swappable so tests can cross midnight.

use chrono::{DateTime, Utc};
use std::sync::Mutex;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that only moves when told to.
pub struct FakeClock(Mutex<DateTime<Utc>>);

impl FakeClock {
    pub fn at(rfc3339: &str) -> Self {
        let t = DateTime::parse_from_rfc3339(rfc3339)
            .expect("an RFC 3339 time")
            .with_timezone(&Utc);
        Self(Mutex::new(t))
    }

    pub fn set(&self, rfc3339: &str) {
        *self.0.lock().unwrap() = DateTime::parse_from_rfc3339(rfc3339)
            .expect("an RFC 3339 time")
            .with_timezone(&Utc);
    }

    pub fn advance(&self, by: chrono::Duration) {
        *self.0.lock().unwrap() += by;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}
