//! The guard around the judge call of a precise recall (Task 21 fix round 1): at most
//! [`MAX_IN_FLIGHT`] grading calls at once, and a breaker that stops asking a judge that keeps
//! failing, so a slow or unreachable judge costs a default recall nothing after the first few.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Grading calls of recalls in flight at once; a recall beyond that answers fast at once.
pub const MAX_IN_FLIGHT: usize = 4;
/// Consecutive failures that open the breaker.
pub const FAILURES_TO_OPEN: u32 = 3;
/// How long an open breaker keeps recalls away from the judge before one probes it.
pub const OPEN_FOR: Duration = Duration::from_secs(60);

pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Why a recall was not let through to the judge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The breaker is open (or a probe is out).
    Open,
    /// [`MAX_IN_FLIGHT`] calls are in flight.
    Busy,
}

#[derive(Default)]
struct Breaker {
    failures: u32,
    open_until: Option<Instant>,
    probing: bool,
}

pub struct PreciseGate {
    clock: Clock,
    breaker: Mutex<Breaker>,
    in_flight: Arc<AtomicUsize>,
}

/// Permission to make one grading call. Dropping it (when the call's thread ends) frees the slot.
pub struct Permit {
    in_flight: Arc<AtomicUsize>,
    probe: bool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Permit {
    /// True for the one call that tests an open breaker.
    pub fn is_probe(&self) -> bool {
        self.probe
    }
}

impl PreciseGate {
    pub fn new(clock: Option<Clock>) -> Self {
        Self {
            clock: clock.unwrap_or_else(|| Arc::new(Instant::now)),
            breaker: Mutex::new(Breaker::default()),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn admit(&self) -> Result<Permit, Refused> {
        let now = (self.clock)();
        let mut b = self.breaker.lock();
        let mut probe = false;
        if let Some(until) = b.open_until {
            if now < until || b.probing {
                return Err(Refused::Open);
            }
            probe = true;
        }
        // Counted under the breaker's lock, so a refused probe never leaves `probing` set.
        if self.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(Refused::Busy);
        }
        if probe {
            b.probing = true;
        }
        Ok(Permit {
            in_flight: self.in_flight.clone(),
            probe,
        })
    }

    /// The end of a call: `answered` when the judge gave an answer (including `Budget` and
    /// `Denied`, which are answers), false for a deadline or a failure.
    pub fn finish(&self, probe: bool, answered: bool) {
        let now = (self.clock)();
        let mut b = self.breaker.lock();
        if probe {
            b.probing = false;
        }
        if answered {
            *b = Breaker::default();
        } else {
            b.failures += 1;
            if b.failures >= FAILURES_TO_OPEN || probe {
                b.open_until = Some(now + OPEN_FOR);
            }
        }
    }
}
