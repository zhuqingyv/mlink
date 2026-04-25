//! Room-filtered peer discovery shared by serve/chat/join. Each command
//! spawns a `Scanner::discover_loop` and wires up the unsee channel so the
//! main `select!` can ask the scanner to forget a peer after a failed dial.
//!
//! `disabled_scanner` is the inert variant: `cmd_serve` + `cmd_chat` take
//! the BLE host path, which must not run a scanner at all (we're peripheral-
//! only). Dropping the unsee rx here means any `unsee_tx.send(...)` from the
//! main loop fails fast instead of filling the channel with ghosts.

use mlink_core::core::scanner::Scanner;
use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::{DiscoveredPeer, Transport};
use tokio::sync::mpsc;

use mlink_cli::TransportKind;

/// A live scanner task: `peer_rx` yields fresh `DiscoveredPeer`s; the caller
/// sends wire ids to `unsee_tx` to ask the scanner to re-surface them on the
/// next round (post-backoff retry).
pub(crate) struct ScannerHandle {
    pub peer_rx: mpsc::Receiver<DiscoveredPeer>,
    pub unsee_tx: mpsc::Sender<String>,
}

pub(crate) fn spawn_scanner_task(
    kind: TransportKind,
    app_uuid: String,
    room_hash: [u8; 8],
) -> ScannerHandle {
    let (peer_tx, peer_rx) = mpsc::channel::<DiscoveredPeer>(16);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(16);
    let scan_transport: Box<dyn Transport> = match kind {
        TransportKind::Ble => {
            let mut t = BleTransport::new();
            t.set_room_hash(room_hash);
            Box::new(t)
        }
        TransportKind::Tcp => {
            let mut t = TcpTransport::new();
            t.set_room_hash(room_hash);
            t.set_app_uuid(app_uuid.clone());
            Box::new(t)
        }
    };
    let mut scanner = Scanner::new(scan_transport, app_uuid);
    scanner.set_room_hashes(vec![room_hash]);
    scanner.set_unsee_channel(unsee_rx);
    tokio::spawn(async move {
        if let Err(e) = scanner.discover_loop(peer_tx).await {
            eprintln!("[mlink] scanner error: {e}");
        }
    });
    ScannerHandle { peer_rx, unsee_tx }
}

/// Inert scanner: `peer_rx.recv()` returns `None` immediately and the
/// unsee channel's receiver is dropped so `unsee_tx.send(...)` fails fast.
/// Used by cmd_serve / cmd_chat on the BLE host path (peripheral-only, so
/// no scan must happen) — the main select! keeps the same shape either way.
pub(crate) fn disabled_scanner() -> ScannerHandle {
    let (_peer_tx, peer_rx) = mpsc::channel::<DiscoveredPeer>(1);
    let (unsee_tx, unsee_rx) = mpsc::channel::<String>(1);
    // Drop both halves so: (a) `peer_rx.recv()` returns None on the next
    // tick because the sender is dropped, and (b) any `unsee_tx.send(...)`
    // fails immediately because the receiver is dropped.
    drop(_peer_tx);
    drop(unsee_rx);
    ScannerHandle { peer_rx, unsee_tx }
}
