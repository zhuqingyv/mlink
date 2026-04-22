//! Per-room in-memory message queue.
//!
//! Owns the daemon-side backlog of messages received while no client is
//! actively draining them. Messages are bucketed by room code; each bucket is
//! bounded at `MAX_MESSAGES_PER_ROOM` entries with the oldest evicted on
//! overflow (bounded FIFO). Drains are destructive — callers take ownership of
//! the pending batch and the queue is left empty for that room.
//!
//! The struct is not internally synchronized; wrap it in
//! `Arc<std::sync::Mutex<MessageQueue>>` at the daemon layer so multiple
//! producer tasks (event bridge) and consumer tasks (WS sessions) can share
//! one instance.
//!
//! # Example
//! ```
//! use std::sync::{Arc, Mutex};
//! use mlink_daemon::queue::{MessageEntry, MessageQueue};
//! use serde_json::json;
//!
//! let q = Arc::new(Mutex::new(MessageQueue::new()));
//! q.lock().unwrap().push(MessageEntry {
//!     room: "room-1".into(),
//!     from: "peer-a".into(),
//!     payload: json!({"text": "hi"}),
//!     ts: 0,
//! });
//! let pending = q.lock().unwrap().drain("room-1");
//! assert_eq!(pending.len(), 1);
//! ```

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

/// Hard cap on pending messages per room. Chosen to bound memory without
/// truncating normal bursts — a typical chat generates far fewer than 500
/// unseen messages between reconnects.
pub const MAX_MESSAGES_PER_ROOM: usize = 500;

/// A single queued message. Payload is passed through verbatim as
/// `serde_json::Value` so the queue layer stays agnostic of the application
/// schema — the daemon only needs routing metadata (room / from / ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    /// Room code the message belongs to (matches the `join`-side room string).
    pub room: String,
    /// Sender's app-level UUID, as surfaced by the Node layer.
    pub from: String,
    /// Opaque payload — the daemon does not introspect message bodies.
    pub payload: Value,
    /// Unix timestamp in milliseconds. Producer-assigned; the queue never
    /// synthesizes or rewrites it.
    pub ts: u64,
}

/// In-memory, per-room bounded FIFO. Not `Sync` on its own; wrap in a `Mutex`
/// for multi-task access.
#[derive(Debug, Default)]
pub struct MessageQueue {
    queues: HashMap<String, VecDeque<MessageEntry>>,
}

impl MessageQueue {
    pub fn new() -> Self {
        Self { queues: HashMap::new() }
    }

    /// Push a message onto its room's queue, evicting the oldest entry when
    /// the bucket is already at `MAX_MESSAGES_PER_ROOM`.
    pub fn push(&mut self, entry: MessageEntry) {
        let bucket = self.queues.entry(entry.room.clone()).or_default();
        if bucket.len() >= MAX_MESSAGES_PER_ROOM {
            bucket.pop_front();
        }
        bucket.push_back(entry);
    }

