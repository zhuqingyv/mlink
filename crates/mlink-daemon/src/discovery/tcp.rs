//! TCP discovery flavor: mDNS advertisement + scanner + connect transport.
//! Shared logic lives in `connect::connect_loop`.

use std::sync::Arc;

use mlink_core::core::node::Node;
use mlink_core::core::scanner::Scanner;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::sync::mpsc;

use super::connect_loop;

pub(super) fn spawn_tcp(node: Arc<Node>) {
    let local_name = node.config().name.clone();
    let app_uuid = node.app_uuid().to_string();

    // Peripheral (TCP listener + mDNS advertisement). No room filter — every
    // joined room shares a single listener; handshake does the real filtering.
    let (accepted_tx, accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    let mut listen = TcpTransport::new();
    listen.set_local_name(local_name);
    listen.set_app_uuid(app_uuid.clone());
    tokio::spawn(async move {
        loop {
            match listen.listen().await {
                Ok(conn) => {
                    if accepted_tx.send(conn).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "tcp accept failed");
                    return;
                }
            }
        }
    });

    // Scanner (no room filter).
    let (peer_tx, peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    let mut scan = TcpTransport::new();
    scan.set_app_uuid(app_uuid.clone());
    let mut scanner = Scanner::new(Box::new(scan), app_uuid);
    scanner.set_unsee_channel(unsee_rx);
    tokio::spawn(async move {
        if let Err(e) = scanner.discover_loop(peer_tx).await {
            tracing::warn!(error = %e, "tcp scanner exited");
        }
    });

    let connect = {
        let mut t = TcpTransport::new();
        t.set_app_uuid(node.app_uuid().to_string());
        Box::new(t) as Box<dyn Transport>
    };
    tokio::spawn(connect_loop(node, connect, peer_rx, accepted_rx, unsee_tx, "tcp"));
}
