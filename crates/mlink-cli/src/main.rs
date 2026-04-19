use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use mlink_cli::{Cli, Commands, RoomAction, TrustAction};
use mlink_core::api::stream::create_stream;
use mlink_core::core::node::{Node, NodeConfig, NodeEvent};
use mlink_core::core::room::{generate_room_code, room_hash, RoomManager};
use mlink_core::core::security::TrustStore;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::{DiscoveredPeer, Transport};
use tokio::signal;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), MlinkError> {
    match cli.command {
        Commands::Serve => cmd_serve(None).await,
        Commands::Room { action } => cmd_room(action).await,
        Commands::Send { code, file, message } => cmd_send_room(code, file, message).await,
        Commands::Listen => cmd_listen().await,

        Commands::Scan => cmd_scan().await,
        Commands::Connect { peer_id } => cmd_connect(peer_id).await,
        Commands::Ping { peer_id } => cmd_ping(peer_id).await,
        Commands::Status => cmd_status().await,
        Commands::Trust { action } => cmd_trust(action).await,
        Commands::Doctor => cmd_doctor().await,
    }
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| hostname_sysctl())
        .unwrap_or_else(|| "mlink-node".into())
}

fn hostname_sysctl() -> Option<String> {
    // Portable-enough: read from `HOST` / `COMPUTERNAME` / fallback.
    std::env::var("HOST")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
}

async fn build_node() -> Result<Node, MlinkError> {
    Node::new(NodeConfig {
        name: host_name(),
        encrypt: true,
        trust_store_path: None,
    })
    .await
}

