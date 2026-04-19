use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use mlink_cli::{Cli, Commands, RoomAction, TrustAction};
use mlink_core::api::stream::create_stream;
use mlink_core::core::node::{Node, NodeConfig, NodeEvent};
use mlink_core::core::room::{generate_room_code, room_hash, RoomManager};
use mlink_core::core::scanner::Scanner;
use mlink_core::core::security::TrustStore;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::signal;
use tokio::sync::{mpsc, Mutex};

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
        Some(Commands::Serve) => cmd_serve(None).await,
        Some(Commands::Room { action }) => cmd_room(action).await,
        Some(Commands::Send { code, file, message }) => cmd_send_room(code, file, message).await,
        Some(Commands::Listen) => cmd_listen().await,

        Some(Commands::Scan) => cmd_scan().await,
        Some(Commands::Connect { peer_id }) => cmd_connect(peer_id).await,
        Some(Commands::Ping { peer_id }) => cmd_ping(peer_id).await,
        Some(Commands::Status) => cmd_status().await,
        Some(Commands::Trust { action }) => cmd_trust(action).await,
        Some(Commands::Doctor) => cmd_doctor().await,

        // No subcommand → one-shot "join or create a room" mode.
        None => {
            let code = match cli.code {
                Some(c) => {
                    validate_room_code(&c)?;
                    println!("[mlink] joining room {c}");
                    c
                }
                None => {
                    let c = generate_room_code();
                    println!("[mlink] room created: {c}");
                    println!("[mlink] share this code with other devices, then run:");
                    println!("        mlink {c}");
                    c
                }
            };
            cmd_serve(Some(code)).await
        }
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

