//! Identifiers and clocks.

use chrono::{SecondsFormat, Utc};
use std::sync::Mutex;
use ulid::Generator;

// `Generator::new` is const in ulid 3.0, so a static works; the generator itself is not Sync.
static GENERATOR: Mutex<Generator> = Mutex::new(Generator::new());

/// A new ULID. Monotonic within the process, so ids sort in creation order.
/// (`Ulid::generate()` alone is NOT monotonic inside one millisecond.)
pub fn new_id() -> String {
    let mut generator = GENERATOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match generator.generate() {
        Ok(id) => id.to_string(),
        // 80 random bits overflowed within one millisecond (practically never).
        Err(overflow) => overflow.commit_overflow_increment().to_string(),
    }
}

/// Current UTC time as RFC 3339 with milliseconds, e.g. `2026-10-13T09:44:23.391Z`.
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Largest counter that still fits the four hex digits of the HLC string format.
const MAX_HLC_COUNTER: u32 = 0xFFFF;

/// Hybrid logical clock. Strings sort by (milliseconds, counter); the device id breaks ties.
#[derive(Debug, Clone)]
pub struct Hlc {
    device: String,
    last_ms: i64,
    counter: u32,
}

impl Hlc {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            last_ms: 0,
            counter: 0,
        }
    }

    pub fn tick(&mut self) -> String {
        self.tick_at(Utc::now().timestamp_millis())
    }

    pub fn tick_at(&mut self, now_ms: i64) -> String {
        if now_ms > self.last_ms {
            self.last_ms = now_ms;
            self.counter = 0;
        } else if self.counter >= MAX_HLC_COUNTER {
            // A fifth hex digit would break string ordering, so borrow the next millisecond.
            self.last_ms += 1;
            self.counter = 0;
        } else {
            self.counter += 1;
        }
        format!("{:013}-{:04x}-{}", self.last_ms, self.counter, self.device)
    }

    /// Advance past a timestamp seen from another device.
    pub fn observe(&mut self, remote: &str) {
        if let Some((ms, counter, _)) = parse_hlc(remote)
            && (ms > self.last_ms || (ms == self.last_ms && counter > self.counter))
        {
            self.last_ms = ms;
            self.counter = counter;
        }
    }
}

pub fn parse_hlc(s: &str) -> Option<(i64, u32, String)> {
    let mut parts = s.splitn(3, '-');
    let ms = parts.next()?.parse().ok()?;
    let counter = u32::from_str_radix(parts.next()?, 16).ok()?;
    let device = parts.next()?.to_string();
    Some((ms, counter, device))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_26_char_and_sortable() {
        let a = new_id();
        let b = new_id();
        assert_eq!(a.len(), 26);
        assert!(b > a, "monotonic ids must sort in creation order: {a} {b}");
    }

    #[test]
    fn rfc3339_has_millis_and_z() {
        let t = now_rfc3339();
        assert!(t.ends_with('Z'), "{t}");
        assert_eq!(t.len(), "2026-10-13T09:44:23.391Z".len(), "{t}");
    }

    #[test]
    fn hlc_orders_within_same_millisecond() {
        let mut h = Hlc::new("dev1");
        let a = h.tick_at(1_000);
        let b = h.tick_at(1_000);
        let c = h.tick_at(999); // clock went backwards
        let d = h.tick_at(2_000);
        assert!(a < b && b < c && c < d, "{a} {b} {c} {d}");
        assert_eq!(a, "0000000001000-0000-dev1");
        assert_eq!(parse_hlc(&c), Some((1_000, 2, "dev1".to_string())));
    }

    #[test]
    fn hlc_observe_moves_forward() {
        let mut h = Hlc::new("a");
        h.observe("0000000005000-0003-b");
        let next = h.tick_at(10);
        assert_eq!(next, "0000000005000-0004-a");
    }

    #[test]
    fn hlc_counter_never_outgrows_four_hex_digits() {
        let mut h = Hlc::new("d");
        let mut prev = h.tick_at(1_000);
        for _ in 0..0x1_0000 {
            let next = h.tick_at(1_000);
            assert!(next > prev, "{prev} {next}");
            assert_eq!(next.len(), prev.len(), "{next}");
            prev = next;
        }
        // The counter is spent, so the clock borrows the next millisecond.
        assert_eq!(prev, "0000000001001-0000-d");

        let mut h = Hlc::new("a");
        h.observe("0000000005000-1ffff-b");
        assert_eq!(h.tick_at(10), "0000000005001-0000-a");
    }
}
