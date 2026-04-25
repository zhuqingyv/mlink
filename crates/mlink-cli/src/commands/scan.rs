//! Legacy `mlink scan` — one-shot discovery, prints a peer table. Retained
//! for low-level troubleshooting; room-aware flows use `mlink join` or the
//! `send` command instead.

use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::Transport;

use crate::util::printing::{print_bluetooth_permission_hint, print_peer_list};
use mlink_cli::TransportKind;

pub(crate) async fn cmd_scan(kind: TransportKind) -> Result<(), MlinkError> {
    let mut transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let peers = match transport.discover().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[mlink] scan failed: {e}");
            if matches!(kind, TransportKind::Ble) {
                print_bluetooth_permission_hint();
            }
            return Err(e);
        }
    };
    if peers.is_empty() && matches!(kind, TransportKind::Ble) {
        print_bluetooth_permission_hint();
    }
    print_peer_list(&peers);
    Ok(())
}
