use axum::extract::ws::WebSocket;
use mlink_core::core::node::NodeEvent;

use super::outbound::send_room_state;
use crate::{DaemonState, SessionHandle};

/// Forward peer connect/disconnect events. `MessageReceived` is deliberately
/// ignored here — message routing goes through the central fan-out worker
/// that queues and dispatches per-subscription, so handling it here would
/// either duplicate delivery or race with the queue path.
///
/// On PeerConnected / PeerDisconnected we push a fresh `room_state` containing
/// the *real* peer list (from `Node::peers()`) to every room this session is
/// subscribed to. The Node does not track a peer→room map, but every connected
/// peer has already passed the daemon's room-hash intersection check during
/// handshake — so reporting the full connected set per subscribed room is
/// consistent with what the daemon is actually willing to route.
pub(super) async fn forward_peer_event(
    socket: &mut WebSocket,
    state: &DaemonState,
    handle: &SessionHandle,
    ev: NodeEvent,
) -> bool {
    let codes: Vec<String> = {
        let entry = handle.lock().expect("session poisoned");
        if entry.subs.is_empty() {
            return true;
        }
        entry.subs.iter().cloned().collect()
    };
    match ev {
        NodeEvent::PeerConnected { .. } | NodeEvent::PeerDisconnected { .. } => {
            for code in &codes {
                if !send_room_state(socket, state, code, true).await {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}
