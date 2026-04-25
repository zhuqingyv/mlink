//! Per-session subscription state. Extracted from `lib.rs` so the fan-out
//! worker and WS session task can share a small, focused module instead of
//! depending on the entire daemon surface.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::mpsc;

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
