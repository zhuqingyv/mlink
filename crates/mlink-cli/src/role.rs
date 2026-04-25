//! BLE role arbitration at the CLI layer. Wraps the pure `should_dial_as_central`
//! helper from mlink-core with the transport-kind and metadata-length branching
//! that `cmd_serve` / `cmd_chat` inline today.
//!
//! TCP has no role ambiguity (every node can freely dial and accept without
//! stepping on its own advertisement), so `RoleDecision::Dial` is the answer
//! for every non-BLE transport. Under BLE we gate on the 8-byte identity the
//! peer advertised in its metadata.

use mlink_core::transport::ble::should_dial_as_central;

use crate::TransportKind;

/// Whether this side should drive the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleDecision {
    /// Dial as central. This is either a TCP peer, a BLE peer whose identity
    /// is shorter than ours, or a BLE peer whose metadata didn't carry a full
    /// 8-byte identity and falls through to the symmetric-dial fallback.
    Dial,
    /// Skip dialling — the peer outranks us and will dial in. Only returned
    /// when `should_dial_as_central` explicitly said `Some(false)`.
    Skip,
    /// Tie or unparsable local UUID on BLE. Caller falls back to the legacy
    /// symmetric-dial path — both sides attempt, first writer wins.
    SymmetricFallback,
}

/// Decide the dial role. `peer_metadata` is the scanner's opaque bag (the
/// 8-byte identity only populates under BLE when the peer advertised it).
pub fn decide_ble_role(
    kind: TransportKind,
    local_app_uuid: &str,
    peer_metadata: &[u8],
) -> RoleDecision {
    // TCP has no role arbitration — every TCP node advertises + dials freely.
    if !matches!(kind, TransportKind::Ble) {
        return RoleDecision::Dial;
    }
    // BLE advertisement carries an 8-byte identity in `metadata`. Anything
    // else is an older peer or a scan hit without the 0000FFBB UUID — keep
    // the legacy symmetric-dial behaviour so nothing hangs.
    if peer_metadata.len() != 8 {
        return RoleDecision::Dial;
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(peer_metadata);
    match should_dial_as_central(local_app_uuid, &id) {
        Some(true) => RoleDecision::Dial,
        Some(false) => RoleDecision::Skip,
        None => RoleDecision::SymmetricFallback,
    }
}
