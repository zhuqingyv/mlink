//! Peripheral advertisement (BLE) or TCP listener, spawned as a tokio task
//! and hooked back to the main select! via a bounded mpsc. Shared by
//! cmd_serve and cmd_chat — cmd_join never listens.
//!
//! The BLE branch is macOS-only because that's the only platform where we
//! have a `MacPeripheralConnection`. On Linux/Windows the BLE path falls
//! through to `listen().await` which currently parks the task forever.

use mlink_core::transport::ble::BleTransport;
use mlink_core::transport::tcp::TcpTransport;
use mlink_core::transport::{Connection, Transport};
use tokio::sync::mpsc;

use crate::util::printing::print_bluetooth_permission_hint;
use mlink_cli::TransportKind;

pub(crate) struct ListenHandle {
    pub rx: mpsc::Receiver<Box<dyn Connection>>,
}

pub(crate) fn spawn_listen_task(
    kind: TransportKind,
    local_name: String,
    app_uuid: String,
    room_hash: Option<[u8; 8]>,
) -> ListenHandle {
    let (accepted_tx, accepted_rx) = mpsc::channel::<Box<dyn Connection>>(4);
    match kind {
        TransportKind::Ble => {
            let mut listen_transport = BleTransport::new();
            listen_transport.set_local_name(local_name.clone());
            // Plumb our app_uuid into the peripheral so advertisements carry
            // the 0000FFBB-prefixed identity UUID (see ble.rs::encode_identity_uuid).
            // Without this, the scanner on the peer can't pick a deterministic
            // role and both sides race to dial.
            listen_transport.set_app_uuid(app_uuid);
            if let Some(h) = room_hash {
                listen_transport.set_room_hash(h);
            }
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
                            "[mlink:debug] peripheral accepted central {central_id} (as {local_name})"
                        );
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (local_name, accepted_tx);
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
            listen_transport.set_local_name(local_name);
            listen_transport.set_app_uuid(app_uuid);
            if let Some(h) = room_hash {
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
    ListenHandle { rx: accepted_rx }
}
