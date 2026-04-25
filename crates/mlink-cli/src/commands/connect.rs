//! Legacy `mlink connect <peer_id>` — scan once, dial a specific wire id,
//! report the handshake's app_uuid. The room-aware flows in serve/chat/join
//! supersede this but it stays useful for debugging single-peer dials.

use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::Transport;

use crate::node_build::build_node;
use mlink_cli::TransportKind;

pub(crate) async fn cmd_connect(peer_id: String, kind: TransportKind) -> Result<(), MlinkError> {
    let mut transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let peers = transport.discover().await?;
    let discovered = peers.into_iter().find(|p| p.id == peer_id).ok_or_else(|| {
        MlinkError::HandlerError(format!("peer {peer_id} not found in scan results"))
    })?;

    let node = build_node().await?;
    node.start().await?;

    println!("connecting to {peer_id}...");
    let app_uuid = node.connect_peer(transport.as_mut(), &discovered).await?;
    println!("connected (app_uuid={app_uuid})");
    Ok(())
}