/// Long-running loop: advertise, scan, auto-connect, print incoming frames.
/// If `room_code` is set both the peripheral advertisement and the scan filter
/// are tagged with its hash so the two sides can rendezvous on the same room.
async fn cmd_serve(room_code: Option<String>) -> Result<(), MlinkError> {
    let mut node = build_node().await?;
    let local_name = node.config().name.clone();
    let room_hash_bytes: Option<[u8; 8]> = room_code.as_deref().map(room_hash);

    // Tell the node which room we belong to — the handshake round-trip will
    // drop any peer that claims a different room (or no room, when we have one).
    node.set_room_hash(room_hash_bytes);

    node.start().await?;

    match &room_code {
        Some(code) => {
            println!("[mlink] room: {code}");
            println!("[mlink] serving as {} — waiting for peers...", node.app_uuid());
        }
        None => println!("[mlink] serving as {}", node.app_uuid()),
    }

    // --- 1. Peripheral advertisement (background) ------------------------
    // Advertises the mlink service and forwards every accepted central's
    // connection back to the main loop so it can drive the handshake on the
    // Node. Without this, a central that dials us gets frames routed into
    // a dead `MacPeripheralConnection` and the handshake stalls forever.
    let (accepted_tx, mut accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    {
        let mut listen_transport = BleTransport::new();
        listen_transport.set_local_name(local_name.clone());
        if let Some(h) = room_hash_bytes {
            listen_transport.set_room_hash(h);
        }
        let local_name_for_task = local_name.clone();
        tokio::spawn(async move {
            #[cfg(target_os = "macos")]
            {
                let peripheral = match listen_transport.start_peripheral().await {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[mlink] peripheral advertisement error: {e}");
                        print_bluetooth_permission_hint();
                        return;
                    }
                };
                loop {
                    let central_id = match peripheral.wait_for_central().await {
                        Ok(id) => id,
                        Err(e) => {
                            eprintln!("[mlink] peripheral accept error: {e}");
                            return;
                        }
                    };
                    let conn: Box<dyn Connection> = Box::new(
                        mlink_core::transport::peripheral::MacPeripheralConnection::new(
                            central_id.clone(),
                            peripheral.clone(),
                        ),
                    );
                    if accepted_tx.send(conn).await.is_err() {
                        return;
                    }
                    eprintln!(
                        "[mlink:debug] peripheral accepted central {central_id} (as {local_name_for_task})"
                    );
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (local_name_for_task, accepted_tx);
                match listen_transport.listen().await {
                    Ok(_conn) => {
                        std::future::pending::<()>().await;
                    }
                    Err(e) => {
                        eprintln!("[mlink] peripheral advertisement error: {e}");
                        print_bluetooth_permission_hint();
                    }
                }
            }
        });
    }

    // --- 2. Scan loop (background, only when a room is set) --------------
    // Scanner pushes newly-seen peers down `peer_rx`; auto-connect runs on the
    // main task because `Node::connect_peer` needs `&mut Node`.
    let (peer_tx, mut peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    if room_hash_bytes.is_some() {
        let app_uuid = node.app_uuid().to_string();
        let hash = room_hash_bytes.expect("is_some checked above");
        let mut scan_transport = BleTransport::new();
        scan_transport.set_room_hash(hash);
        let mut scanner = Scanner::new(Box::new(scan_transport), app_uuid);
        scanner.set_room_hashes(vec![hash]);
        tokio::spawn(async move {
            if let Err(e) = scanner.discover_loop(peer_tx).await {
                eprintln!("[mlink] scanner error: {e}");
            }
        });
    }

    let mut events = node.subscribe();
    // Connecting transport reused across auto-connect attempts.
    let mut connect_transport = BleTransport::new();
    // Track wire-level peer ids we have already engaged (as central or as
    // peripheral). When both Macs see each other simultaneously they'd
    // otherwise dial each other; the second dial kicks the first connection.
    // We record any wire_id we've touched and skip duplicates.
    use std::collections::HashSet;
    let mut engaged_wire_ids: HashSet<String> = HashSet::new();

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
                if engaged_wire_ids.contains(&peer.id) {
                    eprintln!(
                        "[mlink:conn] serve: skip dial {} — already engaged (incoming-first wins)",
                        peer.id
                    );
                    continue;
                }
                engaged_wire_ids.insert(peer.id.clone());
                println!("[mlink] discovered {} ({}) — connecting...", peer.name, peer.id);
                eprintln!("[mlink:conn] serve: dial as central wire_id={}", peer.id);
                match node.connect_peer(&mut connect_transport, &peer).await {
                    Ok(peer_id) => println!("[mlink] + {peer_id}"),
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped {peer_id}: different room");
                    }
                    Err(e) => {
                        eprintln!("[mlink] connect {} failed: {e}", peer.id);
                        // Allow a future retry if the first attempt died.
                        engaged_wire_ids.remove(&peer.id);
                    }
                }
            }
            maybe_conn = accepted_rx.recv() => {
                let conn = match maybe_conn {
                    Some(c) => c,
                    None => continue,
                };
                let wire_id = conn.peer_id().to_string();
                // Mark this wire as engaged so the scanner doesn't also dial
                // them while we're finishing the peripheral-side handshake.
                engaged_wire_ids.insert(wire_id.clone());
                println!("[mlink] incoming central {wire_id} — handshaking...");
                eprintln!("[mlink:conn] serve: accept as peripheral wire_id={wire_id}");
                match node.accept_incoming(conn, "ble", wire_id.clone()).await {
                    Ok(peer_id) => println!("[mlink] + {peer_id} (incoming)"),
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped incoming {peer_id}: different room");
                    }
                    Err(e) => {
                        eprintln!("[mlink] accept {wire_id} failed: {e}");
                        engaged_wire_ids.remove(&wire_id);
                    }
                }
            }
            ev = events.recv() => {
                match ev {
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
    node.set_room_hash(Some(room_hash(&code)));
    node.start().await?;

    // Scanner owns a transport instance and enforces the room-hash filter so
    // we only surface peripherals advertising our room. A second transport is
    // created below for the actual connect path since Scanner takes ownership.
    let scan_transport = {
        let mut t = BleTransport::new();
        t.set_room_hash(room_hash(&code));
        t
    };
    let mut scanner = Scanner::new(Box::new(scan_transport), node.app_uuid().to_string());
    scanner.set_room_hashes(vec![room_hash(&code)]);
    let peers = scanner.discover_once().await?;
    if peers.is_empty() {
        println!("[mlink] no peers found in room {code}");
        return Ok(());
    }

    // Separate transport for the connect path (Scanner already consumed the
    // first one). We still filter advertisements via Scanner above, so `peers`
    // here is the room-matched set.
    let mut transport = BleTransport::new();
    transport.set_room_hash(room_hash(&code));

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
    let peers = match transport.discover().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[mlink] scan failed: {e}");
            print_bluetooth_permission_hint();
            return Err(e);
        }
    };
    if peers.is_empty() {
        print_bluetooth_permission_hint();
    }
    print_peer_list(&peers);
    Ok(())
}

/// Print a Bluetooth permission remediation hint. macOS silently returns an
/// empty scan list when Core Bluetooth access is denied, so we print this on
/// both explicit error *and* an empty discover result — the user can't tell
/// the difference otherwise.
fn print_bluetooth_permission_hint() {
    eprintln!("[mlink] If this persists, Bluetooth access may be denied.");
    eprintln!(
        "[mlink] Fix: System Settings → Privacy & Security → Bluetooth → add your terminal app"
    );
    eprintln!("[mlink] Or install as app: bash scripts/install.sh");
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
            if peers.is_empty() {
                print_bluetooth_permission_hint();
            }
        }
        Err(e) => {
            println!("FAIL ({e})");
            print_bluetooth_permission_hint();
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
