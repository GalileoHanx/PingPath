//! Token buckets for per-session packet and byte limits.

use std::time::Instant;

/// Allows `rate` units per second with bursts of up to `burst` units.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(rate: u64, burst: u64) -> Self {
        Self::new_at(rate, burst, Instant::now())
    }

    pub fn new_at(rate: u64, burst: u64, now: Instant) -> Self {
        let burst = burst.max(1) as f64;
        Self {
            rate: rate as f64,
            burst,
            tokens: burst,
            last: now,
        }
    }

    /// Takes `cost` units if available.
    pub fn take(&mut self, cost: u64) -> bool {
        self.take_at(cost, Instant::now())
    }

    pub fn take_at(&mut self, cost: u64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        let cost = cost as f64;
        if self.tokens >= cost {
            self.tokens -= cost;
            true
        } else {
            false
        }
    }
}

/// A session's limits in one direction: packets and bytes per second, with
/// one second of burst.
#[derive(Debug, Clone)]
pub struct Limiter {
    packets: TokenBucket,
    bytes: TokenBucket,
}

impl Limiter {
    pub fn new(packets_per_second: u64, bits_per_second: u64) -> Self {
        let bytes_per_second = bits_per_second / 8;
        Self {
            packets: TokenBucket::new(packets_per_second, packets_per_second),
            bytes: TokenBucket::new(bytes_per_second, bytes_per_second),
        }
    }

    /// Whether a packet of `len` bytes is within the limits (and accounts
    /// for it if so).
    pub fn allow(&mut self, len: usize) -> bool {
        let now = Instant::now();
        // Check both before taking from either, so a refused packet costs nothing.
        let mut packets = self.packets.clone();
        let mut bytes = self.bytes.clone();
        if packets.take_at(1, now) && bytes.take_at(len as u64, now) {
            self.packets = packets;
            self.bytes = bytes;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn allows_a_burst_then_refills_at_the_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new_at(10, 5, start);
        assert!((0..5).all(|_| bucket.take_at(1, start)));
        assert!(!bucket.take_at(1, start));
        assert!(bucket.take_at(1, start + Duration::from_millis(100)));
        assert!(!bucket.take_at(1, start + Duration::from_millis(100)));
        // Never more than the burst, however long it waits.
        let later = start + Duration::from_secs(60);
        assert!((0..5).all(|_| bucket.take_at(1, later)));
        assert!(!bucket.take_at(1, later));
    }

    #[test]
    fn limiter_enforces_packets_and_bytes() {
        let mut few_packets = Limiter::new(3, 1_000_000);
        assert_eq!((0..5).filter(|_| few_packets.allow(100)).count(), 3);

        let mut few_bytes = Limiter::new(1000, 8 * 1000); // 1000 bytes/s
        assert!(few_bytes.allow(600));
        assert!(!few_bytes.allow(600), "would exceed the byte budget");
        assert!(few_bytes.allow(300), "a refused packet costs nothing");
    }
}
