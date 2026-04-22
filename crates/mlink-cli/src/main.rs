use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use mlink_cli::{Cli, Commands, RoomAction, TransportKind, TrustAction};
use mlink_core::core::node::{Node, NodeConfig, NodeEvent};
use mlink_core::core::room::{generate_room_code, room_hash, RoomManager};
use mlink_core::core::scanner::Scanner;
use mlink_core::core::security::TrustStore;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::{probe_mdns_daemon, TcpTransport};
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::signal;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let cli = Cli::parse();

    // `mlink daemon` wants graceful shutdown so it can clean up
    // `~/.mlink/daemon.json` — the outer `_exit(0)` path below would skip that.
    // Dispatch straight to `mlink_daemon::run()`, which owns its own ctrl-c
    // select and runs `remove_daemon_info` before returning.
    if matches!(cli.command, Some(Commands::Daemon)) {
        return match mlink_daemon::run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: daemon: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Outer Ctrl+C guard. Long-running subcommands (serve/chat/join) each have
    // their own `select!` on `signal::ctrl_c()` for a graceful goodbye, but a
    // handful of paths in the transport layer can wedge cancellation — the BLE
    // FFI drop path has swallowed SIGINT in the wild, and the mDNS
    // `spawn_blocking(browse)` holds an OS thread for up to 3s before the
    // runtime can unwind. To keep `Ctrl+C` feeling instant no matter what, we
    // race the command future against ctrl-c at the top and short-circuit via
    // `libc::_exit` the moment the signal arrives. Any child tasks,
    // spawn_blocking workers, or Obj-C objects get cleaned up by the OS on
    // process teardown — safe for a one-shot CLI.
    tokio::select! {
        // `biased` makes the ctrl-c arm poll first each tick, so a SIGINT
        // arriving while `run` is still inside one of its own `select!`s wins
        // unconditionally — we don't want to race the inner ctrl-c arm into a
        // slow `node.stop()` + runtime drop teardown that the whole point of
        // this outer guard is to bypass.
        biased;
        _ = signal::ctrl_c() => {
            println!("\n[mlink] bye");
            // Flush stdout so the goodbye line actually lands before exit —
            // pipes are block-buffered, and `_exit` skips libc cleanup that
            // would otherwise flush us.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            // `std::process::exit` runs atexit handlers and drops the Tokio
            // runtime, which waits for spawn_blocking workers (the mDNS
            // daemon loop in particular holds a thread for up to 3s). Hop
            // straight to `_exit` to give Ctrl+C an immediate response —
            // OS resource reclamation handles the rest.
            unsafe { libc::_exit(0) };
        }
        result = run(cli) => match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

async fn run(cli: Cli) -> Result<(), MlinkError> {
    let kind = TransportKind::parse(&cli.transport).map_err(MlinkError::HandlerError)?;
    match cli.command {
        Some(Commands::Serve) => cmd_serve(None, kind).await,
        Some(Commands::Room { action }) => cmd_room(action, kind).await,
        Some(Commands::Send { code, file, message }) => {
            cmd_send_room(code, file, message, kind).await
        }
        Some(Commands::Listen) => cmd_listen(kind).await,
        Some(Commands::Chat { code }) => cmd_chat(code, kind).await,
        Some(Commands::Join { code, chat }) => cmd_join(code, chat, kind).await,

        Some(Commands::Scan) => cmd_scan(kind).await,
        Some(Commands::Connect { peer_id }) => cmd_connect(peer_id, kind).await,
        Some(Commands::Ping { peer_id }) => cmd_ping(peer_id, kind).await,
        Some(Commands::Status) => cmd_status().await,
        Some(Commands::Trust { action }) => cmd_trust(action).await,
        Some(Commands::Doctor) => cmd_doctor(kind).await,
        Some(Commands::Daemon) => cmd_daemon().await,

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
            cmd_serve(Some(code), kind).await
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
async fn cmd_serve(room_code: Option<String>, kind: TransportKind) -> Result<(), MlinkError> {
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
    // Advertises the mlink service and forwards every accepted peer's
    // connection back to the main loop so it can drive the handshake on the
    // Node. Without this, a central that dials us gets frames routed into
    // a dead connection and the handshake stalls forever.
    let (accepted_tx, mut accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    match kind {
        TransportKind::Ble => {
            let mut listen_transport = BleTransport::new();
            listen_transport.set_local_name(local_name.clone());
            // Plumb our app_uuid into the peripheral so advertisements carry
            // the 0000FFBB-prefixed identity UUID (see ble.rs::encode_identity_uuid).
            // Without this, the scanner on the peer can't pick a deterministic
            // role and both sides race to dial.
            listen_transport.set_app_uuid(node.app_uuid().to_string());
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
                        let (central_id, rx) = match peripheral.wait_for_central().await {
                            Ok(pair) => pair,
                            Err(e) => {
                                eprintln!("[mlink] peripheral accept error: {e}");
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
        TransportKind::Tcp => {
            let mut listen_transport = TcpTransport::new();
            listen_transport.set_local_name(local_name.clone());
            listen_transport.set_app_uuid(node.app_uuid().to_string());
            if let Some(h) = room_hash_bytes {
                listen_transport.set_room_hash(h);
            }
            tokio::spawn(async move {
                loop {
                    match listen_transport.listen().await {
                        Ok(conn) => {
                            if accepted_tx.send(conn).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            eprintln!("[mlink] tcp accept error: {e}");
                            return;
                        }
                    }
                }
            });
        }
    }

    // --- 2. Scan loop (background, only when a room is set) --------------
    // Scanner pushes newly-seen peers down `peer_rx`; auto-connect runs on the
    // main task because `Node::connect_peer` needs `&mut Node`. `unsee_tx`
    // lets the main loop ask scanner to forget a peer id so it resurfaces on
    // the next scan round (used when a dial fails and we want a retry).
    let (peer_tx, mut peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    // `cmd_serve` is the host path. For BLE we deliberately skip the scanner
    // entirely — BLE connections are strictly peripheral-only from the host;
    // the joining side runs `mlink join <code>` and does all the scanning /
    // dialling. TCP keeps the legacy symmetric behaviour (it has no role
    // ambiguity because every node can freely accept + dial without stepping
    // on its own advertisement).
    let ble_host_only = matches!(kind, TransportKind::Ble);
    if room_hash_bytes.is_some() && !ble_host_only {
        let app_uuid = node.app_uuid().to_string();
        let hash = room_hash_bytes.expect("is_some checked above");
        let scan_transport: Box<dyn Transport> = match kind {
            TransportKind::Ble => {
                let mut t = BleTransport::new();
                t.set_room_hash(hash);
                Box::new(t)
            }
            TransportKind::Tcp => {
                let mut t = TcpTransport::new();
                t.set_room_hash(hash);
                t.set_app_uuid(app_uuid.clone());
                Box::new(t)
            }
        };
        let mut scanner = Scanner::new(scan_transport, app_uuid);
        scanner.set_room_hashes(vec![hash]);
        scanner.set_unsee_channel(unsee_rx);
        tokio::spawn(async move {
            if let Err(e) = scanner.discover_loop(peer_tx).await {
                eprintln!("[mlink] scanner error: {e}");
            }
        });
    } else {
        // No scanner task → nothing will ever read `unsee_rx`.
        // Drop it explicitly so the channel has no receiver and sends from
        // the main loop fail-fast instead of filling the buffer.
        drop(unsee_rx);
        let _ = peer_tx;
    }

    let mut events = node.subscribe();
    // Connecting transport reused across auto-connect attempts.
    let mut connect_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    // Track wire-level peer ids we have already engaged (as central or as
    // peripheral). When both Macs see each other simultaneously they'd
    // otherwise dial each other; the second dial kicks the first connection.
    // A peer is "engaged" while a connect attempt is in flight or the peer
    // connected inbound first. Failure paths remove the entry so a retry can
    // re-acquire the slot.
    use std::collections::{HashMap, HashSet};
    let mut engaged_wire_ids: HashSet<String> = HashSet::new();
    // Per-peer retry bookkeeping: how many connect attempts we've made so far.
    // Reset once an attempt succeeds or the peer connects inbound.
    const MAX_RETRIES: u8 = 3;
    let mut attempts: HashMap<String, u8> = HashMap::new();
    // Peers that connected inbound (peripheral path) before we finished
    // dialling them. We must not then also dial — that would race and tear
    // down the live inbound link.
    let mut connected_inbound: HashSet<String> = HashSet::new();
    // app_uuids we currently hold a live Node connection to. Updated via
    // NodeEvent::PeerConnected / PeerDisconnected. A peer can surface under
    // different wire ids across scan rounds (or arrive inbound vs. outbound),
    // so the scanner's wire_id alone is not enough to dedupe — we also check
    // the post-handshake app_uuid before touching the dial path.
    let mut connected_peers: HashSet<String> = HashSet::new();
    // wire_id -> app_uuid learned at connect/accept time. Lets us short-circuit
    // scanner hits that would otherwise walk all the way into connect_peer
    // before the Node-layer dedup kicks in.
    let mut wire_to_app: HashMap<String, String> = HashMap::new();

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
                if connected_inbound.contains(&peer.id) {
                    eprintln!(
                        "[mlink:conn] serve: skip dial {} — already connected inbound",
                        peer.id
                    );
                    continue;
                }
                if engaged_wire_ids.contains(&peer.id) {
                    eprintln!(
                        "[mlink:conn] serve: skip dial {} — already engaged",
                        peer.id
                    );
                    continue;
                }
                if let Some(app) = wire_to_app.get(&peer.id) {
                    if connected_peers.contains(app) {
                        eprintln!(
                            "[mlink:conn] serve: skip dial {} — app_uuid {} already connected",
                            peer.id, app
                        );
                        continue;
                    }
                }
                // BLE symmetric-dial guard: the peer's advertisement carries
                // its 8-byte app identity (via the 0000FFBB-prefixed service
                // UUID) in `peer.metadata`. If our own app_uuid compares less
                // than the peer's identity, we skip dialling and wait for the
                // inbound subscribe — only one side should dial. `None` from
                // should_dial_as_central (invalid local uuid, tie) falls back
                // to the legacy symmetric-dial path so nothing hangs.
                if matches!(kind, TransportKind::Ble) && peer.metadata.len() == 8 {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&peer.metadata);
                    match mlink_core::transport::ble::should_dial_as_central(
                        node.app_uuid(),
                        &id,
                    ) {
                        Some(false) => {
                            eprintln!(
                                "[mlink:conn] serve: skip dial {} — peer outranks us, waiting for inbound",
                                peer.id
                            );
                            continue;
                        }
                        Some(true) => {
                            eprintln!(
                                "[mlink:conn] serve: role=central for {} (we outrank peer)",
                                peer.id
                            );
                        }
                        None => {
                            eprintln!(
                                "[mlink:conn] serve: role tie/invalid for {} — falling back to symmetric dial",
                                peer.id
                            );
                        }
                    }
                }
                let attempt = attempts.entry(peer.id.clone()).or_insert(0);
                if *attempt >= MAX_RETRIES {
                    eprintln!(
                        "[mlink:conn] serve: giving up on {} after {} attempts",
                        peer.id, *attempt
                    );
                    continue;
                }
                *attempt += 1;
                let attempt_no = *attempt;
                engaged_wire_ids.insert(peer.id.clone());
                println!(
                    "[mlink] discovered {} ({}) — connecting (attempt {}/{})...",
                    peer.name, peer.id, attempt_no, MAX_RETRIES
                );
                eprintln!(
                    "[mlink:conn] serve: dial as central wire_id={} attempt={}",
                    peer.id, attempt_no
                );
                let peer_wire_id = peer.id.clone();
                let dial_result = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.connect_peer(connect_transport.as_mut(), &peer),
                )
                .await;
                let dial_result = match dial_result {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "connect to {} timed out after {:?}",
                        peer_wire_id, CONNECT_TIMEOUT
                    ))),
                };
                match dial_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id}");
                        attempts.remove(&peer_wire_id);
                        wire_to_app.insert(peer_wire_id.clone(), peer_id.clone());
                        connected_peers.insert(peer_id);
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped {peer_id}: different room");
                        // Different room → no retry, pin the attempt counter
                        // so we don't keep redialling a peer we just rejected.
                        attempts.insert(peer_wire_id.clone(), MAX_RETRIES);
                        engaged_wire_ids.remove(&peer_wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] connect {} failed: {e}", peer_wire_id);
                        engaged_wire_ids.remove(&peer_wire_id);
                        // Random 1-3s backoff, then ask scanner to re-surface
                        // this peer so the retry runs on the next scan tick.
                        let delay = random_backoff();
                        let unsee_tx = unsee_tx.clone();
                        let wire_id = peer_wire_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = unsee_tx.send(wire_id).await;
                        });
                        eprintln!(
                            "[mlink:conn] serve: will retry {} in {}ms",
                            peer_wire_id,
                            delay.as_millis()
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
                // Mark this wire as engaged so the scanner doesn't also dial
                // them while we're finishing the peripheral-side handshake.
                engaged_wire_ids.insert(wire_id.clone());
                // Treat inbound-first as the authoritative direction: future
                // scanner hits for the same wire_id will be skipped instead of
                // dialled on top of the live link.
                connected_inbound.insert(wire_id.clone());
                println!("[mlink] incoming central {wire_id} — handshaking...");
                eprintln!("[mlink:conn] serve: accept as peripheral wire_id={wire_id}");
                let accept_result = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.accept_incoming(conn, transport_label(kind), wire_id.clone()),
                )
                .await;
                let accept_result = match accept_result {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "accept from {} timed out after {:?}",
                        wire_id, CONNECT_TIMEOUT
                    ))),
                };
                match accept_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id} (incoming)");
                        attempts.remove(&wire_id);
                        wire_to_app.insert(wire_id.clone(), peer_id.clone());
                        connected_peers.insert(peer_id);
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped incoming {peer_id}: different room");
                        engaged_wire_ids.remove(&wire_id);
                        connected_inbound.remove(&wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] accept {wire_id} failed: {e}");
                        engaged_wire_ids.remove(&wire_id);
                        connected_inbound.remove(&wire_id);
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        println!("[mlink] peer connected: {peer_id}");
                        connected_peers.insert(peer_id);
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        println!("[mlink] peer disconnected: {peer_id}");
                        connected_peers.remove(&peer_id);
                        wire_to_app.retain(|_, app| app != &peer_id);
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

