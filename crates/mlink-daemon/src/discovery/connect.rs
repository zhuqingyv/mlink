//! Transport-agnostic connect/accept driver shared by the TCP and BLE spawners.
//! Handles dedup across already-engaged peers, retry budget, and the unsee
//! signal the scanner needs to re-surface a peer after a failed dial.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mlink_core::core::node::{Node, NodeEvent};
use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::sync::mpsc;

/// Outer timeout for dial / accept. Mirrors the CLI — long enough that a slow
/// honest peer completes, short enough that a dead socket unwinds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RETRIES: u8 = 3;

/// Shared connect/accept driver. Mirrors the structure of `cmd_serve` in the
/// CLI but is transport-agnostic — the caller hands us the already-configured
/// peer/accept channels + a connect transport.
pub async fn connect_loop(
    node: Arc<Node>,
    mut connect_transport: Box<dyn Transport>,
    mut peer_rx: mpsc::Receiver<DiscoveredPeer>,
    mut accepted_rx: mpsc::Receiver<Box<dyn Connection>>,
    unsee_tx: mpsc::Sender<String>,
    transport_label: &'static str,
) {
    let mut engaged: HashSet<String> = HashSet::new();
    let mut attempts: HashMap<String, u8> = HashMap::new();
    let mut connected_inbound: HashSet<String> = HashSet::new();
    let mut connected_peers: HashSet<String> = HashSet::new();
    let mut wire_to_app: HashMap<String, String> = HashMap::new();
    let mut events = node.subscribe();

    loop {
        tokio::select! {
            maybe_peer = peer_rx.recv() => {
                let Some(peer) = maybe_peer else { return; };
                if connected_inbound.contains(&peer.id) || engaged.contains(&peer.id) {
                    continue;
                }
                if let Some(app) = wire_to_app.get(&peer.id) {
                    if connected_peers.contains(app) {
                        continue;
                    }
                }
                let attempt = attempts.entry(peer.id.clone()).or_insert(0);
                if *attempt >= MAX_RETRIES {
                    continue;
                }
                *attempt += 1;
                engaged.insert(peer.id.clone());
                let wire_id = peer.id.clone();
                let dial = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.connect_peer(connect_transport.as_mut(), &peer),
                )
                .await;
                let dial = match dial {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "connect to {wire_id} timed out"
                    ))),
                };
                match dial {
                    Ok(app) => {
                        attempts.remove(&wire_id);
                        wire_to_app.insert(wire_id.clone(), app.clone());
                        connected_peers.insert(app.clone());
                        drop(node.spawn_peer_reader(app));
                    }
                    Err(MlinkError::RoomMismatch { .. }) => {
                        attempts.insert(wire_id.clone(), MAX_RETRIES);
                        engaged.remove(&wire_id);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, peer = %wire_id, "connect failed");
                        engaged.remove(&wire_id);
                        let unsee = unsee_tx.clone();
                        let id = wire_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            let _ = unsee.send(id).await;
                        });
                    }
                }
            }
            maybe_conn = accepted_rx.recv() => {
                let Some(conn) = maybe_conn else { return; };
                let wire_id = conn.peer_id().to_string();
                engaged.insert(wire_id.clone());
                connected_inbound.insert(wire_id.clone());
                let accept = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.accept_incoming(conn, transport_label, wire_id.clone()),
                )
                .await;
                let accept = match accept {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "accept from {wire_id} timed out"
                    ))),
                };
                match accept {
                    Ok(app) => {
                        attempts.remove(&wire_id);
                        wire_to_app.insert(wire_id.clone(), app.clone());
                        connected_peers.insert(app.clone());
                        drop(node.spawn_peer_reader(app));
                    }
                    Err(MlinkError::RoomMismatch { .. }) => {
                        engaged.remove(&wire_id);
                        connected_inbound.remove(&wire_id);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, peer = %wire_id, "accept failed");
                        engaged.remove(&wire_id);
                        connected_inbound.remove(&wire_id);
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        connected_peers.insert(peer_id);
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        // Drop bookkeeping for the now-dead peer and tell the
                        // scanner to forget every wire_id that mapped to this
                        // app_uuid. Without the unsee the scanner's `seen`
                        // set keeps the stale wire_id → the peer never
                        // re-surfaces when it comes back online, so we can
                        // never re-dial. `engaged` / `connected_inbound` also
                        // need clearing so a fresh advertisement from the
                        // same wire_id isn't silently dropped.
                        connected_peers.remove(&peer_id);
                        let dead_wires: Vec<String> = wire_to_app
                            .iter()
                            .filter_map(|(w, a)| (a == &peer_id).then(|| w.clone()))
                            .collect();
                        wire_to_app.retain(|_, a| a != &peer_id);
                        for wire in dead_wires {
                            engaged.remove(&wire);
                            connected_inbound.remove(&wire);
                            attempts.remove(&wire);
                            let _ = unsee_tx.try_send(wire);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }
}
