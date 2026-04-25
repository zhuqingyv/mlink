//! Interactive chat: same connect/accept plumbing as `cmd_serve`, plus a
//! stdin reader that broadcasts each typed line and a per-peer reader task
//! that surfaces incoming messages as `NodeEvent::MessageReceived` which we
//! print inline with the rest of the event log.

use std::collections::HashMap;

use mlink_core::core::node::NodeEvent;
use mlink_core::core::room::room_hash;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::Transport;
use mlink_cli::TransportKind;
use tokio::signal;

use mlink_cli::conn_tracker::{ConnTracker, MAX_RETRIES};
use crate::node_build::{build_node, transport_label, validate_room_code};
use mlink_cli::role::{decide_ble_role, RoleDecision};
use crate::session::dial::{accept_with_timeout, dial_with_timeout, schedule_retry_unsee};
use crate::session::listen::spawn_listen_task;
use crate::session::scanner::{disabled_scanner, spawn_scanner_task};
use crate::session::stdin::spawn_stdin_task;

pub(crate) async fn cmd_chat(code: String, kind: TransportKind) -> Result<(), MlinkError> {
    validate_room_code(&code)?;

    let node = build_node().await?;
    let local_name = node.config().name.clone();
    let hash = room_hash(&code);
    node.add_room_hash(hash);
    node.start().await?;

    println!("[mlink] chat: room {code}");
    println!(
        "[mlink] chatting as {} via {} — type a line and press Enter to broadcast",
        node.app_uuid(),
        transport_label(kind)
    );

    // --- Peripheral / TCP accept (same pattern as serve) -----------------
    let listen_handle = spawn_listen_task(
        kind,
        local_name.clone(),
        node.app_uuid().to_string(),
        Some(hash),
    );
    let mut accepted_rx = listen_handle.rx;

    // --- Scan loop (room-filtered). BLE host: skip scanner entirely —
    // the joining side does the discovery + dialling.
    let ble_host_only = matches!(kind, TransportKind::Ble);
    let scanner_handle = if ble_host_only {
        disabled_scanner()
    } else {
        spawn_scanner_task(kind, node.app_uuid().to_string(), hash)
    };
    let mut peer_rx = scanner_handle.peer_rx;
    let unsee_tx = scanner_handle.unsee_tx;

    // --- Stdin reader ----------------------------------------------------
    let mut stdin_rx = spawn_stdin_task();

    let mut events = node.subscribe();
    let mut connect_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let mut tracker = ConnTracker::new();
    // Reader-task handles per peer id. Kept so we can abort on disconnect
    // and avoid double-spawning if the same peer_id reconnects.
    let mut readers: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // Pretty names keyed by app_uuid so incoming messages print as
    // `[name] message` instead of `[uuid] message` once the handshake name
    // is known. Falls back to the uuid itself before we've seen the peer.
    let mut peer_names: HashMap<String, String> = HashMap::new();

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                println!("\n[mlink] bye");
                break;
            }
            maybe_line = stdin_rx.recv() => {
                let Some(line) = maybe_line else { continue; };
                // Broadcast to every peer we currently have a connection with.
                let peers = node.peers().await;
                if peers.is_empty() {
                    println!("[mlink] (no peers yet — message dropped)");
                    continue;
                }
                for p in &peers {
                    if let Err(e) = node
                        .send_raw(&p.id, MessageType::Message, line.as_bytes())
                        .await
                    {
                        eprintln!("[mlink] send to {} failed: {e}", p.id);
                    }
                }
            }
            maybe_peer = peer_rx.recv() => {
                let peer = match maybe_peer {
                    Some(p) => p,
                    None => continue,
                };
                // Chat keeps the original silent-skip behaviour: all of
                // inbound / engaged / already-connected-by-app / max-retries
                // short-circuit without logging (the cmd_serve variant logs).
                if tracker.should_skip_dial(&peer.id).is_some() {
                    continue;
                }
                match decide_ble_role(kind, node.app_uuid(), &peer.metadata) {
                    RoleDecision::Skip => {
                        eprintln!(
                            "[mlink:conn] chat: skip dial {} — peer outranks us, waiting for inbound",
                            peer.id
                        );
                        continue;
                    }
                    RoleDecision::Dial => {
                        if matches!(kind, TransportKind::Ble) && peer.metadata.len() == 8 {
                            eprintln!(
                                "[mlink:conn] chat: role=central for {} (we outrank peer)",
                                peer.id
                            );
                        }
                    }
                    RoleDecision::SymmetricFallback => {
                        eprintln!(
                            "[mlink:conn] chat: role tie/invalid for {} — falling back to symmetric dial",
                            peer.id
                        );
                    }
                }
                let attempt_no = match tracker.bump_attempt(&peer.id) {
                    Some(n) => n,
                    None => continue,
                };
                println!(
                    "[mlink] discovered {} ({}) — connecting (attempt {}/{})...",
                    peer.name, peer.id, attempt_no, MAX_RETRIES
                );
                let peer_wire_id = peer.id.clone();
                let peer_wire_name = peer.name.clone();
                let dial_result = dial_with_timeout(&node, connect_transport.as_mut(), &peer).await;
                match dial_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id}");
                        tracker.on_dial_ok(&peer_wire_id, &peer_id);
                        peer_names.insert(peer_id.clone(), peer_wire_name);
                        if !readers.contains_key(&peer_id) {
                            readers.insert(peer_id.clone(), node.spawn_peer_reader(peer_id));
                        }
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped {peer_id}: different room");
                        tracker.mark_room_mismatch_dial(&peer_wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] connect {} failed: {e}", peer_wire_id);
                        tracker.on_dial_err(&peer_wire_id);
                        schedule_retry_unsee(unsee_tx.clone(), peer_wire_id);
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
                println!("[mlink] incoming {wire_id} — handshaking...");
                let accept_result =
                    accept_with_timeout(&node, conn, transport_label(kind), wire_id.clone()).await;
                match accept_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id} (incoming)");
                        tracker.on_accept_ok(&wire_id, &peer_id);
                        peer_names.insert(peer_id.clone(), wire_id.clone());
                        if !readers.contains_key(&peer_id) {
                            readers.insert(peer_id.clone(), node.spawn_peer_reader(peer_id));
                        }
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
                    Ok(NodeEvent::MessageReceived { peer_id, payload }) => {
                        let name = peer_names
                            .get(&peer_id)
                            .cloned()
                            .unwrap_or_else(|| peer_id.clone());
                        let text = match std::str::from_utf8(&payload) {
                            Ok(s) => s.to_string(),
                            Err(_) => format!("<{} bytes binary>", payload.len()),
                        };
                        println!("[{name}] {text}");
                    }
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        println!("[mlink] peer connected: {peer_id}");
                        tracker.on_peer_connected(&peer_id);
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        println!("[mlink] peer disconnected: {peer_id}");
                        tracker.on_peer_disconnected(&peer_id);
                        if let Some(h) = readers.remove(&peer_id) {
                            h.abort();
                        }
                    }
                    Ok(NodeEvent::PeerLost { peer_id }) => {
                        println!("[mlink] peer lost: {peer_id}");
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }

    for (_, h) in readers.drain() {
        h.abort();
    }
    node.stop().await?;
    Ok(())
}