/// Interactive chat: same connect/accept plumbing as `cmd_serve`, plus a
/// stdin reader that broadcasts each typed line and a per-peer reader task
/// that surfaces incoming messages as `NodeEvent::MessageReceived` which we
/// print inline with the rest of the event log.
async fn cmd_chat(code: String, kind: TransportKind) -> Result<(), MlinkError> {
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
    let (accepted_tx, mut accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    match kind {
        TransportKind::Ble => {
            let mut listen_transport = BleTransport::new();
            listen_transport.set_local_name(local_name.clone());
            // See cmd_serve: the peer's scanner needs our identity UUID in
            // the advertisement to pick a role without symmetric-dial racing.
            listen_transport.set_app_uuid(node.app_uuid().to_string());
            listen_transport.set_room_hash(hash);
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
                        let (central_id, rx) = match peripheral.wait_for_central().await {
                            Ok(pair) => pair,
                            Err(e) => {
                                eprintln!("[mlink] peripheral accept error: {e}");
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
        TransportKind::Tcp => {
            let mut listen_transport = TcpTransport::new();
            listen_transport.set_local_name(local_name.clone());
            listen_transport.set_app_uuid(node.app_uuid().to_string());
            listen_transport.set_room_hash(hash);
            tokio::spawn(async move {
                loop {
                    match listen_transport.listen().await {
                        Ok(conn) => {
                            if accepted_tx.send(conn).await.is_err() {
                                return;
                            }
                        }
                        Err(e) => {
                            eprintln!("[mlink] tcp accept error: {e}");
                            return;
                        }
                    }
                }
            });
        }
    }

    // --- Scan loop (room-filtered) ---------------------------------------
    // Host-mode chat: BLE skips the scanner entirely (peripheral-only) — the
    // joining side runs `mlink join --chat <code>` and is responsible for all
    // discovery and dialling. TCP keeps the legacy symmetric behaviour.
    let (peer_tx, mut peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    let ble_host_only = matches!(kind, TransportKind::Ble);
    if !ble_host_only {
        let app_uuid = node.app_uuid().to_string();
        let scan_transport: Box<dyn Transport> = match kind {
            TransportKind::Ble => {
                let mut t = BleTransport::new();
                t.set_room_hash(hash);
                Box::new(t)
            }
            TransportKind::Tcp => {
                let mut t = TcpTransport::new();
                t.set_room_hash(hash);
                t.set_app_uuid(app_uuid.clone());
                Box::new(t)
            }
        };
        let mut scanner = Scanner::new(scan_transport, app_uuid);
        scanner.set_room_hashes(vec![hash]);
        scanner.set_unsee_channel(unsee_rx);
        tokio::spawn(async move {
            if let Err(e) = scanner.discover_loop(peer_tx).await {
                eprintln!("[mlink] scanner error: {e}");
            }
        });
    } else {
        drop(unsee_rx);
        let _ = peer_tx;
    }

    // --- Stdin reader ----------------------------------------------------
    // Forwards each typed line to the main select loop. Using a bounded mpsc
    // keeps backpressure on fast typists (or piped-in files) so we don't grow
    // the queue unboundedly while a broadcast is mid-flight.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(16);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let stdin = tokio::io::stdin();
        let mut lines = BufReader::new(stdin).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.is_empty() {
                        continue;
                    }
                    if stdin_tx.send(line).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return, // stdin closed
                Err(e) => {
                    eprintln!("[mlink] stdin read error: {e}");
                    return;
                }
            }
        }
    });

    let mut events = node.subscribe();
    let mut connect_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    use std::collections::{HashMap, HashSet};
    let mut engaged_wire_ids: HashSet<String> = HashSet::new();
    const MAX_RETRIES: u8 = 3;
    let mut attempts: HashMap<String, u8> = HashMap::new();
    let mut connected_inbound: HashSet<String> = HashSet::new();
    // See cmd_serve for the rationale — dedup by app_uuid, mapped from wire_id.
    let mut connected_peers: HashSet<String> = HashSet::new();
    let mut wire_to_app: HashMap<String, String> = HashMap::new();
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
                if connected_inbound.contains(&peer.id) || engaged_wire_ids.contains(&peer.id) {
                    continue;
                }
                if let Some(app) = wire_to_app.get(&peer.id) {
                    if connected_peers.contains(app) {
                        continue;
                    }
                }
                // BLE role arbitration — same rationale as cmd_serve. The
                // peer's 8-byte identity rides in `peer.metadata`; if we
                // don't outrank it, skip and let them dial us.
                if matches!(kind, TransportKind::Ble) && peer.metadata.len() == 8 {
                    let mut id = [0u8; 8];
                    id.copy_from_slice(&peer.metadata);
                    match mlink_core::transport::ble::should_dial_as_central(
                        node.app_uuid(),
                        &id,
                    ) {
                        Some(false) => {
                            eprintln!(
                                "[mlink:conn] chat: skip dial {} — peer outranks us, waiting for inbound",
                                peer.id
                            );
                            continue;
                        }
                        Some(true) => {
                            eprintln!(
                                "[mlink:conn] chat: role=central for {} (we outrank peer)",
                                peer.id
                            );
                        }
                        None => {
                            eprintln!(
                                "[mlink:conn] chat: role tie/invalid for {} — falling back to symmetric dial",
                                peer.id
                            );
                        }
                    }
                }
                let attempt = attempts.entry(peer.id.clone()).or_insert(0);
                if *attempt >= MAX_RETRIES {
                    continue;
                }
                *attempt += 1;
                let attempt_no = *attempt;
                engaged_wire_ids.insert(peer.id.clone());
                println!(
                    "[mlink] discovered {} ({}) — connecting (attempt {}/{})...",
                    peer.name, peer.id, attempt_no, MAX_RETRIES
                );
                let peer_wire_id = peer.id.clone();
                let peer_wire_name = peer.name.clone();
                let dial_result = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.connect_peer(connect_transport.as_mut(), &peer),
                )
                .await;
                let dial_result = match dial_result {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "connect to {} timed out after {:?}",
                        peer_wire_id, CONNECT_TIMEOUT
                    ))),
                };
                match dial_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id}");
                        attempts.remove(&peer_wire_id);
                        peer_names.insert(peer_id.clone(), peer_wire_name);
                        wire_to_app.insert(peer_wire_id.clone(), peer_id.clone());
                        connected_peers.insert(peer_id.clone());
                        if !readers.contains_key(&peer_id) {
                            readers.insert(peer_id.clone(), node.spawn_peer_reader(peer_id));
                        }
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped {peer_id}: different room");
                        attempts.insert(peer_wire_id.clone(), MAX_RETRIES);
                        engaged_wire_ids.remove(&peer_wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] connect {} failed: {e}", peer_wire_id);
                        engaged_wire_ids.remove(&peer_wire_id);
                        let delay = random_backoff();
                        let unsee_tx = unsee_tx.clone();
                        let wire_id = peer_wire_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = unsee_tx.send(wire_id).await;
                        });
                    }
                }
            }
            maybe_conn = accepted_rx.recv() => {
                let conn = match maybe_conn {
                    Some(c) => c,
                    None => continue,
                };
                let wire_id = conn.peer_id().to_string();
                engaged_wire_ids.insert(wire_id.clone());
                connected_inbound.insert(wire_id.clone());
                println!("[mlink] incoming {wire_id} — handshaking...");
                let accept_result = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.accept_incoming(conn, transport_label(kind), wire_id.clone()),
                )
                .await;
                let accept_result = match accept_result {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "accept from {} timed out after {:?}",
                        wire_id, CONNECT_TIMEOUT
                    ))),
                };
                match accept_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id} (incoming)");
                        attempts.remove(&wire_id);
                        peer_names.insert(peer_id.clone(), wire_id.clone());
                        wire_to_app.insert(wire_id.clone(), peer_id.clone());
                        connected_peers.insert(peer_id.clone());
                        if !readers.contains_key(&peer_id) {
                            readers.insert(peer_id.clone(), node.spawn_peer_reader(peer_id));
                        }
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped incoming {peer_id}: different room");
                        engaged_wire_ids.remove(&wire_id);
                        connected_inbound.remove(&wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] accept {wire_id} failed: {e}");
                        engaged_wire_ids.remove(&wire_id);
                        connected_inbound.remove(&wire_id);
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::MessageReceived { peer_id, payload }) => {
                        let name = peer_names.get(&peer_id).cloned().unwrap_or_else(|| peer_id.clone());
                        let text = match std::str::from_utf8(&payload) {
                            Ok(s) => s.to_string(),
                            Err(_) => format!("<{} bytes binary>", payload.len()),
                        };
                        println!("[{name}] {text}");
                    }
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        println!("[mlink] peer connected: {peer_id}");
                        connected_peers.insert(peer_id);
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        println!("[mlink] peer disconnected: {peer_id}");
                        connected_peers.remove(&peer_id);
                        wire_to_app.retain(|_, app| app != &peer_id);
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

