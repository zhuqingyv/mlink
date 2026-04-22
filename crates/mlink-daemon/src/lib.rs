//! mlink-daemon — long-running process that owns a single `Node` and exposes
//! it to local clients over WebSocket. This skeleton wires up:
//!
//! 1. `DaemonState`  — shared `Arc<Node>` + a `broadcast::Sender<NodeEvent>`
//!    that fans Node events out to every connected WS client.
//! 2. Single-instance lock — `~/.mlink/daemon.json` carries `{port, pid}`; we
//!    refuse to start if a previous daemon's PID is still alive. Stale files
//!    are cleaned up automatically.
//! 3. `GET /ws`     — axum WebSocket upgrade that sends a `ready` frame on
//!    connect and logs any inbound message. Actual command handling is
//!    intentionally out of scope for this skeleton.

pub mod discovery;
pub mod protocol;
pub mod queue;
pub mod rooms;
pub mod session;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use mlink_core::core::node::{Node, NodeConfig, NodeEvent};
use mlink_core::core::room::room_hash;
use mlink_core::protocol::errors::MlinkError;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};

use crate::protocol::{encode_frame, MessagePayload};
use crate::queue::{MessageEntry, MessageQueue};
use crate::rooms::RoomStore;

/// Wire version for the WS protocol envelope. Incremented only on a breaking
/// change to the frame shape.
pub const WS_PROTOCOL_VERSION: u32 = 1;
/// Semver-ish string emitted in the `ready` payload. Bumped per release of the
/// daemon crate — kept in sync with `Cargo.toml` manually for now.
pub const DAEMON_VERSION: &str = "0.1.0";

/// Broadcast channel capacity for fanning `NodeEvent`s to WS clients. Matches
/// the order of magnitude of Node's internal broadcast; if a client lags
/// beyond this the `recv()` yields `Lagged` and we simply skip ahead.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// One WS session's subscription state, shared between the session task and
/// the central fan-out worker. The worker reads `subs` to decide whether to
/// push a routed message into `tx`; the session task mutates `subs` on
/// join/leave. `tx` is the session's outbound WS queue — dropping it causes
/// the worker to prune the entry on next fan-out.
pub struct SessionEntry {
    /// Room codes this WS client is currently subscribed to.
    pub subs: HashSet<String>,
    /// Outbound text frames destined for this session's WebSocket.
    pub tx: mpsc::Sender<String>,
}

/// Shared handle to a session's subscription state. The list in
/// `DaemonState::sessions` holds one of these per live WS connection; the
/// pointer is also held by the session task itself for mutation.
pub type SessionHandle = Arc<StdMutex<SessionEntry>>;

/// State shared across WS connections. `Node` is behind `Arc` so we can clone
/// the handle into each connection task; `node_events` is the broadcast
/// sender fed by a background bridge task that reads from `Node::subscribe()`.
#[derive(Clone)]
pub struct DaemonState {
    pub node: Arc<Node>,
    pub node_events: broadcast::Sender<NodeEvent>,
    pub sessions: Arc<StdMutex<Vec<SessionHandle>>>,
    /// Persistent room membership. Mutated by WS join/leave so the set
    /// survives daemon restarts; loaded and auto-joined in `build_state`
    /// before discovery boots.
    pub rooms: Arc<StdMutex<RoomStore>>,
    /// Per-room backlog. Incoming peer messages are appended here before
    /// being fanned to subscribers, and `join` drains the bucket so a client
    /// that reconnects doesn't lose messages received while it was gone.
    pub queue: Arc<StdMutex<MessageQueue>>,
}

/// Contents of `~/.mlink/daemon.json`. Written on startup, removed on clean
/// shutdown. A stale file with a dead PID is overwritten rather than causing
/// the new instance to abort.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub pid: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another mlink-daemon is already running (pid={pid}, port={port})")]
    AlreadyRunning { pid: u32, port: u16 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mlink error: {0}")]
    Mlink(#[from] MlinkError),
    #[error("could not resolve home directory")]
    NoHome,
}

/// Default path for the daemon-info file. Honours `MLINK_DAEMON_FILE` when
/// set, otherwise falls back to `~/.mlink/daemon.json`. The override exists
/// primarily for integration tests that run the daemon in a tempdir.
pub fn daemon_info_path() -> Result<PathBuf, DaemonError> {
    if let Ok(p) = std::env::var("MLINK_DAEMON_FILE") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or(DaemonError::NoHome)?;
    Ok(home.join(".mlink").join("daemon.json"))
}

/// Return true if a process with the given PID is still alive. Uses
/// `kill(pid, 0)` on Unix which returns 0 for a live PID, ESRCH for a dead
/// one, and EPERM when the PID exists but we can't signal it — the last case
/// still means "alive", so we treat ESRCH alone as proof of death.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Safety: `kill(pid, 0)` is a safe syscall wrapper with no side effects —
    // signal 0 is the "just check permissions/existence" probe.
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if res == 0 {
        return true;
    }
    // errno == ESRCH → no such process. Anything else (EPERM) → alive.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    // Windows support deferred — conservatively assume "not alive" so we
    // don't wedge a user who has no way to clear the file.
    false
}

