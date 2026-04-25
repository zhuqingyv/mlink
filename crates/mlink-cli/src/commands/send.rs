//! `mlink send <code> [msg] [--file <path>]` — short-lived broadcast. Bring
//! up a node, scan once for peers in the given room, connect to each, then
//! send either a text frame or (when `--file` is present) punt to the not-
//! yet-implemented file-transfer stub.

use std::path::PathBuf;

use mlink_core::core::node::Node;
use mlink_core::core::room::room_hash;
use mlink_core::core::scanner::Scanner;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::Transport;

use crate::node_build::{build_node, validate_room_code};
use mlink_cli::TransportKind;

pub(crate) async fn cmd_send_room(
    code: String,
    file: Option<PathBuf>,
    message: Option<String>,
    kind: TransportKind,
) -> Result<(), MlinkError> {
    validate_room_code(&code)?;
    if file.is_none() && message.is_none() {
        return Err(MlinkError::HandlerError(
            "send: provide either a <message> or --file <path>".into(),
        ));
    }

    let node = build_node().await?;
    node.add_room_hash(room_hash(&code));
    node.start().await?;

    // Scanner owns a transport instance and enforces the room-hash filter so
    // we only surface peripherals advertising our room. A second transport is
    // created below for the actual connect path since Scanner takes ownership.
    let scan_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => {
            let mut t = BleTransport::new();
            t.set_room_hash(room_hash(&code));
            Box::new(t)
        }
        TransportKind::Tcp => {
            let mut t = TcpTransport::new();
            t.set_room_hash(room_hash(&code));
            Box::new(t)
        }
    };
    let mut scanner = Scanner::new(scan_transport, node.app_uuid().to_string());
    scanner.set_room_hashes(vec![room_hash(&code)]);
    let peers = scanner.discover_once().await?;
    if peers.is_empty() {
        println!("[mlink] no peers found in room {code}");
        return Ok(());
    }

    // Separate transport for the connect path (Scanner already consumed the
    // first one). We still filter advertisements via Scanner above, so `peers`
    // here is the room-matched set.
    let mut transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => {
            let mut t = BleTransport::new();
            t.set_room_hash(room_hash(&code));
            Box::new(t)
        }
        TransportKind::Tcp => {
            let mut t = TcpTransport::new();
            t.set_room_hash(room_hash(&code));
            Box::new(t)
        }
    };

    // Connect to every discovered peer in the room, then broadcast.
    let mut connected: Vec<String> = Vec::new();
    for p in &peers {
        match node.connect_peer(transport.as_mut(), p).await {
            Ok(peer_id) => connected.push(peer_id),
            Err(e) => eprintln!("[mlink] failed to connect {}: {e}", p.id),
        }
    }

    if connected.is_empty() {
        return Err(MlinkError::HandlerError(
            "send: could not connect to any peer in the room".into(),
        ));
    }

    if let Some(path) = file {
        send_file_to_peers(&node, &connected, &path).await?;
    } else if let Some(msg) = message {
        for peer_id in &connected {
            node.send_raw(peer_id, MessageType::Message, msg.as_bytes())
                .await?;
            println!("[mlink] sent {} bytes to {peer_id}", msg.len());
        }
    }
    Ok(())
}

async fn send_file_to_peers(
    _node: &Node,
    _peers: &[String],
    _path: &PathBuf,
) -> Result<(), MlinkError> {
    Err(MlinkError::HandlerError(
        "file transfer not yet implemented; use `mlink send <code> <text>` for now".into(),
    ))
}
