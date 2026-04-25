//! Legacy `mlink ping <peer_id>` — connect to a specific peer, send a single
//! Heartbeat, wait for the reply, print the round-trip time. Useful for
//! sanity-checking link quality; the room-aware flows do not need this.

use std::time::Instant;

use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::Transport;

use crate::node_build::build_node;
use mlink_cli::TransportKind;

pub(crate) async fn cmd_ping(peer_id: String, kind: TransportKind) -> Result<(), MlinkError> {
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

    let app_uuid = node.connect_peer(transport.as_mut(), &discovered).await?;
    let start = Instant::now();
    node.send_raw(&app_uuid, MessageType::Heartbeat, &[]).await?;
    let (_frame, _payload) = node.recv_raw(&app_uuid).await?;
    let rtt = start.elapsed();
    println!("ping {app_uuid}: rtt={:.2}ms", rtt.as_secs_f64() * 1000.0);
    Ok(())
}
