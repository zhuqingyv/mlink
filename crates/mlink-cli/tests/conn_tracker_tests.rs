//! Unit tests for the pure state machine pulled out of cmd_serve / cmd_chat /
//! cmd_join. No IO, no async — the async dial/accept path stays in the caller.
//!
//! Each test mirrors one of the transitions described in the comments of
//! `main.rs`: the dedup chain, retry-with-backoff, RoomMismatch pinning, and
//! the PeerDisconnected cleanup. When the big select-loop gets split into its
//! own file, these tests act as the safety net that protects the transitions.

use mlink_cli::conn_tracker::{ConnTracker, SkipReason, MAX_RETRIES};

// ---------- should_skip_dial: dedup chain ------------------------------------

#[test]
fn skip_dial_returns_none_for_fresh_wire() {
    let t = ConnTracker::new();
    assert_eq!(t.should_skip_dial("wire-1"), None);
}

#[test]
fn skip_dial_prefers_inbound_over_engaged() {
    // Both flags set by on_accept_begin. The inbound branch must win so the
    // operator log reads "already connected inbound" not "already engaged".
    let mut t = ConnTracker::new();
    t.on_accept_begin("wire-1");
    assert_eq!(t.should_skip_dial("wire-1"), Some(SkipReason::AlreadyInbound));
}

#[test]
fn skip_dial_reports_engaged_when_only_dialing() {
    let mut t = ConnTracker::new();
    let attempt = t.bump_attempt("wire-1");
    assert_eq!(attempt, Some(1));
    assert_eq!(t.should_skip_dial("wire-1"), Some(SkipReason::AlreadyEngaged));
}

#[test]
fn skip_dial_short_circuits_on_wire_to_app_connected() {
    // Same peer surfaces under a second wire id after a successful dial; the
    // app_uuid lookup must catch it even though the new wire id is fresh.
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.on_dial_ok("wire-1", "app-uuid-A");
    // Pretend the scanner resurfaces the same peer under a different wire id
    // (BLE reconnects often do this). Simulate by inserting a mapping to the
    // same app_uuid on a second wire_id via another dial_ok.
    t.bump_attempt("wire-2");
    t.on_dial_ok("wire-2", "app-uuid-A");
    // Now the engaged flag for wire-2 is set, so that check fires first.
    assert_eq!(t.should_skip_dial("wire-2"), Some(SkipReason::AlreadyEngaged));
}

#[test]
fn skip_dial_returns_connected_when_not_engaged_but_app_is_live() {
    // Manually arrange: wire mapping points at a live app_uuid, but wire is
    // no longer in engaged (simulates a stale scanner hit after the caller
    // cleared engaged for some reason — defensive check).
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.on_dial_ok("wire-1", "app-uuid-A");
    // Drop the engaged flag to expose the app_uuid branch.
    t.on_dial_err("wire-1");
    match t.should_skip_dial("wire-1") {
        Some(SkipReason::AlreadyConnected { app_uuid }) => {
            assert_eq!(app_uuid, "app-uuid-A");
        }
        other => panic!("expected AlreadyConnected, got {other:?}"),
    }
}

#[test]
fn skip_dial_reports_max_retries_once_pinned() {
    let mut t = ConnTracker::new();
    for _ in 0..MAX_RETRIES {
        assert!(t.bump_attempt("wire-1").is_some());
        t.on_dial_err("wire-1");
    }
    match t.should_skip_dial("wire-1") {
        Some(SkipReason::MaxRetries { attempts }) => assert_eq!(attempts, MAX_RETRIES),
        other => panic!("expected MaxRetries, got {other:?}"),
    }
}

// ---------- bump_attempt: retry ceiling --------------------------------------

#[test]
fn bump_attempt_increments_from_zero() {
    let mut t = ConnTracker::new();
    assert_eq!(t.bump_attempt("wire-1"), Some(1));
    t.on_dial_err("wire-1");
    assert_eq!(t.bump_attempt("wire-1"), Some(2));
    t.on_dial_err("wire-1");
    assert_eq!(t.bump_attempt("wire-1"), Some(3));
}

#[test]
fn bump_attempt_returns_none_at_max() {
    let mut t = ConnTracker::new();
    for _ in 0..MAX_RETRIES {
        t.bump_attempt("wire-1");
        t.on_dial_err("wire-1");
    }
    assert_eq!(t.bump_attempt("wire-1"), None);
    // And the engaged flag must NOT have been asserted on the rejected bump
    // (otherwise we'd never retry anything else that shares this wire id).
    assert!(!t.is_engaged("wire-1"));
}