    /// Take every pending message for `room`, leaving the bucket empty. An
    /// empty `Vec` is returned when the room has no queue or no messages.
    pub fn drain(&mut self, room: &str) -> Vec<MessageEntry> {
        match self.queues.get_mut(room) {
            Some(q) => q.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// Pending length for `room`; 0 when the room has never been seen.
    pub fn len(&self, room: &str) -> usize {
        self.queues.get(room).map(|q| q.len()).unwrap_or(0)
    }

    /// Drop the bucket for `room` entirely. Useful when a client leaves a
    /// room and the daemon wants to release memory rather than keep an empty
    /// `VecDeque` around indefinitely.
    pub fn remove_room(&mut self, room: &str) {
        self.queues.remove(room);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(room: &str, from: &str, ts: u64) -> MessageEntry {
        MessageEntry {
            room: room.to_string(),
            from: from.to_string(),
            payload: json!({ "seq": ts }),
            ts,
        }
    }

    #[test]
    fn push_and_drain_preserves_order() {
        let mut q = MessageQueue::new();
        q.push(entry("r1", "p", 1));
        q.push(entry("r1", "p", 2));
        q.push(entry("r1", "p", 3));
        assert_eq!(q.len("r1"), 3);

        let drained = q.drain("r1");
        let seqs: Vec<u64> = drained.iter().map(|e| e.ts).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(q.len("r1"), 0);
    }

    #[test]
    fn push_under_limit_keeps_all() {
        let mut q = MessageQueue::new();
        for i in 0..MAX_MESSAGES_PER_ROOM as u64 {
            q.push(entry("r1", "p", i));
        }
        assert_eq!(q.len("r1"), MAX_MESSAGES_PER_ROOM);

        let drained = q.drain("r1");
        assert_eq!(drained.len(), MAX_MESSAGES_PER_ROOM);
        assert_eq!(drained.first().unwrap().ts, 0);
        assert_eq!(drained.last().unwrap().ts, (MAX_MESSAGES_PER_ROOM - 1) as u64);
    }

    #[test]
    fn push_over_limit_drops_oldest() {
        let mut q = MessageQueue::new();
        // Insert 501 messages (ts 0..=500). ts=0 must be evicted; the
        // surviving window is ts=1..=500.
        for i in 0..=MAX_MESSAGES_PER_ROOM as u64 {
            q.push(entry("r1", "p", i));
        }
        assert_eq!(q.len("r1"), MAX_MESSAGES_PER_ROOM);

        let drained = q.drain("r1");
        assert_eq!(drained.len(), MAX_MESSAGES_PER_ROOM);
        assert_eq!(drained.first().unwrap().ts, 1);
        assert_eq!(drained.last().unwrap().ts, MAX_MESSAGES_PER_ROOM as u64);
    }

    #[test]
    fn push_far_over_limit_stays_bounded() {
        let mut q = MessageQueue::new();
        let overflow = MAX_MESSAGES_PER_ROOM as u64 + 250;
        for i in 0..overflow {
            q.push(entry("r1", "p", i));
        }
        assert_eq!(q.len("r1"), MAX_MESSAGES_PER_ROOM);

        let drained = q.drain("r1");
        assert_eq!(drained.len(), MAX_MESSAGES_PER_ROOM);
        // Oldest survivor is overflow - MAX; newest is overflow - 1.
        assert_eq!(drained.first().unwrap().ts, overflow - MAX_MESSAGES_PER_ROOM as u64);
        assert_eq!(drained.last().unwrap().ts, overflow - 1);
    }

    #[test]
    fn drain_clears_queue() {
        let mut q = MessageQueue::new();
        q.push(entry("r1", "p", 1));
        q.push(entry("r1", "p", 2));
        let first = q.drain("r1");
        assert_eq!(first.len(), 2);
        let second = q.drain("r1");
        assert!(second.is_empty());
        assert_eq!(q.len("r1"), 0);
    }

    #[test]
    fn drain_unknown_room_is_empty() {
        let mut q = MessageQueue::new();
        assert!(q.drain("nope").is_empty());
        assert_eq!(q.len("nope"), 0);
    }

    #[test]
    fn rooms_are_isolated() {
        let mut q = MessageQueue::new();
        q.push(entry("r1", "p", 1));
        q.push(entry("r2", "p", 2));
        q.push(entry("r2", "p", 3));

        assert_eq!(q.len("r1"), 1);
        assert_eq!(q.len("r2"), 2);

        let r2 = q.drain("r2");
        assert_eq!(r2.len(), 2);
        // r1 untouched.
        assert_eq!(q.len("r1"), 1);
    }

    #[test]
    fn remove_room_drops_bucket() {
        let mut q = MessageQueue::new();
        q.push(entry("r1", "p", 1));
        q.push(entry("r2", "p", 2));
        q.remove_room("r1");
        assert_eq!(q.len("r1"), 0);
        assert_eq!(q.len("r2"), 1);
        // Removing a non-existent room is a no-op.
        q.remove_room("ghost");
        assert_eq!(q.len("r2"), 1);
    }

    #[test]
    fn remove_then_push_starts_fresh() {
        let mut q = MessageQueue::new();
        for i in 0..10 {
            q.push(entry("r1", "p", i));
        }
        q.remove_room("r1");
        assert_eq!(q.len("r1"), 0);
        q.push(entry("r1", "p", 99));
        let drained = q.drain("r1");
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].ts, 99);
    }
}
