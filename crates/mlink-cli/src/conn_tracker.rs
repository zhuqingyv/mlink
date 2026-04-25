//! Per-command connection bookkeeping extracted from `cmd_serve` / `cmd_chat` /
//! `cmd_join`. The three commands previously juggled five mutable collections
//! (`engaged_wire_ids`, `connected_inbound`, `connected_peers`, `wire_to_app`,
//! `attempts`) inline inside a `tokio::select!` loop. Pulling the pure state
//! machine here lets us unit-test the transitions without spinning up a BLE
//! stack or a Node.
//!
//! Scope: only the dedup / retry / inbound-vs-outbound arbitration logic.
//! The async dial and handshake stay in the caller.

use std::collections::{HashMap, HashSet};

pub const MAX_RETRIES: u8 = 3;

/// Why a dial attempt is being skipped for a given wire id. Exposed so callers
/// can log the reason verbatim instead of re-deriving it from state checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// We already accepted an inbound connection from this wire id — dialling
    /// on top would race the live link.
    AlreadyInbound,
    /// A dial attempt is already in flight (or an accept is mid-handshake).
    AlreadyEngaged,
    /// We have a live Node-level connection to the app_uuid this wire id maps
    /// to — a second scan-surface of the same peer should not redial.
    AlreadyConnected { app_uuid: String },
    /// Attempts bucket is pinned at the max. Used after RoomMismatch to stop
    /// rediallling a peer we just rejected; the counter is pinned by calling
    /// `mark_room_mismatch`.
    MaxRetries { attempts: u8 },
}

/// State machine that tracks which wire ids are engaged, which peers are
/// connected (by app_uuid), and per-wire retry counts.
///
/// The three caller sites each instantiate one tracker per `tokio::select!`
/// loop. None of the methods are async; they only mutate the in-memory maps.
#[derive(Debug, Default)]
pub struct ConnTracker {
    engaged: HashSet<String>,
    inbound: HashSet<String>,
    connected: HashSet<String>,
    wire_to_app: HashMap<String, String>,
    attempts: HashMap<String, u8>,
}

