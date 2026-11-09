//! Test support shared with the application: a loopback mock exchange. Compiled only for this
//! crate's tests and under the `testing` feature.

pub mod conformance;
pub mod mock;

use std::sync::atomic::{AtomicI64, Ordering};

use crate::adapter::{Clock, SystemClock};

/// A clock that runs in real time but can be pushed forward by hand: a test makes the machine
/// "sleep" by calling [`ManualClock::advance`], without waiting.
#[derive(Default)]
pub struct ManualClock {
    offset_ms: AtomicI64,
}

impl ManualClock {
    pub fn new(_start_ms: i64) -> Self {
        Self::default()
    }

    /// Jumps forward `ms` at once.
    pub fn advance(&self, ms: i64) {
        self.offset_ms.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        SystemClock.now_ms() + self.offset_ms.load(Ordering::SeqCst)
    }
}