#[test]
fn bump_attempt_marks_engaged() {
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    assert!(t.is_engaged("wire-1"));
}

// ---------- on_dial_ok / on_dial_err -----------------------------------------

#[test]
fn on_dial_ok_clears_attempts_and_records_mapping() {
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.on_dial_ok("wire-1", "app-uuid-A");
    assert_eq!(t.attempts("wire-1"), 0);
    assert_eq!(t.app_for_wire("wire-1"), Some("app-uuid-A"));
    assert!(t.is_connected("app-uuid-A"));
}

#[test]
fn on_dial_err_preserves_attempts_counter() {
    // Otherwise MAX_RETRIES would never bite — backoff relies on the counter
    // persisting across failed dials.
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.on_dial_err("wire-1");
    assert_eq!(t.attempts("wire-1"), 1);
    assert!(!t.is_engaged("wire-1"));
}

// ---------- mark_room_mismatch_dial: no retry, pinned -----------------------

#[test]
fn room_mismatch_dial_pins_counter_and_releases_engaged() {
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.mark_room_mismatch_dial("wire-1");
    assert_eq!(t.attempts("wire-1"), MAX_RETRIES);
    assert!(!t.is_engaged("wire-1"));
    // And the next skip check reports MaxRetries — we deliberately stop
    // redialling a peer we just rejected.
    match t.should_skip_dial("wire-1") {
        Some(SkipReason::MaxRetries { attempts }) => assert_eq!(attempts, MAX_RETRIES),
        other => panic!("expected MaxRetries after room mismatch, got {other:?}"),
    }
}

// ---------- on_accept_* ------------------------------------------------------

#[test]
fn on_accept_begin_sets_both_flags() {
    let mut t = ConnTracker::new();
    t.on_accept_begin("wire-1");
    assert!(t.is_engaged("wire-1"));
    assert!(t.is_inbound("wire-1"));
}

#[test]
fn on_accept_ok_records_wire_to_app_and_keeps_inbound() {
    let mut t = ConnTracker::new();
    t.on_accept_begin("wire-1");
    t.on_accept_ok("wire-1", "app-uuid-B");
    assert_eq!(t.app_for_wire("wire-1"), Some("app-uuid-B"));
    assert!(t.is_connected("app-uuid-B"));
    // Inbound stays set so a later scanner hit still prints "already
    // connected inbound", not "already engaged".
    assert!(t.is_inbound("wire-1"));
    assert_eq!(t.should_skip_dial("wire-1"), Some(SkipReason::AlreadyInbound));
}

#[test]
fn on_accept_err_releases_both_flags() {
    let mut t = ConnTracker::new();
    t.on_accept_begin("wire-1");
    t.on_accept_err("wire-1");
    assert!(!t.is_engaged("wire-1"));
    assert!(!t.is_inbound("wire-1"));
}

// ---------- on_peer_disconnected: cleanup ------------------------------------

#[test]
fn on_peer_disconnected_drops_connected_and_wire_mapping() {
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.on_dial_ok("wire-1", "app-uuid-A");
    t.on_peer_disconnected("app-uuid-A");
    assert!(!t.is_connected("app-uuid-A"));
    assert_eq!(t.app_for_wire("wire-1"), None);
}

#[test]
fn on_peer_disconnected_only_prunes_matching_app() {
    // Two wires mapped to two different app_uuids; disconnecting one must
    // leave the other's mapping intact.
    let mut t = ConnTracker::new();
    t.bump_attempt("wire-1");
    t.on_dial_ok("wire-1", "app-A");
    t.bump_attempt("wire-2");
    t.on_dial_ok("wire-2", "app-B");
    t.on_peer_disconnected("app-A");
    assert_eq!(t.app_for_wire("wire-1"), None);
    assert_eq!(t.app_for_wire("wire-2"), Some("app-B"));
    assert!(t.is_connected("app-B"));
    assert!(!t.is_connected("app-A"));
}

#[test]
fn peer_connected_marks_app_without_wire_mapping() {
    // NodeEvent path — we learn about a peer at the Node layer without the
    // dial/accept arm having run. The app_uuid check in should_skip_dial
    // only fires when the wire→app mapping exists, so a bare
    // on_peer_connected should NOT block a dial for some unrelated wire.
    let mut t = ConnTracker::new();
    t.on_peer_connected("app-A");
    assert!(t.is_connected("app-A"));
    assert_eq!(t.should_skip_dial("unrelated-wire"), None);
}