/// Read `daemon.json` and, if it points at a live PID, refuse to start.
/// Returns `Ok(())` when the file doesn't exist, is unparseable, or the PID
/// is dead — any of which mean we can safely claim the slot.
pub fn ensure_single_instance(path: &PathBuf) -> Result<(), DaemonError> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let info: DaemonInfo = match serde_json::from_slice(&raw) {
        Ok(i) => i,
        // Corrupt file — treat as stale and let the caller overwrite.
        Err(_) => return Ok(()),
    };
    if pid_alive(info.pid) {
        return Err(DaemonError::AlreadyRunning { pid: info.pid, port: info.port });
    }
    Ok(())
}

/// Atomically-ish write the daemon info file. We write to a sibling tempfile
/// then rename so a crashed write doesn't leave a half-JSON blob on disk.
pub fn write_daemon_info(path: &PathBuf, info: &DaemonInfo) -> Result<(), DaemonError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(info)?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Best-effort cleanup. Ignores NotFound so a double-shutdown is idempotent.
pub fn remove_daemon_info(path: &PathBuf) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "failed to remove daemon info"),
    }
}

/// Inlined web-debug page. Served at `GET /` so `mlink dev` can open a browser
/// straight at the daemon with no external file dependency — handy for ad-hoc
/// debugging without checking out the repo. Kept as a raw `include_str!` so
/// any change to the HTML is picked up on the next build.
pub const WEB_DEBUG_HTML: &str = include_str!("../../../examples/web-debug/index.html");

/// Build the axum router. Split out so tests can mount the same routes on an
/// arbitrary listener without going through `run()`.
pub fn router(state: DaemonState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn index_handler() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        WEB_DEBUG_HTML,
    )
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<DaemonState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| session::run(socket, state))
}

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
    // one scanner/peripheral pair. `DaemonTransport::from_env` is permissive —
    // an unknown value silently falls back to TCP (safe default for CI).
    discovery::spawn(Arc::clone(&node), discovery::DaemonTransport::from_env());

    Ok(DaemonState {
        node,
        node_events: tx,
        sessions,
        rooms,
        queue,
    })
}

fn host_name() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "mlink-daemon".into())
}

/// Main entrypoint. Binds a random port (unless `MLINK_DAEMON_PORT` is set),
/// records `{port, pid}` in the daemon info file, serves WS until the task is
/// cancelled, then cleans up the file on exit.
pub async fn run() -> Result<(), DaemonError> {
    let (_port, serve) = bind_and_prepare().await?;
    serve.await_shutdown().await
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_info_roundtrip() {
        let dir = tempdir();
        let path = dir.join("daemon.json");
        let info = DaemonInfo { port: 12345, pid: std::process::id() };
        write_daemon_info(&path, &info).unwrap();
        // Our own PID is alive → ensure_single_instance must refuse.
        let err = ensure_single_instance(&path).unwrap_err();
        match err {
            DaemonError::AlreadyRunning { pid, port } => {
                assert_eq!(pid, info.pid);
                assert_eq!(port, info.port);
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        remove_daemon_info(&path);
    }

    #[test]
    fn stale_pid_is_ignored() {
        let dir = tempdir();
        let path = dir.join("daemon.json");
        // PID 0 is never a valid user process — pid_alive returns false, so
        // ensure_single_instance should succeed.
        let info = DaemonInfo { port: 54321, pid: 0 };
        write_daemon_info(&path, &info).unwrap();
        ensure_single_instance(&path).expect("stale pid should not block startup");
    }

    #[test]
    fn missing_file_is_ok() {
        let dir = tempdir();
        let path = dir.join("nope.json");
        ensure_single_instance(&path).expect("missing file should not block startup");
    }

    #[test]
    fn corrupt_file_is_ok() {
        let dir = tempdir();
        let path = dir.join("daemon.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not valid json").unwrap();
        ensure_single_instance(&path).expect("corrupt file should not block startup");
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "mlink-daemon-test-{}-{}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn uniq() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }
}
