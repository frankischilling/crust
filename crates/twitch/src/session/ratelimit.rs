use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crust_core::model::ChannelId;

/// A rolling-window limiter. A full burst stays charged until its timestamps
/// leave the window; time between sends does not refill a second burst.
pub struct RateLimiter {
    capacity: usize,
    window: Duration,
    sent: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(capacity: u32, window: Duration) -> Self {
        assert!(capacity > 0 && !window.is_zero());
        Self {
            capacity: capacity as usize,
            window,
            sent: VecDeque::new(),
        }
    }

    /// Twitch allows 20 JOIN attempts per user in a rolling 10-second window.
    pub fn join() -> Self {
        Self::new(20, Duration::from_secs(10))
    }

    pub fn try_consume(&mut self) -> bool {
        self.try_consume_at(Instant::now())
    }

    pub fn try_consume_at(&mut self, now: Instant) -> bool {
        if !self.wait_time_at(now).is_zero() {
            return false;
        }
        self.sent.push_back(now);
        true
    }

    pub fn wait_time(&mut self) -> Duration {
        self.wait_time_at(Instant::now())
    }

    pub fn wait_time_at(&mut self, now: Instant) -> Duration {
        self.wait_for_limit_at(self.capacity, now)
    }

    fn wait_for_limit_at(&mut self, limit: usize, now: Instant) -> Duration {
        while self
            .sent
            .front()
            .is_some_and(|sent| now.saturating_duration_since(*sent) >= self.window)
        {
            self.sent.pop_front();
        }
        if self.sent.len() < limit {
            Duration::ZERO
        } else {
            // A lower limit can be exceeded by privileged messages. Wait until
            // enough of those messages expire, not merely the oldest one.
            let oldest_blocking = self.sent[self.sent.len() - limit];
            (oldest_blocking + self.window).saturating_duration_since(now)
        }
    }
}

/// Both chat limits count all messages from this user. Ordinary channels stop
/// at 20 per 30 seconds; broadcaster/moderator/VIP channels can continue to 100.
/// Ordinary channels also require a full second between messages.
pub(super) struct ChatRateLimiter {
    global: RateLimiter,
    last_by_channel: HashMap<ChannelId, Instant>,
}

impl ChatRateLimiter {
    pub(super) fn new() -> Self {
        Self {
            global: RateLimiter::new(100, Duration::from_secs(30)),
            last_by_channel: HashMap::new(),
        }
    }

    pub(super) fn wait_time_at(
        &mut self,
        channel: &ChannelId,
        elevated: bool,
        now: Instant,
    ) -> Duration {
        let global_wait = self
            .global
            .wait_for_limit_at(if elevated { 100 } else { 20 }, now);
        let channel_wait = if elevated {
            Duration::ZERO
        } else {
            self.last_by_channel
                .get(channel)
                .map(|sent| (*sent + Duration::from_secs(1)).saturating_duration_since(now))
                .unwrap_or_default()
        };
        global_wait.max(channel_wait)
    }

    pub(super) fn try_consume_at(
        &mut self,
        channel: &ChannelId,
        elevated: bool,
        now: Instant,
    ) -> bool {
        if !self.wait_time_at(channel, elevated, now).is_zero() {
            return false;
        }
        if !self.global.try_consume_at(now) {
            return false;
        }
        self.last_by_channel
            .retain(|_, sent| now.saturating_duration_since(*sent) < Duration::from_secs(1));
        self.last_by_channel.insert(channel.clone(), now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_does_not_refill_before_its_rolling_window_expires() {
        let now = Instant::now();
        let mut limiter = RateLimiter::new(20, Duration::from_secs(10));
        for _ in 0..20 {
            assert!(limiter.try_consume_at(now));
        }
        assert!(!limiter.try_consume_at(now + Duration::from_secs(5)));
        assert_eq!(
            limiter.wait_time_at(now + Duration::from_secs(9)),
            Duration::from_secs(1)
        );
        assert!(limiter.try_consume_at(now + Duration::from_secs(10)));
    }

    #[test]
    fn staggered_sends_expire_individually() {
        let now = Instant::now();
        let mut limiter = RateLimiter::new(2, Duration::from_secs(10));
        assert!(limiter.try_consume_at(now));
        assert!(limiter.try_consume_at(now + Duration::from_secs(4)));
        assert!(limiter.try_consume_at(now + Duration::from_secs(10)));
        assert_eq!(
            limiter.wait_time_at(now + Duration::from_secs(10)),
            Duration::from_secs(4)
        );
        assert!(!limiter.try_consume_at(now + Duration::from_secs(13)));
        assert!(limiter.try_consume_at(now + Duration::from_secs(14)));
    }

    #[test]
    fn ordinary_channel_spacing_does_not_block_another_channel() {
        let now = Instant::now();
        let mut limiter = ChatRateLimiter::new();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        assert!(limiter.try_consume_at(&first, false, now));
        assert!(!limiter.try_consume_at(&first, false, now + Duration::from_millis(999)));
        assert!(limiter.try_consume_at(&second, false, now));
        assert!(limiter.try_consume_at(&first, false, now + Duration::from_secs(1)));
    }

    #[test]
    fn elevated_messages_count_against_the_ordinary_global_limit() {
        let now = Instant::now();
        let mut limiter = ChatRateLimiter::new();
        let elevated = ChannelId::new("moderated");
        let ordinary = ChannelId::new("ordinary");
        for _ in 0..20 {
            assert!(limiter.try_consume_at(&elevated, true, now));
        }
        assert!(!limiter.try_consume_at(&ordinary, false, now));
        for _ in 20..100 {
            assert!(limiter.try_consume_at(&elevated, true, now));
        }
        assert!(!limiter.try_consume_at(&elevated, true, now));
        assert!(limiter.try_consume_at(&ordinary, false, now + Duration::from_secs(30)));
    }

    #[test]
    fn ordinary_send_waits_until_fewer_than_twenty_total_messages_remain() {
        let now = Instant::now();
        let mut limiter = ChatRateLimiter::new();
        let channel = ChannelId::new("room");
        for _ in 0..20 {
            assert!(limiter.try_consume_at(&channel, true, now));
        }
        for _ in 0..20 {
            assert!(limiter.try_consume_at(&channel, true, now + Duration::from_secs(5)));
        }
        assert_eq!(
            limiter.wait_time_at(&channel, false, now + Duration::from_secs(30)),
            Duration::from_secs(5)
        );
        assert!(!limiter.try_consume_at(&channel, false, now + Duration::from_secs(30)));
        assert!(limiter.try_consume_at(&channel, false, now + Duration::from_secs(35)));
    }

    #[test]
    fn losing_privileges_keeps_the_channel_send_interval() {
        let now = Instant::now();
        let channel = ChannelId::new("room");
        let mut limiter = ChatRateLimiter::new();
        assert!(limiter.try_consume_at(&channel, true, now));
        assert!(!limiter.try_consume_at(&channel, false, now));
        assert!(limiter.try_consume_at(&channel, false, now + Duration::from_secs(1)));
    }
}
