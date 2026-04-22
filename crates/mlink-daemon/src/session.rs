//! Per-connection WS session. Parses envelopes, routes to handlers, and
//! forwards Node events / routed backlog messages to the client based on its
//! room subscription set.
//!
//! Sessions no longer own the room on behalf of the daemon. Subscription is
//! purely a *routing filter*: the daemon is already joined to every room in
//! the persistent RoomStore, and backlog messages accumulate in `state.queue`
//! whether any WS client is listening or not. A `join` frame adds the code to
//! the client's sub set and drains any accumulated backlog into the socket;
//! `leave` removes the sub and, only if no other live session still holds the
//! code, retires the room from the Node + RoomStore. A WS disconnect removes
//! subs silently — daemon-side membership is unaffected.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::ws::{Message, WebSocket};
use futures::stream::StreamExt;
use mlink_core::core::node::NodeEvent;
use mlink_core::core::room::room_hash;
use mlink_core::protocol::types::MessageType;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::protocol::{
    codes, encode_frame, AckPayload, Envelope, ErrorPayload, HelloPayload, MessagePayload,
    PeerInfo, RoomPayload, RoomStatePayload, SendPayload,
};
use crate::queue::MessageEntry;
use crate::{DaemonState, SessionEntry, SessionHandle, DAEMON_VERSION, WS_PROTOCOL_VERSION};

/// Maximum accepted frame size, in bytes. Matches the product rule ("WS
/// single message upper bound 1 MB"). Oversize frames are rejected with
/// `payload_too_large` before being parsed.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Outbound mpsc capacity. Sized generously relative to `MAX_MESSAGES_PER_ROOM`
/// so a join-time drain of one full backlog fits without back-pressure, while
/// staying small enough that a wedged socket doesn't grow memory unboundedly.
const OUTBOUND_CAPACITY: usize = 1024;

