//! Background discovery. Started once at daemon boot so every WS client shares
//! a single scanner + accept loop per transport. Join/leave only toggles the
//! set of room hashes the node validates against at handshake time — the
//! scanner itself runs with no room filter so peers from every active room can
//! be surfaced through the same transport.
//!
//! Transport selection is driven by `MLINK_DAEMON_TRANSPORT`:
//! - `"tcp"` (default) — mDNS + TCP
//! - `"ble"` — Core Bluetooth (macOS only; falls back to TCP elsewhere)
//! - `"dual"` — both in parallel; a single `Arc<Node>` sees link additions from
//!   either side and SessionManager merges them into one Session per peer.

use std::sync::Arc;

use mlink_core::core::node::Node;

mod connect;
mod tcp;
#[cfg(target_os = "macos")]
mod ble;

pub use connect::connect_loop;

/// Choice of link-layer transport. Parsed from `MLINK_DAEMON_TRANSPORT` —
/// anything other than a literal `"ble"` / `"dual"` falls back to TCP so a
/// typo doesn't silently boot a radio the user didn't ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonTransport {
    Tcp,
    Ble,
    Dual,
}

impl DaemonTransport {
    pub fn from_env() -> Self {
        match std::env::var("MLINK_DAEMON_TRANSPORT").as_deref() {
            Ok("ble") => DaemonTransport::Ble,
            Ok("dual") => DaemonTransport::Dual,
            _ => DaemonTransport::Tcp,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DaemonTransport::Tcp => "tcp",
            DaemonTransport::Ble => "ble",
            DaemonTransport::Dual => "dual",
        }
    }
}

/// Spawn long-lived discovery + auto-connect loops for the requested mode.
/// Returns immediately; all work runs on tokio tasks for the lifetime of the
/// process. Errors bubble up as tracing warnings — the daemon keeps serving WS
/// regardless.
pub fn spawn(node: Arc<Node>, transport: DaemonTransport) {
    match transport {
        DaemonTransport::Tcp => tcp::spawn_tcp(node),
        DaemonTransport::Ble => spawn_ble_or_fallback(node),
        DaemonTransport::Dual => {
            // Share the same Node so SessionManager naturally merges a BLE link
            // and a TCP link for the same peer into one Session.
            tcp::spawn_tcp(Arc::clone(&node));
            spawn_ble_or_fallback(node);
        }
    }
    tracing::info!(transport = transport.label(), "discovery loop started");
}

#[cfg(target_os = "macos")]
fn spawn_ble_or_fallback(node: Arc<Node>) {
    ble::spawn_ble(node);
}

#[cfg(not(target_os = "macos"))]
fn spawn_ble_or_fallback(node: Arc<Node>) {
    tracing::warn!("BLE daemon transport only supported on macOS; falling back to TCP");
    tcp::spawn_tcp(node);
}
