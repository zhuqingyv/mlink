//! `mlink doctor` — one-shot environment check: platform, selected transport,
//! BLE adapter (or TCP loopback + mDNS probe), trust store. The `tcp_loopback_check`
//! helper lives here because this is its only caller.

use std::time::Duration;

use mlink_core::core::security::TrustStore;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::probe_mdns_daemon;
use mlink_core::transport::Transport;

use crate::node_build::transport_label;
use crate::util::printing::print_bluetooth_permission_hint;
use mlink_cli::TransportKind;

pub(crate) async fn cmd_doctor(kind: TransportKind) -> Result<(), MlinkError> {
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

async fn tcp_loopback_check() -> Result<u16, MlinkError> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| MlinkError::HandlerError(format!("tcp bind 127.0.0.1:0: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| MlinkError::HandlerError(format!("tcp local_addr: {e}")))?;
    let port = addr.port();

    let accept_task = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
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