/// Joining-side entrypoint. Runs scanner + dial only — no peripheral
/// advertisement, no accept loop. Pairs with the host running `mlink <code>`
/// (or `mlink chat <code>`) on the other device. When `chat` is true we also
/// wire up a stdin reader and spawn per-peer message readers so typed lines
/// broadcast and inbound messages print inline (same shape as `cmd_chat`).
async fn cmd_join(code: String, chat: bool, kind: TransportKind) -> Result<(), MlinkError> {
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
    let (peer_tx, mut peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    {
        let app_uuid = node.app_uuid().to_string();
        let scan_transport: Box<dyn Transport> = match kind {
            TransportKind::Ble => {
                let mut t = BleTransport::new();
                t.set_room_hash(hash);
                Box::new(t)
            }
            TransportKind::Tcp => {
                let mut t = TcpTransport::new();
                t.set_room_hash(hash);
                t.set_app_uuid(app_uuid.clone());
                Box::new(t)
            }
        };
        let mut scanner = Scanner::new(scan_transport, app_uuid);
        scanner.set_room_hashes(vec![hash]);
        scanner.set_unsee_channel(unsee_rx);
        tokio::spawn(async move {
            if let Err(e) = scanner.discover_loop(peer_tx).await {
                eprintln!("[mlink] scanner error: {e}");
            }
        });
    }

    // --- Optional stdin reader (chat mode) ------------------------------
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<String>(16);
    if chat {
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let stdin = tokio::io::stdin();
            let mut lines = BufReader::new(stdin).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.is_empty() {
                            continue;
                        }
                        if stdin_tx.send(line).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        eprintln!("[mlink] stdin read error: {e}");
                        return;
                    }
                }
            }
        });
    } else {
        drop(stdin_tx);
    }

    let mut events = node.subscribe();
    let mut connect_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    use std::collections::{HashMap, HashSet};
    let mut engaged_wire_ids: HashSet<String> = HashSet::new();
    const MAX_RETRIES: u8 = 3;
    let mut attempts: HashMap<String, u8> = HashMap::new();
    let mut connected_peers: HashSet<String> = HashSet::new();
    let mut wire_to_app: HashMap<String, String> = HashMap::new();
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
                if engaged_wire_ids.contains(&peer.id) {
                    continue;
                }
                if let Some(app) = wire_to_app.get(&peer.id) {
                    if connected_peers.contains(app) {
                        continue;
                    }
                }
                // In join mode we always dial — no role arbitration is needed
                // because the host deliberately does not dial back. This is
                // the whole point of the host/join split.
                let attempt = attempts.entry(peer.id.clone()).or_insert(0);
                if *attempt >= MAX_RETRIES {
                    eprintln!(
                        "[mlink:conn] join: giving up on {} after {} attempts",
                        peer.id, *attempt
                    );
                    continue;
                }
                *attempt += 1;
                let attempt_no = *attempt;
                engaged_wire_ids.insert(peer.id.clone());
                println!(
                    "[mlink] discovered {} ({}) — connecting (attempt {}/{})...",
                    peer.name, peer.id, attempt_no, MAX_RETRIES
                );
                let peer_wire_id = peer.id.clone();
                let peer_wire_name = peer.name.clone();
                let dial_result = tokio::time::timeout(
                    CONNECT_TIMEOUT,
                    node.connect_peer(connect_transport.as_mut(), &peer),
                )
                .await;
                let dial_result = match dial_result {
                    Ok(inner) => inner,
                    Err(_) => Err(MlinkError::HandlerError(format!(
                        "connect to {} timed out after {:?}",
                        peer_wire_id, CONNECT_TIMEOUT
                    ))),
                };
                match dial_result {
                    Ok(peer_id) => {
                        println!("[mlink] + {peer_id}");
                        attempts.remove(&peer_wire_id);
                        peer_names.insert(peer_id.clone(), peer_wire_name);
                        wire_to_app.insert(peer_wire_id.clone(), peer_id.clone());
                        connected_peers.insert(peer_id.clone());
                        if chat && !readers.contains_key(&peer_id) {
                            readers.insert(peer_id.clone(), node.spawn_peer_reader(peer_id));
                        }
                    }
                    Err(MlinkError::RoomMismatch { peer_id }) => {
                        println!("[mlink] dropped {peer_id}: different room");
                        attempts.insert(peer_wire_id.clone(), MAX_RETRIES);
                        engaged_wire_ids.remove(&peer_wire_id);
                    }
                    Err(e) => {
                        eprintln!("[mlink] connect {} failed: {e}", peer_wire_id);
                        engaged_wire_ids.remove(&peer_wire_id);
                        let delay = random_backoff();
                        let unsee_tx = unsee_tx.clone();
                        let wire_id = peer_wire_id.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = unsee_tx.send(wire_id).await;
                        });
                    }
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::MessageReceived { peer_id, payload }) if chat => {
                        let name = peer_names.get(&peer_id).cloned().unwrap_or_else(|| peer_id.clone());
                        let text = match std::str::from_utf8(&payload) {
                            Ok(s) => s.to_string(),
                            Err(_) => format!("<{} bytes binary>", payload.len()),
                        };
                        println!("[{name}] {text}");
                    }
                    Ok(NodeEvent::PeerConnected { peer_id }) => {
                        println!("[mlink] peer connected: {peer_id}");
                        connected_peers.insert(peer_id);
                    }
                    Ok(NodeEvent::PeerDisconnected { peer_id }) => {
                        println!("[mlink] peer disconnected: {peer_id}");
                        connected_peers.remove(&peer_id);
                        wire_to_app.retain(|_, app| app != &peer_id);
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

async fn cmd_room(action: RoomAction, kind: TransportKind) -> Result<(), MlinkError> {
    match action {
        RoomAction::New => {
            let code = generate_room_code();
            println!("[mlink] room created: {code}");
            println!("[mlink] share this code with other devices, then run:");
            println!("        mlink room join {code}");
            cmd_serve(Some(code), kind).await
        }
        RoomAction::Join { code } => {
            validate_room_code(&code)?;
            println!("[mlink] joining room {code}");
            cmd_serve(Some(code), kind).await
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
    mlink_cli::validate_room_code(code).map_err(MlinkError::HandlerError)
}

/// `send <code> [msg] [--file <path>]` — bring up a short-lived node, scan
/// for peers in the given room, broadcast, exit.
async fn cmd_send_room(
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

async fn cmd_listen(kind: TransportKind) -> Result<(), MlinkError> {
    // Listen = serve without joining any particular room; just print every
    // MESSAGE frame we receive across any peer the daemon has accepted.
    println!("[mlink] listen: running serve loop, Ctrl+C to quit");
    cmd_serve(None, kind).await
}

fn transport_label(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Ble => "ble",
        TransportKind::Tcp => "tcp",
    }
}

// ---- legacy peer-id commands ------------------------------------------------

async fn cmd_scan(kind: TransportKind) -> Result<(), MlinkError> {
    let mut transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let peers = match transport.discover().await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[mlink] scan failed: {e}");
            if matches!(kind, TransportKind::Ble) {
                print_bluetooth_permission_hint();
            }
            return Err(e);
        }
    };
    if peers.is_empty() && matches!(kind, TransportKind::Ble) {
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

async fn cmd_connect(peer_id: String, kind: TransportKind) -> Result<(), MlinkError> {
    let mut transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let peers = transport.discover().await?;
    let discovered = peers.into_iter().find(|p| p.id == peer_id).ok_or_else(|| {
        MlinkError::HandlerError(format!("peer {peer_id} not found in scan results"))
    })?;

    let node = build_node().await?;
    node.start().await?;

    println!("connecting to {peer_id}...");
    let app_uuid = node.connect_peer(transport.as_mut(), &discovered).await?;
    println!("connected (app_uuid={app_uuid})");
    Ok(())
}

async fn cmd_ping(peer_id: String, kind: TransportKind) -> Result<(), MlinkError> {
    let mut transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => Box::new(BleTransport::new()),
        TransportKind::Tcp => Box::new(TcpTransport::new()),
    };
    let peers = transport.discover().await?;
    let discovered = peers.into_iter().find(|p| p.id == peer_id).ok_or_else(|| {
        MlinkError::HandlerError(format!("peer {peer_id} not found in scan results"))
    })?;

    let node = build_node().await?;
    node.start().await?;

    let app_uuid = node.connect_peer(transport.as_mut(), &discovered).await?;
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

async fn cmd_doctor(kind: TransportKind) -> Result<(), MlinkError> {
    println!("mlink doctor");
    println!("  platform:     {}", std::env::consts::OS);
    println!("  architecture: {}", std::env::consts::ARCH);
    println!("  transport:    {}", transport_label(kind));

    match kind {
        TransportKind::Ble => {
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
        }
        TransportKind::Tcp => {
            print!("  TCP loopback: ");
            match tcp_loopback_check().await {
                Ok(port) => println!("OK (bound 127.0.0.1:{port}, round-trip succeeded)"),
                Err(e) => {
                    println!("FAIL ({e})");
                    return Err(e);
                }
            }

            print!("  mDNS daemon:  ");
            match probe_mdns_daemon() {
                Ok(()) => println!("OK (ServiceDaemon start/shutdown succeeded)"),
                Err(e) => {
                    println!("FAIL ({e})");
                    return Err(e);
                }
            }
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

/// Run the long-lived daemon. Defers all the heavy lifting to
/// `mlink_daemon::run()` which owns the Node, single-instance lock, and WS
/// server. We just translate its error into `MlinkError::HandlerError` so the
/// CLI's error printing path is uniform.
async fn cmd_daemon() -> Result<(), MlinkError> {
    mlink_daemon::run()
        .await
        .map_err(|e| MlinkError::HandlerError(format!("daemon: {e}")))
}

async fn tcp_loopback_check() -> Result<u16, MlinkError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| MlinkError::HandlerError(format!("tcp bind 127.0.0.1:0: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| MlinkError::HandlerError(format!("tcp local_addr: {e}")))?;
    let port = addr.port();

    let accept_task = tokio::spawn(async move {
        listener.accept().await.map(|_| ())
    });
    let _client = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| MlinkError::HandlerError(format!("tcp connect self {addr}: {e}")))?;
    let accept_result = tokio::time::timeout(Duration::from_secs(2), accept_task)
        .await
        .map_err(|_| MlinkError::HandlerError("tcp self-accept timed out".into()))?
        .map_err(|e| MlinkError::HandlerError(format!("tcp accept task join: {e}")))?;
    accept_result.map_err(|e| MlinkError::HandlerError(format!("tcp self-accept: {e}")))?;
    Ok(port)
}


/// Outer timeout for connect_peer / accept_incoming. Picked larger than the
/// 10s handshake IO ceiling in `perform_handshake` so an honest-but-slow peer
/// still completes, but small enough that a silent peer can't freeze the
/// main `select!` loop. If this fires, the dial/accept path unwinds via a
/// `HandlerError` and the normal retry-with-backoff path runs.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Random retry delay in the 1000-3000ms window. We derive the jitter from
/// the current wall-clock nanoseconds to avoid pulling in a `rand` dependency
/// for a single dice roll.
fn random_backoff() -> std::time::Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // 1000..=3000 ms window → 2001 possible values.
    let ms = 1000u64 + (nanos as u64 % 2001);
    std::time::Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_backoff_stays_within_window() {
        for _ in 0..50 {
            let d = random_backoff();
            assert!(d >= std::time::Duration::from_millis(1000));
            assert!(d <= std::time::Duration::from_millis(3000));
        }
    }
}
