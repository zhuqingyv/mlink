//! Daemon bootstrap: wiring Node + broadcast bridge + fan-out worker + listener
//! bind + single-instance lock into a ready-to-serve `BoundServe`. Split out of
//! `lib.rs` so the crate root only re-exports module surfaces and delegates
//! `run()` here. The two public entrypoints are `build_state` (state only, used
//! by integration tests) and `bind_and_prepare` (state + bound listener, used
//! by `run()` and `mlink dev`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use mlink_core::core::node::{Node, NodeConfig, NodeEvent};
use mlink_core::core::room::room_hash;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use crate::discovery;
use crate::http::router;
use crate::lifecycle::{
    daemon_info_path, ensure_single_instance, remove_daemon_info, write_daemon_info, DaemonError,
    DaemonInfo,
};
use crate::protocol::{encode_frame, MessagePayload};
use crate::queue::{MessageEntry, MessageQueue};
use crate::rooms::{self, RoomStore};
use crate::state::{DaemonState, EVENT_CHANNEL_CAPACITY};
use crate::subscription::SessionHandle;

/// Start a `Node`, wire a background bridge from `Node::subscribe()` into the
/// returned `broadcast::Sender`. Exposed so integration tests can stand up a
/// daemon state without touching the filesystem / port binding.
pub async fn build_state() -> Result<DaemonState, DaemonError> {
    let node = Node::new(NodeConfig {
        name: host_name(),
        encrypt: true,
        trust_store_path: None,
    })
    .await?;
    node.start().await?;

    let node = Arc::new(node);
    let (tx, _rx) = broadcast::channel::<NodeEvent>(EVENT_CHANNEL_CAPACITY);

    // Bridge Node → broadcast so WS handlers can `tx.subscribe()` and never
    // starve the underlying Node broadcast. We drop sends when nobody is
    // listening (the initial state after startup); lagged slow consumers
    // will see a `RecvError::Lagged` and skip ahead on their side.
    let mut node_events = node.subscribe();
    let tx_bg = tx.clone();
    tokio::spawn(async move {
        loop {
            match node_events.recv().await {
                Ok(ev) => {
                    let _ = tx_bg.send(ev);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "node event bridge lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    // Restore persisted room membership *before* discovery boots, so peers
    // advertising those rooms match as soon as the scanner sees them. Missing
    // or corrupt rooms.json yields an empty store (see rooms::RoomStore::load).
    let rooms_path = rooms::rooms_file_path()?;
    let room_store = RoomStore::load(rooms_path);
    for code in room_store.list() {
        node.add_room_hash(room_hash(&code));
    }
    let rooms = Arc::new(StdMutex::new(room_store));
    let queue = Arc::new(StdMutex::new(MessageQueue::new()));
    let sessions: Arc<StdMutex<Vec<SessionHandle>>> = Arc::new(StdMutex::new(Vec::new()));

    // Fan-out worker: consumes MessageReceived, stores one entry per
    // daemon-level room, and pushes to every session subscribed to that
    // room. The queue+sessions mutex pair is held across push-then-fan so a
    // racing `join` either sees the message in the queue (via drain) or
    // receives it through fan-out, never both, never neither.
    {
        let mut events_rx = tx.subscribe();
        let queue_bg = Arc::clone(&queue);
        let sessions_bg = Arc::clone(&sessions);
        let rooms_bg = Arc::clone(&rooms);
        tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    Ok(NodeEvent::MessageReceived { peer_id, payload }) => {
                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        // Parse payload into JSON once (fall back to utf-8
                        // string / binary placeholder) so the queued entry
                        // is the same shape every subscriber will see.
                        let value = match std::str::from_utf8(&payload) {
                            Ok(s) => match serde_json::from_str(s) {
                                Ok(v) => v,
                                Err(_) => serde_json::Value::String(s.to_string()),
                            },
                            Err(_) => serde_json::Value::String(format!(
                                "<{} bytes binary>",
                                payload.len()
                            )),
                        };
                        let daemon_rooms: Vec<String> = rooms_bg
                            .lock()
                            .expect("rooms poisoned")
                            .list();
                        if daemon_rooms.is_empty() {
                            continue;
                        }
                        // Under one critical section: queue push + subscriber
                        // fan-out. Drops on a slow session `tx` are tolerated
                        // because the message is already in the backlog.
                        let mut q = queue_bg.lock().expect("queue poisoned");
                        let sessions = sessions_bg.lock().expect("sessions poisoned");
                        for room_code in &daemon_rooms {
                            q.push(MessageEntry {
                                room: room_code.clone(),
                                from: peer_id.clone(),
                                payload: value.clone(),
                                ts,
                            });
                            let frame = encode_frame(
                                "message",
                                None,
                                MessagePayload {
                                    room: room_code,
                                    from: peer_id.clone(),
                                    payload: value.clone(),
                                    ts,
                                },
                            );
                            for session in sessions.iter() {
                                let (is_sub, tx) = {
                                    let e = session.lock().expect("session poisoned");
                                    (e.subs.contains(room_code), e.tx.clone())
                                };
                                if is_sub {
                                    let _ = tx.try_send(frame.clone());
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "fan-out worker lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    // Kick off discovery + accept in the background so every WS client shares
    // one scanner/peripheral pair. `DaemonTransport::from_env` defaults to
    // `Dual` — an unset env var boots both radios, matching the product-level
    // expectation that the daemon speaks whatever its peers do. BLE bring-up
    // failures downgrade to TCP-only (see `discovery::spawn`). The shared
    // `Arc<Node>` lets SessionManager merge two inbound links for the same
    // peer into one Session.
    let transports =
        discovery::spawn(Arc::clone(&node), discovery::DaemonTransport::from_env());

    let state = DaemonState {
        node,
        node_events: tx,
        sessions,
        rooms,
        queue,
        transports,
    };
    // Re-publish SessionEvent::Switched / StreamProgress onto every WS session.
    // LinkAdded / LinkRemoved are delivered via the NodeEvent path instead, so
    // we don't double-render in the UI.
    crate::session::spawn_session_event_forwarder(state.clone());
    Ok(state)
}

/// Bind the listener, write the daemon-info file, and return the bound port
/// alongside a future that serves requests until ctrl-c. Split from `run()`
/// so callers like `mlink dev` can learn the port before the serve future
/// resolves (needed to open the browser).
pub async fn bind_and_prepare() -> Result<(u16, BoundServe), DaemonError> {
    let info_path = daemon_info_path()?;
    ensure_single_instance(&info_path)?;

    let state = build_state().await?;

    let port: u16 = std::env::var("MLINK_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let info = DaemonInfo { port: bound.port(), pid: std::process::id() };
    write_daemon_info(&info_path, &info)?;

    tracing::info!(port = info.port, pid = info.pid, path = %info_path.display(), "mlink-daemon listening");
    eprintln!("[mlink-daemon] listening on 127.0.0.1:{} (pid={})", info.port, info.pid);

    let app = router(state);
    Ok((bound.port(), BoundServe { listener, app, info_path }))
}

/// Handle carrying a bound listener + router ready to be served. Kept opaque
/// so `run()` and `mlink dev` share exactly the same ctrl-c + cleanup path.
pub struct BoundServe {
    listener: TcpListener,
    app: Router,
    info_path: PathBuf,
}

impl BoundServe {
    /// Serve until ctrl-c, then remove the daemon-info file. Idempotent on
    /// error — the file is always cleaned up on return.
    pub async fn await_shutdown(self) -> Result<(), DaemonError> {
        let BoundServe { listener, app, info_path } = self;
        // `axum::serve(...)` returns a `Serve` future; `IntoFuture::into_future`
        // isn't in scope here, so we wrap the `.await` in an inner async block
        // and race it against ctrl-c at the same level.
        let result = tokio::select! {
            r = async { axum::serve(listener, app).await } => r.map_err(DaemonError::from),
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[mlink-daemon] shutting down");
                Ok(())
            }
        };
        remove_daemon_info(&info_path);
        result
    }
}

/// Best-effort hostname lookup, used as the `name` passed to the `Node`. Falls
/// back to a generic label when no OS-level hint is available (common in CI
/// containers).
pub fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "mlink-daemon".into())
}
