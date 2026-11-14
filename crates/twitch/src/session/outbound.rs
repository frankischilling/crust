use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use crust_core::model::ChannelId;

use super::ratelimit::{ChatRateLimiter, RateLimiter};

const MAX_PENDING_CHAT: usize = 256;
const MAX_PENDING_CHAT_BYTES: usize = 256 * 1024;
const MAX_PENDING_JOINS: usize = 512;

/// Pending output belongs to one connection. Dropping it on reconnect also
/// prevents unsent chat from crossing an account switch.
#[derive(Default)]
pub(super) struct OutboundQueue {
    joins: VecDeque<ChannelId>,
    pending_joins: HashSet<ChannelId>,
    joined: HashSet<ChannelId>,
    chats: HashMap<ChannelId, VecDeque<String>>,
    chat_order: VecDeque<ChannelId>,
    chat_count: usize,
    chat_bytes: usize,
}

impl OutboundQueue {
    /// Returns false for a channel already queued or joined.
    pub(super) fn join(&mut self, channel: ChannelId) -> Result<bool, ()> {
        if self.joined.contains(&channel) || self.pending_joins.contains(&channel) {
            return Ok(false);
        }
        if self.joins.len() >= MAX_PENDING_JOINS {
            return Err(());
        }
        self.pending_joins.insert(channel.clone());
        self.joins.push_back(channel);
        Ok(true)
    }

    /// Cancel unsent output for a channel. Return whether its JOIN was sent,
    /// in which case the caller must send PART.
    pub(super) fn leave(&mut self, channel: &ChannelId) -> bool {
        if self.pending_joins.remove(channel) {
            self.joins.retain(|pending| pending != channel);
        }
        if let Some(messages) = self.chats.remove(channel) {
            self.chat_count -= messages.len();
            self.chat_bytes -= messages.iter().map(String::len).sum::<usize>();
            self.chat_order.retain(|pending| pending != channel);
        }
        self.joined.remove(channel)
    }

    pub(super) fn message(&mut self, channel: ChannelId, line: String) -> bool {
        if self.chat_count >= MAX_PENDING_CHAT
            || line.len() > MAX_PENDING_CHAT_BYTES.saturating_sub(self.chat_bytes)
        {
            return false;
        }
        self.chat_count += 1;
        self.chat_bytes += line.len();
        let messages = self.chats.entry(channel.clone()).or_insert_with(|| {
            self.chat_order.push_back(channel);
            VecDeque::new()
        });
        messages.push_back(line);
        true
    }

    pub(super) fn next_send_at(
        &self,
        chat_limiter: &mut ChatRateLimiter,
        join_limiter: &mut RateLimiter,
        elevated_channels: &HashSet<ChannelId>,
        now: Instant,
    ) -> Option<Instant> {
        let mut next = self
            .joins
            .front()
            .map(|_| now + join_limiter.wait_time_at(now));
        for channel in &self.chat_order {
            if self.pending_joins.contains(channel) {
                continue;
            }
            let ready =
                now + chat_limiter.wait_time_at(channel, elevated_channels.contains(channel), now);
            next = Some(next.map_or(ready, |current| current.min(ready)));
        }
        next
    }