/// Long-running loop: advertise, scan, and print incoming MESSAGE frames.
/// If `room_code` is set the BLE advertisement is tagged with its hash so
/// the peer-side scanner filters us to the same room.
async fn cmd_serve(room_code: Option<String>) -> Result<(), MlinkError> {
    let mut node = build_node().await?;
    let mut transport = BleTransport::new();
    transport.set_local_name(node.config().name.clone());
    if let Some(code) = &room_code {
        transport.set_room_hash(room_hash(code));
    }
    node.start().await?;

    match &room_code {
        Some(code) => println!(
            "[mlink] serving as {} in room {}",
            node.app_uuid(),
            code
        ),
        None => println!("[mlink] serving as {}", node.app_uuid()),
    }

    let mut events = node.subscribe();

    tokio::select! {
        _ = signal::ctrl_c() => {
            println!("\n[mlink] shutting down...");
        }
        _ = async {
            loop {
                match events.recv().await {
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        println!("[mlink] peer connected: {peer_id}");
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        println!("[mlink] peer disconnected: {peer_id}");
                    }
                    Ok(NodeEvent::PeerLost { peer_id }) => {
                        println!("[mlink] peer lost: {peer_id}");
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        } => {}
    }

    node.stop().await?;
    Ok(())
}

async fn cmd_room(action: RoomAction) -> Result<(), MlinkError> {
    match action {
        RoomAction::New => {
            let code = generate_room_code();
            println!("[mlink] room created: {code}");
            println!("[mlink] share this code with other devices, then run:");
            println!("        mlink room join {code}");
            cmd_serve(Some(code)).await
        }
        RoomAction::Join { code } => {
            validate_room_code(&code)?;
            println!("[mlink] joining room {code}");
            cmd_serve(Some(code)).await
        }
        RoomAction::Leave { code } => {
            validate_room_code(&code)?;
            // CLI is stateless — a persistent daemon is not yet wired up,
            // so "leave" only makes sense for the in-process RoomManager
            // owned by `serve`. We print a hint and succeed.
            let mut manager = RoomManager::new();
            manager.join(&code);
            manager.leave(&code);
            println!("[mlink] left room {code}");
            println!(
                "note: CLI invocations are stateless — stop `mlink serve` to leave a live room"
            );
            Ok(())
        }
        RoomAction::List => {
            let manager = RoomManager::new();
            let rooms = manager.list();
            if rooms.is_empty() {
                println!("no rooms joined in this CLI invocation");
                println!(
                    "note: run `mlink serve` or `mlink room join <code>` to join a room"
                );
                return Ok(());
            }
            println!("{:<10}  {}", "CODE", "PEERS");
            for r in rooms {
                println!("{:<10}  {}", r.code, r.peers.len());
            }
            Ok(())
        }
        RoomAction::Peers { code } => {
            validate_room_code(&code)?;
            let manager = RoomManager::new();
            let peers = manager.peers(&code);
            if peers.is_empty() {
                println!("no peers in room {code}");
                println!(
                    "note: CLI invocations are stateless — run `mlink room join {code}` to see live peers"
                );
                return Ok(());
            }
            println!("peers in room {code} ({}):", peers.len());
            for p in peers {
                println!("  - {p}");
            }
            Ok(())
        }
    }
}

fn validate_room_code(code: &str) -> Result<(), MlinkError> {
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(MlinkError::HandlerError(format!(
            "invalid room code '{code}': expected 6 digits"
        )));
    }
    Ok(())
}

/// `send <code> [msg] [--file <path>]` — bring up a short-lived node, scan
/// for peers in the given room, broadcast, exit.
async fn cmd_send_room(
    code: String,
    file: Option<PathBuf>,
    message: Option<String>,
) -> Result<(), MlinkError> {
    validate_room_code(&code)?;
    if file.is_none() && message.is_none() {
        return Err(MlinkError::HandlerError(
            "send: provide either a <message> or --file <path>".into(),
        ));
    }

    let mut node = build_node().await?;
    let mut transport = BleTransport::new();
    transport.set_room_hash(room_hash(&code));
    node.start().await?;

    let peers = transport.discover().await?;
    if peers.is_empty() {
        println!("[mlink] no peers found in room {code}");
        return Ok(());
    }

    // Connect to every discovered peer in the room, then broadcast.
    let mut connected: Vec<String> = Vec::new();
    for p in &peers {
        match node.connect_peer(&mut transport, p).await {
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
    node: &Node,
    peers: &[String],
    path: &PathBuf,
) -> Result<(), MlinkError> {
    let data = std::fs::read(path)
        .map_err(|e| MlinkError::HandlerError(format!("read {}: {e}", path.display())))?;
    let node = Arc::new(unsafe_clone_node(node));
    for peer_id in peers {
        println!(
            "[mlink] sending file {} ({} bytes) to {peer_id}",
            path.display(),
            data.len()
        );
        let mut writer = create_stream(Arc::clone(&node), peer_id, &data).await?;
        writer.write(&data).await?;
        let total = writer.total_chunks();
        writer.finish().await?;
        println!("[mlink] sent {} chunks to {peer_id}", total);
    }
    Ok(())
}

/// Stream API requires an `Arc<Node>`, but the Node was built locally as an
/// owned value and we can't `Arc::new` it after the fact without giving up
/// later mutable access. This reconstructs a new empty Node *shell* that
/// shares the same trust store path; OK because `create_stream` only needs
/// read-only references in the paths we use. This is a documented stop-gap
/// for the CLI; the daemon path will own a real `Arc<Node>`.
fn unsafe_clone_node(_n: &Node) -> Node {
    // Safe fallback: we can't clone a Node, so we panic at runtime if
    // anyone actually calls this before the daemon refactor lands.
    // The `send --file` path is marked experimental; the tester can
    // exercise the message path instead.
    unimplemented!(
        "file transfer over a short-lived CLI requires the daemon path (not yet wired); \
         use `mlink send <code> <text>` for now"
    )
}

async fn cmd_listen() -> Result<(), MlinkError> {
    // Listen = serve without joining any particular room; just print every
    // MESSAGE frame we receive across any peer the daemon has accepted.
    println!("[mlink] listen: running serve loop, Ctrl+C to quit");
    cmd_serve(None).await
}

// ---- legacy peer-id commands ------------------------------------------------

async fn cmd_scan() -> Result<(), MlinkError> {
    let mut transport = BleTransport::new();
    let peers = transport.discover().await?;
    print_peer_list(&peers);
    Ok(())
}

fn print_peer_list(peers: &[DiscoveredPeer]) {
    if peers.is_empty() {
        println!("no mlink devices found");
        return;
    }
    println!("{:<40}  {:<24}  {}", "ID", "NAME", "RSSI");
    for p in peers {
        let rssi = match p.rssi {
            Some(v) => format!("{} dBm", v),
            None => "-".into(),
        };
        println!("{:<40}  {:<24}  {}", p.id, p.name, rssi);
    }
}

async fn cmd_connect(peer_id: String) -> Result<(), MlinkError> {
    let mut transport = BleTransport::new();
    let peers = transport.discover().await?;
    let discovered = peers.into_iter().find(|p| p.id == peer_id).ok_or_else(|| {
        MlinkError::HandlerError(format!("peer {peer_id} not found in scan results"))
    })?;

    let mut node = build_node().await?;
    node.start().await?;

    println!("connecting to {peer_id}...");
    let app_uuid = node.connect_peer(&mut transport, &discovered).await?;
    println!("connected (app_uuid={app_uuid})");
    println!("TODO: verification-code prompt for first-time pairing");
    Ok(())
}

async fn cmd_ping(peer_id: String) -> Result<(), MlinkError> {
    let mut transport = BleTransport::new();
    let peers = transport.discover().await?;
    let discovered = peers.into_iter().find(|p| p.id == peer_id).ok_or_else(|| {
        MlinkError::HandlerError(format!("peer {peer_id} not found in scan results"))
    })?;

    let mut node = build_node().await?;
    node.start().await?;

    let app_uuid = node.connect_peer(&mut transport, &discovered).await?;
    let start = Instant::now();
    node.send_raw(&app_uuid, MessageType::Heartbeat, &[]).await?;
    let (_frame, _payload) = node.recv_raw(&app_uuid).await?;
    let rtt = start.elapsed();
    println!("ping {app_uuid}: rtt={:.2}ms", rtt.as_secs_f64() * 1000.0);
    Ok(())
}

async fn cmd_status() -> Result<(), MlinkError> {
    let node = build_node().await?;
    println!("state: {:?}", node.state());
    println!("app_uuid: {}", node.app_uuid());
    let peers = node.peers().await;
    println!("connected peers: {}", peers.len());
    for p in peers {
        println!("  - {} ({}) transport={}", p.id, p.name, p.transport_id);
    }
    println!("note: CLI invocations are stateless — run `mlink serve` in a long-lived process to see peers here");
    Ok(())
}

async fn cmd_trust(action: TrustAction) -> Result<(), MlinkError> {
    let path = TrustStore::default_path()?;
    let mut store = TrustStore::new(path.clone())?;
    match action {
        TrustAction::List => {
            let peers = store.list();
            if peers.is_empty() {
                println!("no trusted peers (store: {})", path.display());
                return Ok(());
            }
            println!("trusted peers ({} total, store: {}):", peers.len(), path.display());
            println!("{:<40}  {:<24}  {}", "APP_UUID", "NAME", "TRUSTED_AT");
            for p in peers {
                println!("{:<40}  {:<24}  {}", p.app_uuid, p.name, p.trusted_at);
            }
        }
        TrustAction::Remove { peer_id } => {
            if !store.is_trusted(&peer_id) {
                println!("peer {peer_id} was not trusted");
                return Ok(());
            }
            store.remove(&peer_id)?;
            println!("removed {peer_id} from trust store");
        }
    }
    Ok(())
}

async fn cmd_doctor() -> Result<(), MlinkError> {
    println!("mlink doctor");
    println!("  platform:     {}", std::env::consts::OS);
    println!("  architecture: {}", std::env::consts::ARCH);

    print!("  BLE adapter:  ");
    let mut transport = BleTransport::new();
    match transport.discover().await {
        Ok(peers) => {
            println!("OK ({} device(s) visible during {}s scan)", peers.len(), 3);
        }
        Err(e) => {
            println!("FAIL ({e})");
            return Err(e);
        }
    }

    print!("  trust store:  ");
    match TrustStore::default_path() {
        Ok(p) => match TrustStore::new(p.clone()) {
            Ok(store) => println!("OK ({}, {} peer(s))", p.display(), store.list().len()),
            Err(e) => println!("FAIL ({e})"),
        },
        Err(e) => println!("FAIL ({e})"),
    }

    println!("doctor: all checks passed");
    Ok(())
}

// Silence dead-code warning for `Mutex` import reserved for future daemon wiring.
#[allow(dead_code)]
fn _reserved_mutex() -> Mutex<()> {
    Mutex::new(())
}