pub async fn run(mut socket: WebSocket, state: DaemonState) {
    // ---- send `ready` -----
    let ready = encode_frame(
        "ready",
        None,
        serde_json::json!({
            "app_uuid": state.node.app_uuid(),
            "version": DAEMON_VERSION,
        }),
    );
    if socket.send(Message::Text(ready)).await.is_err() {
        return;
    }

    let (tx, mut rx) = mpsc::channel::<String>(OUTBOUND_CAPACITY);
    let handle: SessionHandle = Arc::new(StdMutex::new(SessionEntry {
        subs: HashSet::new(),
        tx,
    }));
    {
        let mut sessions = state.sessions.lock().expect("sessions poisoned");
        sessions.push(Arc::clone(&handle));
    }
    let mut events = state.node_events.subscribe();

    // `StreamExt::next` on WebSocket is cancel-safe, as is `mpsc::Receiver::recv`
    // and `broadcast::Receiver::recv`. All three drive one session lifetime —
    // dropping the socket breaks the loop and deregisters from the sessions
    // list.
    loop {
        tokio::select! {
            ws_msg = socket.next() => {
                let Some(ws_msg) = ws_msg else { break; };
                let msg = match ws_msg {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!(error = %e, "ws recv error");
                        break;
                    }
                };
                match msg {
                    Message::Text(t) => {
                        if t.len() > MAX_FRAME_BYTES {
                            let err = encode_frame(
                                "error",
                                None,
                                ErrorPayload {
                                    id: None,
                                    code: codes::PAYLOAD_TOO_LARGE,
                                    message: format!(
                                        "frame {} bytes exceeds {MAX_FRAME_BYTES}",
                                        t.len()
                                    ),
                                },
                            );
                            let _ = socket.send(Message::Text(err)).await;
                            continue;
                        }
                        if !handle_text(&mut socket, &state, &handle, &t).await {
                            break;
                        }
                    }
                    Message::Binary(b) => {
                        let err = encode_frame(
                            "error",
                            None,
                            ErrorPayload {
                                id: None,
                                code: codes::BAD_JSON,
                                message: format!("binary frames are not supported ({} bytes)", b.len()),
                            },
                        );
                        if socket.send(Message::Text(err)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            routed = rx.recv() => {
                // Messages delivered by the central fan-out worker. `None`
                // means every sender was dropped, which only happens at
                // shutdown — we'd be about to exit anyway.
                let Some(frame) = routed else { break; };
                if !send(&mut socket, frame).await {
                    break;
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        if !forward_peer_event(&mut socket, &handle, ev).await {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "session event bridge lagged");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    // Deregister this session. The fan-out worker will notice the dropped
    // sender on its next push and tolerate the failure, but removing the
    // entry here keeps the list bounded.
    {
        let mut sessions = state.sessions.lock().expect("sessions poisoned");
        sessions.retain(|s| !Arc::ptr_eq(s, &handle));
    }
    // Disconnect path never retires daemon-level rooms: the daemon owns the
    // membership and keeps its device-to-device connections alive even when
    // no WS client is listening. Subscription state dies with the session.
}

/// Process one inbound text frame. Returns `false` when the socket went away
/// mid-send, signalling the caller to exit the outer loop.
async fn handle_text(
    socket: &mut WebSocket,
    state: &DaemonState,
    handle: &SessionHandle,
    text: &str,
) -> bool {
    let env: Envelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            return send(
                socket,
                encode_frame(
                    "error",
                    None,
                    ErrorPayload { id: None, code: codes::BAD_JSON, message: e.to_string() },
                ),
            )
            .await;
        }
    };
    if env.v != WS_PROTOCOL_VERSION {
        return send(
            socket,
            encode_frame(
                "error",
                env.id.as_deref(),
                ErrorPayload {
                    id: env.id.as_deref(),
                    code: codes::BAD_VERSION,
                    message: format!("unsupported version {}", env.v),
                },
            ),
        )
        .await;
    }
    match env.ty.as_str() {
        "hello" => handle_hello(socket, state, env.id.as_deref(), env.payload).await,
        "join" => handle_join(socket, state, handle, env.id.as_deref(), env.payload).await,
        "leave" => handle_leave(socket, state, handle, env.id.as_deref(), env.payload).await,
        "send" => handle_send(socket, state, handle, env.id.as_deref(), env.payload).await,
        "ping" => handle_ping(socket, env.id.as_deref()).await,
        other => {
            send(
                socket,
                encode_frame(
                    "error",
                    env.id.as_deref(),
                    ErrorPayload {
                        id: env.id.as_deref(),
                        code: codes::BAD_TYPE,
                        message: format!("unknown message type '{other}'"),
                    },
                ),
            )
            .await
        }
    }
}

async fn handle_hello(
    socket: &mut WebSocket,
    state: &DaemonState,
    id: Option<&str>,
    payload: Value,
) -> bool {
    // `client_name` is currently informational (logged for debugging). We
    // still parse it so a malformed object is flagged rather than silently
    // ignored.
    let hello: HelloPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => return send_bad_payload(socket, id, e).await,
    };
    if let Some(n) = &hello.client_name {
        tracing::debug!(client_name = %n, "hello");
    }
    let ready = encode_frame(
        "ready",
        id,
        serde_json::json!({
            "app_uuid": state.node.app_uuid(),
            "version": DAEMON_VERSION,
        }),
    );
    if !send(socket, ready).await {
        return false;
    }
    send_ack_if_id(socket, id, "hello").await
}

async fn handle_join(
    socket: &mut WebSocket,
    state: &DaemonState,
    handle: &SessionHandle,
    id: Option<&str>,
    payload: Value,
) -> bool {
    let room: RoomPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => return send_bad_payload(socket, id, e).await,
    };
    if !valid_room_code(&room.room) {
        return send(
            socket,
            encode_frame(
                "error",
                id,
                ErrorPayload {
                    id,
                    code: codes::BAD_ROOM,
                    message: format!("invalid room code '{}': expected 6 digits", room.room),
                },
            ),
        )
        .await;
    }
    let hash = room_hash(&room.room);
    // Daemon-level membership is idempotent — add_room_hash dedupes and
    // RoomStore::add is a no-op if already present. We do it before touching
    // the queue so a just-arrived peer message (if any) is keyed correctly.
    state.node.add_room_hash(hash);
    state.rooms.lock().expect("rooms poisoned").add(&room.room);

    // Subscribe + drain atomically under the queue lock. Holding the queue
    // lock blocks the central fan-out worker, guaranteeing that any backlog
    // present at this moment has NOT been fanned to this session (subs was
    // empty for this room), and any backlog that arrives after we release
    // WILL be fanned (subs now contains the room). No duplication, no loss.
    let drained = {
        let mut q = state.queue.lock().expect("queue poisoned");
        {
            let mut entry = handle.lock().expect("session poisoned");
            entry.subs.insert(room.room.clone());
        }
        q.drain(&room.room)
    };

    if !send_ack_if_id(socket, id, "join").await {
        return false;
    }
    if !send_room_state(socket, state, &room.room, true).await {
        return false;
    }
    // Flush backlog. Ordering within a room is preserved because the queue is
    // FIFO and we send before returning control to the outer select!, i.e.
    // before any newly-routed message can reach the mpsc receiver.
    for entry in drained {
        if !send_backlog_entry(socket, &entry).await {
            return false;
        }
    }
    true
}

async fn handle_leave(
    socket: &mut WebSocket,
    state: &DaemonState,
    handle: &SessionHandle,
    id: Option<&str>,
    payload: Value,
) -> bool {
    let room: RoomPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => return send_bad_payload(socket, id, e).await,
    };
    if !valid_room_code(&room.room) {
        return send(
            socket,
            encode_frame(
                "error",
                id,
                ErrorPayload {
                    id,
                    code: codes::BAD_ROOM,
                    message: format!("invalid room code '{}': expected 6 digits", room.room),
                },
            ),
        )
        .await;
    }
    let hash = room_hash(&room.room);
    // Drop the subscription first, then — only if no other live session still
    // has this room subscribed — retire daemon-level membership. A WS client
    // that explicitly `leave`s has asked to stop caring about the room; when
    // the last such client goes, the daemon does too.
    let removed = {
        let mut entry = handle.lock().expect("session poisoned");
        entry.subs.remove(&room.room)
    };
    if removed {
        let still_used = {
            let sessions = state.sessions.lock().expect("sessions poisoned");
            sessions.iter().any(|s| {
                s.lock().expect("session poisoned").subs.contains(&room.room)
            })
        };
        if !still_used {
            state.node.remove_room_hash(&hash);
            state.rooms.lock().expect("rooms poisoned").remove(&room.room);
            // Free the backlog bucket too: nobody will ever drain it, and
            // new messages for that room would have nowhere routable to go
            // once the Node stops accepting peers on its hash.
            state.queue.lock().expect("queue poisoned").remove_room(&room.room);
        }
    }

    if !send_ack_if_id(socket, id, "leave").await {
        return false;
    }
    send_room_state(socket, state, &room.room, false).await
}

async fn handle_send(
    socket: &mut WebSocket,
    state: &DaemonState,
    handle: &SessionHandle,
    id: Option<&str>,
    payload: Value,
) -> bool {
    let send_payload: SendPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => return send_bad_payload(socket, id, e).await,
    };
    if !valid_room_code(&send_payload.room) {
        return send(
            socket,
            encode_frame(
                "error",
                id,
                ErrorPayload {
                    id,
                    code: codes::BAD_ROOM,
                    message: format!("invalid room code '{}': expected 6 digits", send_payload.room),
                },
            ),
        )
        .await;
    }
    let is_subbed = handle
        .lock()
        .expect("session poisoned")
        .subs
        .contains(&send_payload.room);
    if !is_subbed {
        return send(
            socket,
            encode_frame(
                "error",
                id,
                ErrorPayload {
                    id,
                    code: codes::NOT_JOINED,
                    message: format!("not joined to room {}", send_payload.room),
                },
            ),
        )
        .await;
    }

    // Build the wire payload once: the `payload` field travels verbatim as
    // UTF-8 JSON (rule: no base64 envelope), and is addressed to either one
    // peer or every currently-connected peer.
    let bytes = match serde_json::to_vec(&send_payload.payload) {
        Ok(b) => b,
        Err(e) => return send_bad_payload(socket, id, e).await,
    };

    let recipients: Vec<String> = match &send_payload.to {
        Some(peer) => vec![peer.clone()],
        None => state.node.peers().await.into_iter().map(|p| p.id).collect(),
    };
    if recipients.is_empty() {
        return send(
            socket,
            encode_frame(
                "error",
                id,
                ErrorPayload {
                    id,
                    code: codes::SEND_FAILED,
                    message: "no connected peers".into(),
                },
            ),
        )
        .await;
    }

    let mut last_err: Option<String> = None;
    let mut delivered = 0usize;
    for peer_id in &recipients {
        match state
            .node
            .send_raw(peer_id, MessageType::Message, &bytes)
            .await
        {
            Ok(()) => delivered += 1,
            Err(e) => last_err = Some(e.to_string()),
        }
    }
    if delivered == 0 {
        return send(
            socket,
            encode_frame(
                "error",
                id,
                ErrorPayload {
                    id,
                    code: codes::SEND_FAILED,
                    message: last_err.unwrap_or_else(|| "send failed".into()),
                },
            ),
        )
        .await;
    }

    send_ack_if_id(socket, id, "send").await
}

async fn handle_ping(socket: &mut WebSocket, id: Option<&str>) -> bool {
    send(socket, encode_frame("pong", id, serde_json::json!({}))).await
}

/// Forward peer connect/disconnect events. `MessageReceived` is deliberately
/// ignored here — message routing goes through the central fan-out worker
/// that queues and dispatches per-subscription, so handling it here would
/// either duplicate delivery or race with the queue path.
async fn forward_peer_event(
    socket: &mut WebSocket,
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
                if !send_room_state_empty_peers(socket, code, true).await {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}

async fn send_backlog_entry(socket: &mut WebSocket, entry: &MessageEntry) -> bool {
    let frame = encode_frame(
        "message",
        None,
        MessagePayload {
            room: &entry.room,
            from: entry.from.clone(),
            payload: entry.payload.clone(),
            ts: entry.ts,
        },
    );
    send(socket, frame).await
}

async fn send_room_state(
    socket: &mut WebSocket,
    state: &DaemonState,
    code: &str,
    joined: bool,
) -> bool {
    let peers: Vec<PeerInfo> = state
        .node
        .peers()
        .await
        .into_iter()
        .map(|p| PeerInfo { app_uuid: p.app_uuid, name: p.name })
        .collect();
    let frame = encode_frame(
        "room_state",
        None,
        RoomStatePayload { room: code, peers, joined },
    );
    send(socket, frame).await
}

async fn send_room_state_empty_peers(
    socket: &mut WebSocket,
    code: &str,
    joined: bool,
) -> bool {
    // Called from `forward_peer_event` which doesn't have `DaemonState` in
    // scope. Clients that care about the fresh peer list will already issue a
    // follow-up `join`; the event here is a cheap "something changed" nudge.
    let frame = encode_frame(
        "room_state",
        None,
        RoomStatePayload { room: code, peers: Vec::new(), joined },
    );
    send(socket, frame).await
}

async fn send_ack_if_id(socket: &mut WebSocket, id: Option<&str>, ty: &str) -> bool {
    let Some(id) = id else { return true; };
    let frame = encode_frame("ack", Some(id), AckPayload { id, ty });
    send(socket, frame).await
}

async fn send_bad_payload(
    socket: &mut WebSocket,
    id: Option<&str>,
    err: serde_json::Error,
) -> bool {
    send(
        socket,
        encode_frame(
            "error",
            id,
            ErrorPayload { id, code: codes::BAD_PAYLOAD, message: err.to_string() },
        ),
    )
    .await
}

async fn send(socket: &mut WebSocket, text: String) -> bool {
    socket.send(Message::Text(text)).await.is_ok()
}

fn valid_room_code(code: &str) -> bool {
    code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_room_code_accepts_six_digits() {
        assert!(valid_room_code("123456"));
        assert!(valid_room_code("000000"));
    }

    #[test]
    fn valid_room_code_rejects_length_and_non_digit() {
        assert!(!valid_room_code(""));
        assert!(!valid_room_code("12345"));
        assert!(!valid_room_code("1234567"));
        assert!(!valid_room_code("12345a"));
    }
}