impl ConnTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the skip reason for the given wire id, or `None` if the caller
    /// should proceed to evaluate the BLE role gate and then dial.
    ///
    /// Checks run in the same order as the inline code: inbound first, then
    /// engaged, then the app_uuid-already-connected shortcut, then the
    /// attempts-maxed terminal bucket. Ordering matters because it dictates
    /// which log line the CLI prints.
    pub fn should_skip_dial(&self, wire_id: &str) -> Option<SkipReason> {
        if self.inbound.contains(wire_id) {
            return Some(SkipReason::AlreadyInbound);
        }
        if self.engaged.contains(wire_id) {
            return Some(SkipReason::AlreadyEngaged);
        }
        if let Some(app) = self.wire_to_app.get(wire_id) {
            if self.connected.contains(app) {
                return Some(SkipReason::AlreadyConnected {
                    app_uuid: app.clone(),
                });
            }
        }
        if let Some(&n) = self.attempts.get(wire_id) {
            if n >= MAX_RETRIES {
                return Some(SkipReason::MaxRetries { attempts: n });
            }
        }
        None
    }

    /// Record that a dial is about to start. Increments the per-wire attempt
    /// counter, marks the wire id engaged, and returns the new attempt number
    /// (1-based). Returns `None` when the wire has already hit `MAX_RETRIES`;
    /// callers should treat that as "give up".
    ///
    /// Calling this without a prior `should_skip_dial` check is safe — the
    /// MAX_RETRIES guard is enforced here as well — but will not emit the
    /// informational skip reasons.
    pub fn bump_attempt(&mut self, wire_id: &str) -> Option<u8> {
        let n = self.attempts.entry(wire_id.to_string()).or_insert(0);
        if *n >= MAX_RETRIES {
            return None;
        }
        *n += 1;
        let attempt_no = *n;
        self.engaged.insert(wire_id.to_string());
        Some(attempt_no)
    }

    /// Record a successful dial: clear the retry counter, remember the
    /// wire→app mapping, and mark the app_uuid connected. Leaves the engaged
    /// set populated (we still hold the link) so duplicate scanner hits keep
    /// being filtered.
    pub fn on_dial_ok(&mut self, wire_id: &str, app_uuid: &str) {
        self.attempts.remove(wire_id);
        self.wire_to_app
            .insert(wire_id.to_string(), app_uuid.to_string());
        self.connected.insert(app_uuid.to_string());
    }

    /// Record a failed dial. Drops the engaged slot so a retry can re-acquire
    /// it; the attempt counter was already bumped by `bump_attempt` and is
    /// left intact so backoff can converge on the MAX_RETRIES ceiling.
    pub fn on_dial_err(&mut self, wire_id: &str) {
        self.engaged.remove(wire_id);
    }

    /// Record a RoomMismatch on the dial path. Pins attempts at MAX so the
    /// scanner won't keep resurrecting the same wire id, and releases the
    /// engaged slot. The pinned counter is the "no retry" sentinel.
    pub fn mark_room_mismatch_dial(&mut self, wire_id: &str) {
        self.attempts.insert(wire_id.to_string(), MAX_RETRIES);
        self.engaged.remove(wire_id);
    }

    /// Mark the start of accepting an inbound connection. Flags both engaged
    /// (so the scanner won't also dial) and inbound (so the authoritative
    /// direction is remembered even if the handshake later fails).
    pub fn on_accept_begin(&mut self, wire_id: &str) {
        self.engaged.insert(wire_id.to_string());
        self.inbound.insert(wire_id.to_string());
    }

    /// Accepted handshake succeeded. Same bookkeeping as `on_dial_ok` plus
    /// leaves the inbound flag in place so future scanner hits of the same
    /// wire id skip via `SkipReason::AlreadyInbound`.
    pub fn on_accept_ok(&mut self, wire_id: &str, app_uuid: &str) {
        self.attempts.remove(wire_id);
        self.wire_to_app
            .insert(wire_id.to_string(), app_uuid.to_string());
        self.connected.insert(app_uuid.to_string());
    }

    /// Inbound handshake failed (room mismatch or other error). Release both
    /// the engaged and inbound slots so a future dial can re-acquire them.
    pub fn on_accept_err(&mut self, wire_id: &str) {
        self.engaged.remove(wire_id);
        self.inbound.remove(wire_id);
    }

    /// NodeEvent::PeerConnected callback — a peer surfaced at the Node layer
    /// without us observing the dial (e.g. the dial raced the subscribe).
    pub fn on_peer_connected(&mut self, app_uuid: &str) {
        self.connected.insert(app_uuid.to_string());
    }

    /// NodeEvent::PeerDisconnected callback — drop the app_uuid and any
    /// wire→app entries that pointed at it, so the next scanner hit for the
    /// underlying wire id is free to redial.
    pub fn on_peer_disconnected(&mut self, app_uuid: &str) {
        self.connected.remove(app_uuid);
        self.wire_to_app.retain(|_, app| app != app_uuid);
    }

    // ---- read-only accessors (for tests + debug printing) -----------------

    pub fn is_engaged(&self, wire_id: &str) -> bool {
        self.engaged.contains(wire_id)
    }

    pub fn is_inbound(&self, wire_id: &str) -> bool {
        self.inbound.contains(wire_id)
    }

    pub fn is_connected(&self, app_uuid: &str) -> bool {
        self.connected.contains(app_uuid)
    }

    pub fn attempts(&self, wire_id: &str) -> u8 {
        self.attempts.get(wire_id).copied().unwrap_or(0)
    }

    pub fn app_for_wire(&self, wire_id: &str) -> Option<&str> {
        self.wire_to_app.get(wire_id).map(|s| s.as_str())
    }
}