    /// Pick one eligible line. Each channel remains FIFO, while a throttled
    /// channel cannot hold up another channel's ready messages.
    pub(super) fn pop_ready(
        &mut self,
        chat_limiter: &mut ChatRateLimiter,
        join_limiter: &mut RateLimiter,
        elevated_channels: &HashSet<ChannelId>,
        now: Instant,
    ) -> Option<String> {
        if !self.joins.is_empty() && join_limiter.try_consume_at(now) {
            let channel = self.joins.pop_front().expect("queued JOIN");
            self.pending_joins.remove(&channel);
            self.joined.insert(channel.clone());
            return Some(format!("JOIN {}", channel.irc_name()));
        }
        for index in 0..self.chat_order.len() {
            let channel = &self.chat_order[index];
            if self.pending_joins.contains(channel)
                || !chat_limiter.try_consume_at(channel, elevated_channels.contains(channel), now)
            {
                continue;
            }
            let channel = self.chat_order.remove(index).expect("queued chat channel");
            let messages = self.chats.get_mut(&channel).expect("queued chat messages");
            let line = messages.pop_front().expect("nonempty chat queue");
            self.chat_count -= 1;
            self.chat_bytes -= line.len();
            if messages.is_empty() {
                self.chats.remove(&channel);
            } else {
                self.chat_order.push_back(channel);
            }
            return Some(line);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn duplicate_joins_and_canceled_channels_leave_no_extra_output() {
        let now = Instant::now();
        let mut queue = OutboundQueue::default();
        let mut chat = ChatRateLimiter::new();
        let mut joins = RateLimiter::join();
        let elevated = HashSet::new();
        let channel = ChannelId::new("room");

        assert_eq!(queue.join(channel.clone()), Ok(true));
        assert_eq!(queue.join(channel.clone()), Ok(false));
        assert!(queue.message(channel.clone(), "queued message".to_owned()));
        assert!(!queue.leave(&channel));
        assert!(queue
            .next_send_at(&mut chat, &mut joins, &elevated, now)
            .is_none());

        assert_eq!(queue.join(channel.clone()), Ok(true));
        assert_eq!(
            queue.pop_ready(&mut chat, &mut joins, &elevated, now),
            Some("JOIN #room".to_owned())
        );
        assert_eq!(queue.join(channel.clone()), Ok(false));
        assert!(queue.leave(&channel));
        assert!(!queue.leave(&channel));
        assert_eq!(queue.join(channel), Ok(true));
    }

    #[test]
    fn a_throttled_channel_preserves_order_without_blocking_another_channel() {
        let now = Instant::now();
        let first = ChannelId::new("first");
        let second = ChannelId::new("second");
        let mut queue = OutboundQueue::default();
        let mut chat = ChatRateLimiter::new();
        let mut joins = RateLimiter::join();
        let elevated = HashSet::new();
        assert!(chat.try_consume_at(&first, false, now));
        assert!(queue.message(first.clone(), "first 1".to_owned()));
        assert!(queue.message(first, "first 2".to_owned()));
        assert!(queue.message(second, "second 1".to_owned()));

        assert_eq!(
            queue.pop_ready(&mut chat, &mut joins, &elevated, now),
            Some("second 1".to_owned())
        );
        assert!(queue
            .pop_ready(&mut chat, &mut joins, &elevated, now)
            .is_none());
        for (seconds, expected) in [(1, "first 1"), (2, "first 2")] {
            assert_eq!(
                queue.pop_ready(
                    &mut chat,
                    &mut joins,
                    &elevated,
                    now + Duration::from_secs(seconds),
                ),
                Some(expected.to_owned())
            );
        }
    }

    #[test]
    fn pending_chat_has_count_and_byte_bounds_and_leave_releases_both() {
        let mut queue = OutboundQueue::default();
        let channel = ChannelId::new("room");
        assert!(!queue.message(channel.clone(), "x".repeat(MAX_PENDING_CHAT_BYTES + 1)));
        for _ in 0..MAX_PENDING_CHAT {
            assert!(queue.message(channel.clone(), "line".to_owned()));
        }
        assert!(!queue.message(channel.clone(), "overflow".to_owned()));
        queue.leave(&channel);
        assert!(queue.message(channel.clone(), "x".repeat(MAX_PENDING_CHAT_BYTES)));
        assert!(!queue.message(channel.clone(), "x".to_owned()));
        queue.leave(&channel);
        assert!(queue.message(channel, "available again".to_owned()));
    }

    #[test]
    fn join_queue_obeys_the_full_window_and_is_bounded() {
        let now = Instant::now();
        let mut queue = OutboundQueue::default();
        let mut chat = ChatRateLimiter::new();
        let mut joins = RateLimiter::join();
        let elevated = HashSet::new();
        for index in 0..MAX_PENDING_JOINS {
            assert_eq!(queue.join(ChannelId::new(format!("room{index}"))), Ok(true));
        }
        assert_eq!(queue.join(ChannelId::new("overflow")), Err(()));
        for _ in 0..20 {
            assert!(queue
                .pop_ready(&mut chat, &mut joins, &elevated, now)
                .is_some());
        }
        assert!(queue
            .pop_ready(
                &mut chat,
                &mut joins,
                &elevated,
                now + Duration::from_secs(5),
            )
            .is_none());
        assert!(queue
            .pop_ready(
                &mut chat,
                &mut joins,
                &elevated,
                now + Duration::from_secs(10),
            )
            .is_some());
    }
}
