//! The guard around the judge call of a precise recall: at most [`MAX_IN_FLIGHT`] grading calls
//! at once, and a breaker that stops asking a judge that keeps failing, so a slow or
//! unreachable judge costs a default recall nothing after the first few.
//!
//! The outcome of a call is recorded by its [`Permit`], which travels with the judge call: a
//! recall whose future is dropped (a client that disconnects) cannot leave the breaker half
//! way, because the call still ends and settles, and a permit dropped without settling counts
//! as a failure.

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

struct Inner {
    clock: Clock,
    breaker: Mutex<Breaker>,
    in_flight: AtomicUsize,
}

impl Inner {
    /// The end of a call. While the breaker is open only the probe's end changes it: a call
    /// admitted before it opened neither re-arms nor resets it when it ends late.
    fn record(&self, probe: bool, answered: bool) {
        let now = (self.clock)();
        let mut b = self.breaker.lock();
        if probe {
            if answered {
                *b = Breaker::default();
            } else {
                b.probing = false;
                b.open_until = Some(now + OPEN_FOR);
            }
        } else if b.open_until.is_none() {
            if answered {
                b.failures = 0;
            } else {
                b.failures += 1;
                if b.failures >= FAILURES_TO_OPEN {
                    b.open_until = Some(now + OPEN_FOR);
                }
            }
        }
    }
}

pub struct PreciseGate {
    inner: Arc<Inner>,
}

/// Permission to make one grading call; it goes with the call. [`settle`](Self::settle) it with
/// the outcome; dropping it unsettled counts a failure. Dropping it frees the slot.
pub struct Permit {
    inner: Arc<Inner>,
    probe: bool,
    settled: bool,
}

impl Permit {
    /// True for the one call that tests an open breaker.
    pub fn is_probe(&self) -> bool {
        self.probe
    }

    /// Record the outcome: `answered` when the judge gave an answer (`Budget` and `Denied` are
    /// answers), false for a deadline or a failure.
    pub fn settle(&mut self, answered: bool) {
        if !self.settled {
            self.settled = true;
            self.inner.record(self.probe, answered);
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.settle(false);
        self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PreciseGate {
    pub fn new(clock: Option<Clock>) -> Self {
        Self {
            inner: Arc::new(Inner {
                clock: clock.unwrap_or_else(|| Arc::new(Instant::now)),
                breaker: Mutex::new(Breaker::default()),
                in_flight: AtomicUsize::new(0),
            }),
        }
    }

    pub fn admit(&self) -> Result<Permit, Refused> {
        let now = (self.inner.clock)();
        let mut b = self.inner.breaker.lock();
        let mut probe = false;
        if let Some(until) = b.open_until {
            if now < until || b.probing {
                return Err(Refused::Open);
            }
            probe = true;
        }
        // Counted under the breaker's lock, so a refused probe never leaves `probing` set.
        if self.inner.in_flight.fetch_add(1, Ordering::SeqCst) >= MAX_IN_FLIGHT {
            self.inner.in_flight.fetch_sub(1, Ordering::SeqCst);
            return Err(Refused::Busy);
        }
        if probe {
            b.probing = true;
        }
        Ok(Permit {
            inner: self.inner.clone(),
            probe,
            settled: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> (PreciseGate, Arc<Mutex<Duration>>) {
        let base = Instant::now();
        let off = Arc::new(Mutex::new(Duration::ZERO));
        let o = off.clone();
        (
            PreciseGate::new(Some(Arc::new(move || base + *o.lock()))),
            off,
        )
    }

    fn open(g: &PreciseGate) {
        for _ in 0..FAILURES_TO_OPEN {
            g.admit().unwrap().settle(false);
        }
        assert_eq!(g.admit().err(), Some(Refused::Open));
    }

    /// R-1: a probe that is dropped without an outcome counts as a failed probe; the next
    /// cool-down ends in another probe, which can close the breaker.
    #[test]
    fn a_probe_dropped_without_an_outcome_does_not_leave_the_breaker_stuck() {
        let (g, off) = gate();
        open(&g);
        *off.lock() = Duration::from_secs(61);
        let probe = g.admit().unwrap();
        assert!(probe.is_probe());
        assert_eq!(g.admit().err(), Some(Refused::Open), "one probe at a time");
        drop(probe);
        assert_eq!(
            g.admit().err(),
            Some(Refused::Open),
            "re-opened for a minute"
        );
        *off.lock() = Duration::from_secs(125);
        let mut again = g.admit().unwrap();
        assert!(again.is_probe());
        again.settle(true);
        assert!(!g.admit().unwrap().is_probe(), "closed");
    }

    /// R-3: a call admitted before the breaker opened changes nothing when it ends late.
    #[test]
    fn a_late_call_does_not_rearm_or_reset_an_open_breaker() {
        let (g, off) = gate();
        let mut late_ok = g.admit().unwrap();
        let mut late_bad = g.admit().unwrap();
        open(&g);
        *off.lock() = Duration::from_secs(30);
        late_bad.settle(false);
        late_ok.settle(true);
        assert_eq!(g.admit().err(), Some(Refused::Open), "still open");
        // Not re-armed from t = 30 s: the cool-down still ends at 60 s.
        *off.lock() = Duration::from_secs(61);
        assert!(g.admit().unwrap().is_probe());
    }
}
