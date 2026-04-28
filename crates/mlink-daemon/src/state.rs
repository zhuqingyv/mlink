//! Shared daemon state: the `Arc<Node>` handle, WS session table, persistent
//! room store and in-memory message backlog, plus the small set of protocol
//! constants every WS session needs. Split out of `lib.rs` so the top-level
//! module can stay focused on wiring (router, single-instance lock, bind).

use std::sync::{Arc, Mutex as StdMutex};

use mlink_core::core::node::{Node, NodeEvent};
use tokio::sync::broadcast;

use crate::discovery::TransportControllers;
use crate::queue::MessageQueue;
use crate::rooms::RoomStore;
use crate::subscription::SessionHandle;

/// Wire version for the WS protocol envelope. Incremented only on a breaking
/// change to the frame shape.
pub const WS_PROTOCOL_VERSION: u32 = 1;
/// Semver-ish string emitted in the `ready` payload. Bumped per release of the
/// daemon crate — kept in sync with `Cargo.toml` manually for now.
pub const DAEMON_VERSION: &str = "0.1.0";

/// Broadcast channel capacity for fanning `NodeEvent`s to WS clients. Matches
/// the order of magnitude of Node's internal broadcast; if a client lags
/// beyond this the `recv()` yields `Lagged` and we simply skip ahead.
pub const EVENT_CHANNEL_CAPACITY: usize = 256;

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
    /// Runtime handle for the per-transport discovery loops. WS clients use
    /// `enable_transport` / `disable_transport` frames to toggle individual
    /// radios on and off without restarting the daemon.
    pub transports: TransportControllers,
}
