//! Per-session `NodeEvent` → WS frame forwarder.
//!
//! - `PeerConnected` / `PeerDisconnected` → fresh `room_state` per subscribed
//!   room (peers list from `Node::peers()`, matches what the daemon routes).
//! - `LinkAdded` / `LinkRemoved` / `LinkSwitched` → the transport-debug frames
//!   contract §12 defines, pushed to every WS session so the debug UI sees
//!   connect/disconnect/switch regardless of room subscription.
//! - `MessageReceived` is ignored here — message routing runs through the
//!   central fan-out worker to avoid double delivery.

use axum::extract::ws::WebSocket;
use mlink_core::core::link::TransportKind;
use mlink_core::core::node::NodeEvent;
use mlink_core::core::session::types::SwitchCause;

use super::outbound::{send, send_room_state};
use crate::protocol::encode_frame;
use crate::{DaemonState, SessionHandle};

pub(super) async fn forward_peer_event(
    socket: &mut WebSocket,
    state: &DaemonState,
    handle: &SessionHandle,
    ev: NodeEvent,
) -> bool {
    // Transport-debug fan-out first — these frames go to every WS session
    // regardless of subscription so the debug UI always sees link changes.
    match &ev {
        NodeEvent::LinkAdded { peer_id, link_id, transport } => {
            let frame = encode_frame(
                "link_added",
                None,
                serde_json::json!({
                    "peer_id": peer_id,
                    "link_id": link_id,
                    "transport_id": kind_wire(*transport),
                }),
            );
            if !send(socket, frame).await {
                return false;
            }
        }
        NodeEvent::LinkRemoved { peer_id, link_id, reason } => {
            let frame = encode_frame(
                "link_removed",
                None,
                serde_json::json!({
                    "peer_id": peer_id,
                    "link_id": link_id,
                    "reason": reason,
                }),
            );
            if !send(socket, frame).await {
                return false;
            }
        }
        NodeEvent::LinkSwitched { peer_id, from_link, to_link, cause } => {
            let frame = encode_frame(
                "primary_changed",
                None,
                serde_json::json!({
                    "peer_id": peer_id,
                    "old": from_link,
                    "new": to_link,
                    "cause": cause_wire(*cause),
                }),
            );
            if !send(socket, frame).await {
                return false;
            }
        }
        _ => {}
    }

    // Room-state fan-out — only per-subscription.
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

fn kind_wire(k: TransportKind) -> &'static str {
    k.as_wire()
}

fn cause_wire(c: SwitchCause) -> &'static str {
    match c {
        SwitchCause::ActiveFailed => "active_failed",
        SwitchCause::BetterCandidate => "better_candidate",
        SwitchCause::ManualUserRequest => "manual",
    }
}
