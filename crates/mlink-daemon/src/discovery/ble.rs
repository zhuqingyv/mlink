//! BLE discovery flavor (macOS only). Mirrors TCP but uses Core Bluetooth —
//! peripheral starts a GATT service advertising a room UUID, scanner listens
//! for peer adverts on the same UUID. Every connection still goes through the
//! shared `connect_loop` so bookkeeping (dedup, retry, unsee) is identical.

#![cfg(target_os = "macos")]

use std::sync::Arc;

use mlink_core::core::node::Node;
use mlink_core::core::scanner::Scanner;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::sync::mpsc;

use super::connect_loop;

pub(super) fn spawn_ble(node: Arc<Node>) {
    let local_name = node.config().name.clone();
    let app_uuid = node.app_uuid().to_string();

    let (accepted_tx, accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    let mut listen = BleTransport::new();
    listen.set_local_name(local_name);
    listen.set_app_uuid(app_uuid.clone());
    // No room_hash on the peripheral: the advertised service UUID encodes a
    // room, so a daemon that joins multiple rooms would have to re-advertise
    // on each `join`. Skip that complexity — peers advertising *their* room
    // will still rendezvous because handshake verification runs on both ends.
    tokio::spawn(async move {
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
    tokio::spawn(async move {
        if let Err(e) = scanner.discover_loop(peer_tx).await {
            tracing::warn!(error = %e, "ble scanner exited");
        }
    });

    let connect = Box::new(BleTransport::new()) as Box<dyn Transport>;
    tokio::spawn(connect_loop(node, connect, peer_rx, accepted_rx, unsee_tx, "ble"));
}
