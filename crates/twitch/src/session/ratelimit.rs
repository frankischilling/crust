use std::time::{Duration, Instant};

/// Simple token-bucket rate limiter.
///
/// Twitch documented limits (normal user):
///   - 20 messages per 30 seconds per channel
///   - 100 JOIN commands per 15 seconds
///
/// Mods/VIPs can send up to 100 messages per 30 seconds, but we start
/// conservatively and the server will NOTICE us if we breach limits.
pub struct RateLimiter {
    /// Maximum tokens in the bucket.
    capacity: u32,
    /// Current token count.
    tokens: u32,
    /// How long it takes to refill one token.
    refill_interval: Duration,
    /// When we last added tokens.
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a rate limiter: `capacity` messages per `window`.
    pub fn new(capacity: u32, window: Duration) -> Self {
        // One token per (window / capacity).
        let refill_interval = window / capacity;
        Self {
            capacity,
            tokens: capacity,
            refill_interval,
            last_refill: Instant::now(),
        }
    }

    /// Standard chat rate limiter: 18 msgs / 30 s (small safety margin below 20).
    pub fn chat() -> Self {
        Self::new(18, Duration::from_secs(30))
    }

    /// JOIN rate limiter: 20 JOINs / 10 s.
    pub fn join() -> Self {
        Self::new(20, Duration::from_secs(10))
    }

    /// Refill tokens based on elapsed time.
    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed();
        let new_tokens = (elapsed.as_millis() / self.refill_interval.as_millis()) as u32;
        if new_tokens > 0 {
            self.tokens = (self.tokens + new_tokens).min(self.capacity);
            self.last_refill += self.refill_interval * new_tokens;
        }
    }

    /// Attempt to consume one token.  Returns `true` if a message may be sent.
    pub fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }

    /// How long to wait before a token becomes available.
    pub fn wait_time(&mut self) -> Duration {
        self.refill();
        if self.tokens > 0 {
            Duration::ZERO
        } else {
            let elapsed_since_last = self.last_refill.elapsed();
            self.refill_interval.saturating_sub(elapsed_since_last)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_capacity() {
        let mut rl = RateLimiter::new(5, Duration::from_secs(5));
        for _ in 0..5 {
            assert!(rl.try_consume());
        }
        assert!(!rl.try_consume());
    }
}
