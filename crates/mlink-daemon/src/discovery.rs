//! Background discovery loop. Started once at daemon boot so every WS client
//! shares a single scanner + accept loop. Join/leave only toggles the set of
//! room hashes the node validates against at handshake time — the scanner
//! itself runs with no room filter so peers from every active room can be
//! surfaced through the same transport.
//!
//! Transport selection is driven by `MLINK_DAEMON_TRANSPORT` (`tcp` default,
//! `ble` when explicitly requested). TCP is the default because it works
//! headlessly in CI and doesn't require macOS Core Bluetooth permissions.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mlink_core::core::node::{Node, NodeEvent};
use mlink_core::core::scanner::Scanner;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::sync::mpsc;

#[cfg(target_os = "macos")]
use mlink_core::transport::ble::BleTransport;

/// Choice of link-layer transport. Parsed from `MLINK_DAEMON_TRANSPORT` —
/// anything other than a literal `"ble"` falls back to TCP so a typo doesn't
/// silently boot a radio the user didn't ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonTransport {
    Tcp,
    Ble,
}

impl DaemonTransport {
    pub fn from_env() -> Self {
        match std::env::var("MLINK_DAEMON_TRANSPORT").as_deref() {
            Ok("ble") => DaemonTransport::Ble,
            _ => DaemonTransport::Tcp,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DaemonTransport::Tcp => "tcp",
            DaemonTransport::Ble => "ble",
        }
    }
}

/// Outer timeout for dial / accept. Mirrors the CLI — long enough that a slow
/// honest peer completes, short enough that a dead socket unwinds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RETRIES: u8 = 3;

/// Spawn the long-lived discovery + auto-connect loop. Returns immediately;
/// the loop runs on tokio tasks for the lifetime of the process. Errors
/// bubble up as tracing warnings — the daemon keeps serving WS regardless.
pub fn spawn(node: Arc<Node>, transport: DaemonTransport) {
    match transport {
        DaemonTransport::Tcp => spawn_tcp(node),
        DaemonTransport::Ble => spawn_ble(node),
    }
    tracing::info!(transport = transport.label(), "discovery loop started");
}

fn spawn_tcp(node: Arc<Node>) {
    let local_name = node.config().name.clone();
    let app_uuid = node.app_uuid().to_string();

    // Peripheral (TCP listener + mDNS advertisement). No room filter — every
    // joined room shares a single listener; handshake does the real filtering.
    let (accepted_tx, accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    let mut listen = TcpTransport::new();
    listen.set_local_name(local_name.clone());
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

fn spawn_ble(node: Arc<Node>) {
    #[cfg(target_os = "macos")]
    {
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
                        central_id.clone(), peripheral.clone(), rx,
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
    #[cfg(not(target_os = "macos"))]
    {
        let _ = node;
        tracing::warn!("BLE daemon transport only supported on macOS; falling back to TCP");
        spawn_tcp(node);
    }
}

/// Shared connect/accept driver. Mirrors the structure of `cmd_serve` in the
/// CLI but is transport-agnostic — the caller hands us the already-configured
/// peer/accept channels + a connect transport.
async fn connect_loop(
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
                        // Drain incoming frames to the broadcast so WS clients
                        // see MessageReceived events.
                        drop(node.spawn_peer_reader(app));
                    }
                    Err(MlinkError::RoomMismatch { .. }) => {
                        // No retry: different room.
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
