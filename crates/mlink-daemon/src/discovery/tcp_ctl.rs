//! Controlled variant of `tcp::spawn_tcp`: identical bring-up, but keeps the
//! three `tokio::spawn` JoinHandles so the outer controller can abort them
//! when the WS layer asks to disable TCP at runtime. Kept separate from
//! `tcp.rs` so the original one-shot function stays small and readable for
//! startup paths that never stop.

use std::sync::Arc;

use mlink_core::core::node::Node;
use mlink_core::core::scanner::Scanner;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::sync::mpsc;

use super::{connect_loop, TransportHandles};

pub(super) fn spawn_tcp_controlled(node: Arc<Node>) -> TransportHandles {
    let local_name = node.config().name.clone();
    let app_uuid = node.app_uuid().to_string();

    let (accepted_tx, accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    let mut listen = TcpTransport::new();
    listen.set_local_name(local_name);
    listen.set_app_uuid(app_uuid.clone());
    let listen_task = tokio::spawn(async move {
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

    let (peer_tx, peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    let mut scan = TcpTransport::new();
    scan.set_app_uuid(app_uuid.clone());
    let mut scanner = Scanner::new(Box::new(scan), app_uuid);
    scanner.set_unsee_channel(unsee_rx);
    let scan_task = tokio::spawn(async move {
        if let Err(e) = scanner.discover_loop(peer_tx).await {
            tracing::warn!(error = %e, "tcp scanner exited");
        }
    });

    let connect = {
        let mut t = TcpTransport::new();
        t.set_app_uuid(node.app_uuid().to_string());
        Box::new(t) as Box<dyn Transport>
    };
    let connect_task = tokio::spawn(connect_loop(
        node,
        connect,
        peer_rx,
        accepted_rx,
        unsee_tx,
        "tcp",
    ));

    TransportHandles { tasks: vec![listen_task, scan_task, connect_task] }
}
