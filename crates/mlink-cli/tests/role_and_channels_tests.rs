//! Tests for the CLI-layer BLE role decision and for the two channel-disable
//! tricks used by the select loops:
//!
//! 1. `decide_ble_role` — covers the five branches inlined into cmd_serve and
//!    cmd_chat before the scanner loop consumed a peer.
//! 2. `disabled_scanner` — when `ble_host_only` is true, cmd_serve/cmd_chat
//!    drop `unsee_rx`; further `unsee_tx.send()` must fail-fast, not buffer.
//! 3. `disabled_stdin` — in cmd_join without `--chat`, stdin_tx is dropped;
//!    stdin_rx.recv() then resolves to None on every tick, which keeps the
//!    arm inert inside the main select loop.

use mlink_cli::role::{decide_ble_role, RoleDecision};
use mlink_cli::TransportKind;
use tokio::sync::mpsc;

// ---- 1. decide_ble_role branches --------------------------------------------

#[test]
fn tcp_always_dials() {
    // Metadata is irrelevant under TCP — no role arbitration.
    let d = decide_ble_role(TransportKind::Tcp, "00000000-0000-0000-0000-000000000000", &[]);
    assert_eq!(d, RoleDecision::Dial);

    let d = decide_ble_role(
        TransportKind::Tcp,
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
        &[1, 2, 3, 4, 5, 6, 7, 8],
    );
    assert_eq!(d, RoleDecision::Dial);
}

#[test]
fn ble_with_wrong_metadata_length_falls_through_to_dial() {
    // Legacy peers / peers that didn't advertise the 0000FFBB UUID surface
    // with empty or short metadata. The code path explicitly allows dialling
    // so the link doesn't hang waiting for a role that never arrives.
    let uuid = "11111111-2222-3333-4444-555555555555";
    assert_eq!(decide_ble_role(TransportKind::Ble, uuid, &[]), RoleDecision::Dial);
    assert_eq!(
        decide_ble_role(TransportKind::Ble, uuid, &[1, 2, 3]),
        RoleDecision::Dial
    );
    // 9 bytes = also not the expected 8, must not crash or skip.
    assert_eq!(
        decide_ble_role(TransportKind::Ble, uuid, &[1, 2, 3, 4, 5, 6, 7, 8, 9]),
        RoleDecision::Dial
    );
}

#[test]
fn ble_peer_outranked_local_dials() {
    // should_dial_as_central compares the first 8 bytes of the parsed UUID
    // against the peer identity. A local UUID starting with 0xff..0xff
    // trivially outranks a peer whose identity is all zeros.
    let local = "ffffffff-ffff-ffff-ffff-000000000000";
    let peer = [0u8; 8];
    assert_eq!(
        decide_ble_role(TransportKind::Ble, local, &peer),
        RoleDecision::Dial
    );
}

#[test]
fn ble_peer_outranks_local_skips() {
    // Flip the previous case: peer identity is all 0xff, local starts at 0.
    let local = "00000000-0000-0000-0000-000000000000";
    let peer = [0xffu8; 8];
    assert_eq!(
        decide_ble_role(TransportKind::Ble, local, &peer),
        RoleDecision::Skip
    );
}

#[test]
fn ble_tie_or_invalid_uuid_falls_back_to_symmetric() {
    // Tie: local first-8-bytes equal peer identity → None → SymmetricFallback.
    // Use an identity that matches the UUID's first 8 bytes exactly.
    let local = "01020304-0506-0708-4444-000000000000";
    // Uuid::parse_str(local).as_bytes()[0..8] = 01 02 03 04 05 06 07 08.
    let peer = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
    assert_eq!(
        decide_ble_role(TransportKind::Ble, local, &peer),
        RoleDecision::SymmetricFallback
    );

    // Invalid UUID: parse fails inside should_dial_as_central → None →
    // SymmetricFallback. The callers fall through to legacy dial in both
    // cases so nothing hangs.
    assert_eq!(
        decide_ble_role(TransportKind::Ble, "not-a-uuid", &[0u8; 8]),
        RoleDecision::SymmetricFallback
    );
}

// ---- 2. disabled_scanner: dropped unsee_rx ----------------------------------
//
// Mirrors the `ble_host_only` branch inside cmd_serve / cmd_chat where the
// scanner task is NOT spawned, so no one will ever read from `unsee_rx`. The
// code explicitly drops the receiver so the backoff task's `unsee_tx.send()`
// fails immediately instead of growing an unread buffer.

#[tokio::test]
async fn disabled_scanner_unsee_send_fails_immediately() {
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    // Simulate the `ble_host_only` branch: drop the rx so no one can read.
    drop(unsee_rx);
    // After the receiver is dropped, send must surface SendError immediately —
    // not block, not silently buffer.
    let result = unsee_tx.send("wire-1".to_string()).await;
    assert!(
        result.is_err(),
        "expected send to fail after rx drop, got {result:?}"
    );
}

#[tokio::test]
async fn disabled_scanner_send_error_carries_payload() {
    // Sanity check: mpsc's SendError gives the payload back for logging.
    let (tx, rx) = mpsc::channel::<String>(16);
    drop(rx);
    let err = tx
        .send("payload".to_string())
        .await
        .expect_err("expected error");
    assert_eq!(err.0, "payload");
}

// ---- 3. disabled_stdin: dropped stdin_tx -----------------------------------
//
// Mirrors the `chat == false` branch in cmd_join. `stdin_tx` is dropped so
// `stdin_rx.recv()` resolves to None on every tick. The main select arm
// `let Some(line) = maybe_line else { continue; };` then keeps looping,
// effectively making the arm inert.

#[tokio::test]
async fn disabled_stdin_recv_returns_none() {
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(16);
    // Simulate the non-chat branch.
    drop(stdin_tx);
    // Resolves to None because all senders have been dropped.
    let got = stdin_rx.recv().await;
    assert!(got.is_none(), "expected None from closed rx, got {got:?}");
}
