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

pub mod bootstrap;
pub mod discovery;
pub mod http;
pub mod lifecycle;
pub mod protocol;
pub mod queue;
pub mod rooms;
pub mod session;
pub mod state;
pub mod subscription;

pub use bootstrap::*;
pub use http::*;
pub use lifecycle::*;
pub use state::*;
pub use subscription::*;

/// Main entrypoint. Binds a random port (unless `MLINK_DAEMON_PORT` is set),
/// records `{port, pid}` in the daemon info file, serves WS until the task is
/// cancelled, then cleans up the file on exit.
pub async fn run() -> Result<(), DaemonError> {
    let (_port, serve) = bind_and_prepare().await?;
    serve.await_shutdown().await
}
