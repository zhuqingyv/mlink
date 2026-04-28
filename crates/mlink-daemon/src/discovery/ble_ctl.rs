//! Controlled variant of `ble::spawn_ble`: platform-gated and fallible at the
//! entry point so the outer controller can treat "BLE unavailable" as a
//! graceful degradation rather than a panic. On non-macOS hosts this always
//! returns `None`; on macOS it returns the set of task handles so runtime
//! disable can abort them cleanly.

use std::sync::Arc;

use mlink_core::core::node::Node;

use super::TransportHandles;

#[cfg(target_os = "macos")]
pub(super) fn spawn_ble_controlled(node: Arc<Node>) -> Option<TransportHandles> {
    Some(spawn_macos(node))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn spawn_ble_controlled(_node: Arc<Node>) -> Option<TransportHandles> {
    tracing::warn!("ble daemon transport is only supported on macOS");
    None
}

#[cfg(target_os = "macos")]
fn spawn_macos(node: Arc<Node>) -> TransportHandles {
    use mlink_core::core::scanner::Scanner;
    use mlink_core::transport::ble::BleTransport;
    use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
    use tokio::sync::mpsc;

    use super::connect_loop;

    let local_name = node.config().name.clone();
    let app_uuid = node.app_uuid().to_string();

    let (accepted_tx, accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    let mut listen = BleTransport::new();
    listen.set_local_name(local_name);
    listen.set_app_uuid(app_uuid.clone());
    let listen_task = tokio::spawn(async move {
        let peripheral = match listen.start_peripheral().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "ble peripheral start failed");
                return;
            }
        };
        loop {
            let (central_id, rx) = match peripheral.wait_for_central().await {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::warn!(error = %e, "ble peripheral accept failed");
                    return;
                }
            };
            let conn: Box<dyn Connection> = Box::new(
                mlink_core::transport::peripheral::MacPeripheralConnection::new(
                    central_id.clone(),
                    peripheral.clone(),
                    rx,
                ),
            );
            if accepted_tx.send(conn).await.is_err() {
                return;
            }
        }
    });

    let (peer_tx, peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    let scan = BleTransport::new();
    let mut scanner = Scanner::new(Box::new(scan), app_uuid);
    scanner.set_unsee_channel(unsee_rx);
    let scan_task = tokio::spawn(async move {
        if let Err(e) = scanner.discover_loop(peer_tx).await {
            tracing::warn!(error = %e, "ble scanner exited");
        }
    });

    let connect = Box::new(BleTransport::new()) as Box<dyn Transport>;
    let connect_task =
        tokio::spawn(connect_loop(node, connect, peer_rx, accepted_rx, unsee_tx, "ble"));

    TransportHandles { tasks: vec![listen_task, scan_task, connect_task] }
}
