//! Joining-side entrypoint. Runs scanner + dial only — no peripheral
//! advertisement, no accept loop. Pairs with the host running `mlink <code>`
//! (or `mlink chat <code>`) on the other device. When `chat` is true we also
//! wire up a stdin reader and spawn per-peer message readers so typed lines
//! broadcast and inbound messages print inline (same shape as `cmd_chat`).

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

use mlink_cli::conn_tracker::{ConnTracker, SkipReason, MAX_RETRIES};
use crate::node_build::{build_node, transport_label, validate_room_code};
use crate::session::dial::{dial_with_timeout, schedule_retry_unsee};
use crate::session::scanner::spawn_scanner_task;
use crate::session::stdin::{disabled_stdin, spawn_stdin_task};

pub(crate) async fn cmd_join(
    code: String,
    chat: bool,
    kind: TransportKind,
) -> Result<(), MlinkError> {
    validate_room_code(&code)?;

    let node = build_node().await?;
    let hash = room_hash(&code);
    node.add_room_hash(hash);
    node.start().await?;

    println!("[mlink] joining room {code}");
    println!(
        "[mlink] joining as {} via {} — scanning for host...",
        node.app_uuid(),
        transport_label(kind)
    );
    if chat {
        println!("[mlink] chat mode: type a line and press Enter to broadcast");
    }

    // --- Scan loop (room-filtered) ---------------------------------------
    let scanner_handle = spawn_scanner_task(kind, node.app_uuid().to_string(), hash);
    let mut peer_rx = scanner_handle.peer_rx;
    let unsee_tx = scanner_handle.unsee_tx;

    // --- Optional stdin reader (chat mode) ------------------------------
    let mut stdin_rx = if chat {
        spawn_stdin_task()
    } else {
        disabled_stdin()
    };

    let mut events = node.subscribe();
    let mut connect_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let mut tracker = ConnTracker::new();
    let mut readers: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut peer_names: HashMap<String, String> = HashMap::new();

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                println!("\n[mlink] bye");
                break;
            }
            maybe_line = stdin_rx.recv() => {
                // `stdin_rx` is closed when `chat == false`; recv() returns None
                // on every tick and the arm becomes inert, which is the intent.
                let Some(line) = maybe_line else { continue; };
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
                // In join mode we always dial — no role arbitration is needed
                // because the host deliberately does not dial back. This is
                // the whole point of the host/join split. We still consult
                // the tracker so duplicate scan hits / already-connected
                // peers don't retrigger the dial.
                match tracker.should_skip_dial(&peer.id) {
                    Some(SkipReason::MaxRetries { attempts }) => {
                        eprintln!(
                            "[mlink:conn] join: giving up on {} after {} attempts",
                            peer.id, attempts
                        );
                        continue;
                    }
                    Some(_) => continue,
                    None => {}
                }
                let attempt_no = match tracker.bump_attempt(&peer.id) {
                    Some(n) => n,
                    None => {
                        eprintln!(
                            "[mlink:conn] join: giving up on {} after {} attempts",
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
                let peer_wire_id = peer.id.clone();
                let peer_wire_name = peer.name.clone();
                let dial_result = dial_with_timeout(&node, connect_transport.as_mut(), &peer).await;
                match dial_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id}");
                        tracker.on_dial_ok(&peer_wire_id, &peer_id);
                        peer_names.insert(peer_id.clone(), peer_wire_name);
                        if chat && !readers.contains_key(&peer_id) {
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
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::MessageReceived { peer_id, payload }) if chat => {
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
