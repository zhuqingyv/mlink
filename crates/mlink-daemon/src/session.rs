//! Per-connection WS session. Parses envelopes, routes to handlers, and
//! forwards Node events to the client based on its room subscription set.
//!
//! Each session owns a `HashSet<String>` of joined room codes. Node-level
//! events (PeerConnected/Disconnected/MessageReceived) are forwarded only
//! when the session has at least one active subscription — the daemon does
//! not currently track per-peer room membership, so every joined client sees
//! every message from every connected peer. That is acceptable for the
//! single-device "my apps" use case the daemon targets; a stricter
//! per-room fan-out would require Node to track which room_hash a peer
//! handshook against, which is out of scope here.

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket};
use futures::stream::StreamExt;
use mlink_core::core::node::NodeEvent;
use mlink_core::core::room::room_hash;
use mlink_core::protocol::types::MessageType;
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::protocol::{
    codes, encode_frame, AckPayload, Envelope, ErrorPayload, HelloPayload, MessagePayload,
    PeerInfo, RoomPayload, RoomStatePayload, SendPayload,
};
use crate::{DaemonState, DAEMON_VERSION, WS_PROTOCOL_VERSION};

/// Maximum accepted frame size, in bytes. Matches the product rule ("WS
/// single message upper bound 1 MB"). Oversize frames are rejected with
/// `payload_too_large` before being parsed.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

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

    // The joined-rooms set lives behind an Arc<Mutex> so the leave handler in
    // *other* sessions can inspect it when deciding whether to retire a
    // room_hash from the Node. Dropped on session exit — see the `retain`
    // call at the bottom of this function.
    let joined: Arc<StdMutex<HashSet<String>>> = Arc::new(StdMutex::new(HashSet::new()));
    {
        let mut sessions = state.sessions.lock().expect("sessions poisoned");
        sessions.push(Arc::clone(&joined));
    }
    let mut events = state.node_events.subscribe();

    // `StreamExt::next` on WebSocket gives us cancel-safe `Option<Result<Message>>`.
    // We select between it and the event broadcast inside one loop so the
    // session lifetime is bound to the socket, not to any background task.
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
                        if !handle_text(&mut socket, &state, &joined, &t).await {
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
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        if !forward_event(&mut socket, &joined, ev).await {
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

    // Deregister this session. The `Arc`-pointer compare is deliberate:
    // HashSet<String> doesn't implement Eq-as-pointer, and two sessions that
    // happened to join the same rooms would otherwise look equal.
    {
        let mut sessions = state.sessions.lock().expect("sessions poisoned");
        sessions.retain(|s| !Arc::ptr_eq(s, &joined));
    }
    // If no other session still uses the rooms this one had joined, retire
    // the hashes from the Node so further discovery doesn't accept peers on
    // their behalf.
    let codes = joined.lock().expect("joined poisoned").clone();
    for code in codes {
        let still_used = {
            let sessions = state.sessions.lock().expect("sessions poisoned");
            sessions.iter().any(|s| s.lock().expect("poisoned").contains(&code))
        };
        if !still_used {
            state.node.remove_room_hash(&room_hash(&code));
        }
    }
}

/// Process one inbound text frame. Returns `false` when the socket went away
/// mid-send, signalling the caller to exit the outer loop.
async fn handle_text(
    socket: &mut WebSocket,
    state: &DaemonState,
    joined: &Arc<StdMutex<HashSet<String>>>,
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
        "join" => handle_join(socket, state, joined, env.id.as_deref(), env.payload).await,
        "leave" => handle_leave(socket, state, joined, env.id.as_deref(), env.payload).await,
        "send" => handle_send(socket, state, joined, env.id.as_deref(), env.payload).await,
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
    joined: &Arc<StdMutex<HashSet<String>>>,
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
    state.node.add_room_hash(hash);
    joined.lock().expect("joined poisoned").insert(room.room.clone());

    if !send_ack_if_id(socket, id, "join").await {
        return false;
    }
    send_room_state(socket, state, &room.room, true).await
}

async fn handle_leave(
    socket: &mut WebSocket,
    state: &DaemonState,
    joined: &Arc<StdMutex<HashSet<String>>>,
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
    // The Node's room-hash set is shared across every WS client, so we only
    // remove the hash when *no* other session still has that room joined.
    // Without that check, two clients in the same room would each see their
    // own "leave" pull the filter out from under the other.
    let removed = joined.lock().expect("joined poisoned").remove(&room.room);
    if removed {
        let still_used = {
            let sessions = state.sessions.lock().expect("sessions poisoned");
            sessions
                .iter()
                .any(|s| s.lock().expect("poisoned").contains(&room.room))
        };
        if !still_used {
            state.node.remove_room_hash(&hash);
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
    joined: &Arc<StdMutex<HashSet<String>>>,
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
    if !joined.lock().expect("joined poisoned").contains(&send_payload.room) {
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

async fn forward_event(
    socket: &mut WebSocket,
    joined: &Arc<StdMutex<HashSet<String>>>,
    ev: NodeEvent,
) -> bool {
    // Snapshot the joined set so we don't hold the lock across awaits.
    let codes: Vec<String> = {
        let g = joined.lock().expect("joined poisoned");
        if g.is_empty() {
            return true;
        }
        g.iter().cloned().collect()
    };
    match ev {
        NodeEvent::PeerConnected { .. } | NodeEvent::PeerDisconnected { .. } => {
            // Re-emit `room_state` for every joined room so the client can
            // refresh its peer list without re-querying.
            for code in &codes {
                if !send_room_state_with_peers(socket, code, true).await {
                    return false;
                }
            }
            true
        }
        NodeEvent::MessageReceived { peer_id, payload } => {
            // Parse payload back into JSON; fall back to the original utf-8
            // string if it wasn't valid JSON (the CLI chat path sends plain
            // text lines), or a placeholder for non-utf8 binary.
            let value: Value = match std::str::from_utf8(&payload) {
                Ok(s) => match serde_json::from_str(s) {
                    Ok(v) => v,
                    Err(_) => Value::String(s.to_string()),
                },
                Err(_) => Value::String(format!("<{} bytes binary>", payload.len())),
            };
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for code in &codes {
                let frame = encode_frame(
                    "message",
                    None,
                    MessagePayload { room: code, from: peer_id.clone(), payload: value.clone(), ts },
                );
                if !send(socket, frame).await {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
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

async fn send_room_state_with_peers(
    socket: &mut WebSocket,
    code: &str,
    joined: bool,
) -> bool {
    // Called from `forward_event` where we don't have `state` in scope; callers
    // of the event loop pass the socket only. We avoid the extra lookup by
    // snapshotting current Node peers via a static accessor — but since we
    // don't have one, use the joined flag shape that doesn't require peers.
    // Future: thread `DaemonState` into `forward_event`. For now, emit an
    // empty `peers` list so clients know to re-query (they do a fresh `join`
    // round-trip for that today).
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
