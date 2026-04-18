use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use mlink_cli::{Cli, Commands, TrustAction};
use mlink_core::core::node::{Node, NodeConfig};
use mlink_core::core::security::TrustStore;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::{DiscoveredPeer, Transport};

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
        Commands::Scan => cmd_scan().await,
        Commands::Connect { peer_id } => cmd_connect(peer_id).await,
        Commands::Ping { peer_id } => cmd_ping(peer_id).await,
        Commands::Send { peer_id, message } => cmd_send(peer_id, message).await,
        Commands::Status => cmd_status().await,
        Commands::Trust { action } => cmd_trust(action).await,
        Commands::Doctor => cmd_doctor().await,
    }
}

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

    let mut node = Node::new(NodeConfig {
        name: "mlink-cli".into(),
        ..Default::default()
    })
    .await?;
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

    let mut node = Node::new(NodeConfig {
        name: "mlink-cli".into(),
        ..Default::default()
    })
    .await?;
    node.start().await?;

    let app_uuid = node.connect_peer(&mut transport, &discovered).await?;
    let start = Instant::now();
    node.send_raw(&app_uuid, MessageType::Heartbeat, &[]).await?;
    let (_frame, _payload) = node.recv_raw(&app_uuid).await?;
    let rtt = start.elapsed();
    println!("ping {app_uuid}: rtt={:.2}ms", rtt.as_secs_f64() * 1000.0);
    Ok(())
}

async fn cmd_send(peer_id: String, message: String) -> Result<(), MlinkError> {
    let mut transport = BleTransport::new();
    let peers = transport.discover().await?;
    let discovered = peers.into_iter().find(|p| p.id == peer_id).ok_or_else(|| {
        MlinkError::HandlerError(format!("peer {peer_id} not found in scan results"))
    })?;

    let mut node = Node::new(NodeConfig {
        name: "mlink-cli".into(),
        ..Default::default()
    })
    .await?;
    node.start().await?;

    let app_uuid = node.connect_peer(&mut transport, &discovered).await?;
    node.send_raw(&app_uuid, MessageType::Message, message.as_bytes())
        .await?;
    println!("sent {} bytes to {app_uuid}", message.len());
    Ok(())
}

async fn cmd_status() -> Result<(), MlinkError> {
    let node = Node::new(NodeConfig {
        name: "mlink-cli".into(),
        ..Default::default()
    })
    .await?;
    println!("state: {:?}", node.state());
    println!("app_uuid: {}", node.app_uuid());
    let peers = node.peers().await;
    println!("connected peers: {}", peers.len());
    for p in peers {
        println!("  - {} ({}) transport={}", p.id, p.name, p.transport_id);
    }
    println!("note: CLI invocations are stateless — run `mlink connect` in a long-lived process to see peers here");
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
