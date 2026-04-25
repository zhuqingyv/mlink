//! Minimal glue shared across every `cmd_*`: hostname resolution, the
//! default `Node::new(...)` factory, the transport-label lookup used in log
//! lines, and the typed-error wrapper around `mlink_cli::validate_room_code`.
//!
//! Nothing here is transport-specific — the session/ helpers and individual
//! command modules all import from this single leaf to avoid re-implementing
//! the same 6-line helpers three times.

use mlink_core::core::node::{Node, NodeConfig};
use mlink_core::protocol::errors::MlinkError;

use mlink_cli::TransportKind;

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(hostname_sysctl)
        .unwrap_or_else(|| "mlink-node".into())
}

fn hostname_sysctl() -> Option<String> {
    // Portable-enough: read from `HOST` / `COMPUTERNAME` / fallback.
    std::env::var("HOST")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
}

pub(crate) async fn build_node() -> Result<Node, MlinkError> {
    Node::new(NodeConfig {
        name: host_name(),
        encrypt: true,
        trust_store_path: None,
    })
    .await
}

pub(crate) fn transport_label(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Ble => "ble",
        TransportKind::Tcp => "tcp",
    }
}

/// Strongly-typed wrapper around the string-returning validator in `lib.rs`.
/// Every command that accepts a room code goes through this so the error
/// surfaces as `MlinkError::HandlerError` in the CLI error-printing path.
pub(crate) fn validate_room_code(code: &str) -> Result<(), MlinkError> {
    mlink_cli::validate_room_code(code).map_err(MlinkError::HandlerError)
}
