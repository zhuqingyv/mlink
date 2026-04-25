//! Background forwarder that re-publishes `SessionEvent::Switched` and
//! `StreamProgress` onto every connected WS session. Separate from
//! `transport_debug.rs` to keep each file under the 200-line rule.
//!
//! `LinkAdded` / `LinkRemoved` are deliberately NOT forwarded here — those
//! already reach each session via `NodeEvent::LinkAdded / LinkRemoved` on the
//! per-session event stream (the bridge spawned by `Node::spawn_session_bridge_for`
//! re-emits them on the NodeEvent broadcaster). Double-forwarding would cause
//! the web-debug UI to render each link change twice.

use std::sync::{Arc, Mutex as StdMutex};

use mlink_core::core::session::types::{SessionEvent, SwitchCause};

use crate::protocol::encode_frame;
use crate::{DaemonState, SessionHandle};

pub fn spawn_session_event_forwarder(state: DaemonState) {
    let mut rx = state.node.subscribe_session_events();
    let sessions = Arc::clone(&state.sessions);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(SessionEvent::Switched { peer_id, from, to, cause }) => {
                    let frame = encode_frame(
                        "primary_changed",
                        None,
                        serde_json::json!({
                            "peer_id": peer_id,
                            "old": from,
                            "new": to,
                            "cause": cause_wire(cause),
                        }),
                    );
                    push_all(&sessions, frame);
                }
                Ok(SessionEvent::StreamProgress { peer_id, stream_id, pct }) => {
                    let frame = encode_frame(
                        "stream_progress",
                        None,
                        serde_json::json!({
                            "peer_id": peer_id,
                            "stream_id": stream_id,
                            "pct": pct,
                        }),
                    );
                    push_all(&sessions, frame);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "session-event forwarder lagged");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

fn push_all(sessions: &Arc<StdMutex<Vec<SessionHandle>>>, frame: String) {
    let snap: Vec<_> = {
        let guard = sessions.lock().expect("sessions poisoned");
        guard.iter().map(Arc::clone).collect()
    };
    for s in snap {
        let tx = {
            let e = s.lock().expect("session poisoned");
            e.tx.clone()
        };
        let _ = tx.try_send(frame.clone());
    }
}

fn cause_wire(c: SwitchCause) -> &'static str {
    match c {
        SwitchCause::ActiveFailed => "active_failed",
        SwitchCause::BetterCandidate => "better_candidate",
        SwitchCause::ManualUserRequest => "manual",
    }
}
