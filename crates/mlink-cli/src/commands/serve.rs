//! Long-running host loop: advertise, (optionally) scan, auto-connect, print
//! incoming frames. `room_code = None` means "listen-only" — no room filter,
//! no scanner, just accept whoever finds us. With a room code both the
//! peripheral advertisement and the scan filter are tagged with its hash so
//! the two sides rendezvous on the same room.
//!
//! This command shares its select! skeleton with cmd_chat and cmd_join;
//! the shared pieces live in `crate::session::{scanner, listen, dial}` and
//! `crate::conn_tracker::ConnTracker`.

use mlink_core::core::node::NodeEvent;
use mlink_core::core::room::room_hash;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::Transport;
use mlink_cli::TransportKind;
use tokio::signal;

use mlink_cli::conn_tracker::{ConnTracker, SkipReason, MAX_RETRIES};
use crate::node_build::{build_node, transport_label};
use mlink_cli::role::{decide_ble_role, RoleDecision};
use crate::session::dial::{accept_with_timeout, dial_with_timeout, schedule_retry_unsee};
use crate::session::listen::spawn_listen_task;
use crate::session::scanner::{disabled_scanner, spawn_scanner_task};

pub(crate) async fn cmd_serve(
    room_code: Option<String>,
    kind: TransportKind,
) -> Result<(), MlinkError> {
    let node = build_node().await?;
    let local_name = node.config().name.clone();
    let room_hash_bytes: Option<[u8; 8]> = room_code.as_deref().map(room_hash);

    // Tell the node which room we belong to — the handshake round-trip will
    // drop any peer that claims a different room (or no room, when we have one).
    if let Some(h) = room_hash_bytes {
        node.add_room_hash(h);
    }

    node.start().await?;

    match &room_code {
        Some(code) => {
            println!("[mlink] room: {code}");
            println!(
                "[mlink] serving as {} via {} — waiting for peers...",
                node.app_uuid(),
                transport_label(kind)
            );
        }
        None => println!(
            "[mlink] serving as {} via {}",
            node.app_uuid(),
            transport_label(kind)
        ),
    }

    // --- 1. Peripheral advertisement / TCP listener (background) ---------
    let listen_handle = spawn_listen_task(
        kind,
        local_name.clone(),
        node.app_uuid().to_string(),
        room_hash_bytes,
    );
    let mut accepted_rx = listen_handle.rx;

    // --- 2. Scan loop (background, only when a room is set AND transport
    // isn't BLE-host). BLE host is peripheral-only; the joining side does
    // all the scanning / dialling.
    let ble_host_only = matches!(kind, TransportKind::Ble);
    let scanner_handle = if let (Some(hash), false) = (room_hash_bytes, ble_host_only) {
        spawn_scanner_task(kind, node.app_uuid().to_string(), hash)
    } else {
        disabled_scanner()
    };
    let mut peer_rx = scanner_handle.peer_rx;
    let unsee_tx = scanner_handle.unsee_tx;

    let mut events = node.subscribe();
    // Connecting transport reused across auto-connect attempts.
    let mut connect_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let mut tracker = ConnTracker::new();

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                println!("\n[mlink] bye");
                break;
            }
            maybe_peer = peer_rx.recv() => {
                let peer = match maybe_peer {
                    Some(p) => p,
                    None => continue,
                };
                match tracker.should_skip_dial(&peer.id) {
                    Some(SkipReason::AlreadyInbound) => {
                        eprintln!(
                            "[mlink:conn] serve: skip dial {} — already connected inbound",
                            peer.id
                        );
                        continue;
                    }
                    Some(SkipReason::AlreadyEngaged) => {
                        eprintln!(
                            "[mlink:conn] serve: skip dial {} — already engaged",
                            peer.id
                        );
                        continue;
                    }
                    Some(SkipReason::AlreadyConnected { app_uuid }) => {
                        eprintln!(
                            "[mlink:conn] serve: skip dial {} — app_uuid {} already connected",
                            peer.id, app_uuid
                        );
                        continue;
                    }
                    Some(SkipReason::MaxRetries { attempts }) => {
                        eprintln!(
                            "[mlink:conn] serve: giving up on {} after {} attempts",
                            peer.id, attempts
                        );
                        continue;
                    }
                    None => {}
                }
                // BLE symmetric-dial guard (see decide_ble_role). `None` peer
                // metadata / tie falls back to symmetric dial so nothing hangs.
                match decide_ble_role(kind, node.app_uuid(), &peer.metadata) {
                    RoleDecision::Skip => {
                        eprintln!(
                            "[mlink:conn] serve: skip dial {} — peer outranks us, waiting for inbound",
                            peer.id
                        );
                        continue;
                    }
                    RoleDecision::Dial => {
                        if matches!(kind, TransportKind::Ble) && peer.metadata.len() == 8 {
                            eprintln!(
                                "[mlink:conn] serve: role=central for {} (we outrank peer)",
                                peer.id
                            );
                        }
                    }
                    RoleDecision::SymmetricFallback => {
                        eprintln!(
                            "[mlink:conn] serve: role tie/invalid for {} — falling back to symmetric dial",
                            peer.id
                        );
                    }
                }
                let attempt_no = match tracker.bump_attempt(&peer.id) {
                    Some(n) => n,
                    None => {
                        eprintln!(
                            "[mlink:conn] serve: giving up on {} after {} attempts",
                            peer.id,
                            tracker.attempts(&peer.id)
                        );
                        continue;
                    }
                };
                println!(
                    "[mlink] discovered {} ({}) — connecting (attempt {}/{})...",
                    peer.name, peer.id, attempt_no, MAX_RETRIES
                );
                eprintln!(
                    "[mlink:conn] serve: dial as central wire_id={} attempt={}",
                    peer.id, attempt_no
                );
                let peer_wire_id = peer.id.clone();
                let dial_result = dial_with_timeout(&node, connect_transport.as_mut(), &peer).await;
                match dial_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id}");
                        tracker.on_dial_ok(&peer_wire_id, &peer_id);
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped {peer_id}: different room");
                        tracker.mark_room_mismatch_dial(&peer_wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] connect {} failed: {e}", peer_wire_id);
                        tracker.on_dial_err(&peer_wire_id);
                        schedule_retry_unsee(unsee_tx.clone(), peer_wire_id.clone());
                        eprintln!(
                            "[mlink:conn] serve: will retry {} after random backoff",
                            peer_wire_id
                        );
                    }
                }
            }
            maybe_conn = accepted_rx.recv() => {
                let conn = match maybe_conn {
                    Some(c) => c,
                    None => continue,
                };
                let wire_id = conn.peer_id().to_string();
                tracker.on_accept_begin(&wire_id);
                println!("[mlink] incoming central {wire_id} — handshaking...");
                eprintln!("[mlink:conn] serve: accept as peripheral wire_id={wire_id}");
                let accept_result =
                    accept_with_timeout(&node, conn, transport_label(kind), wire_id.clone()).await;
                match accept_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id} (incoming)");
                        tracker.on_accept_ok(&wire_id, &peer_id);
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped incoming {peer_id}: different room");
                        tracker.on_accept_err(&wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] accept {wire_id} failed: {e}");
                        tracker.on_accept_err(&wire_id);
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        println!("[mlink] peer connected: {peer_id}");
                        tracker.on_peer_connected(&peer_id);
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        println!("[mlink] peer disconnected: {peer_id}");
                        tracker.on_peer_disconnected(&peer_id);
                    }
                    Ok(NodeEvent::PeerLost { peer_id }) => {
                        println!("[mlink] peer lost: {peer_id}");
                    }
                    Ok(_) => {}
                    // Broadcast lag / sender-dropped → fall through; the main
                    // loop continues until ctrl-C.
                    Err(_) => {}
                }
            }
        }
    }

    node.stop().await?;
    Ok(())
}
