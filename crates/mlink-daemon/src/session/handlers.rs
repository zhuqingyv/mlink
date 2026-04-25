use axum::extract::ws::WebSocket;
use mlink_core::core::room::room_hash;
use serde_json::Value;

use super::outbound::{
    send, send_ack_if_id, send_backlog_entry, send_bad_payload, send_room_state,
};
use super::validate::valid_room_code;
use crate::protocol::{codes, encode_frame, ErrorPayload, HelloPayload, RoomPayload};
use crate::{DaemonState, SessionHandle, DAEMON_VERSION};

pub(super) async fn handle_hello(
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

pub(super) async fn handle_join(
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

pub(super) async fn handle_leave(
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

pub(super) async fn handle_ping(socket: &mut WebSocket, id: Option<&str>) -> bool {
    send(socket, encode_frame("pong", id, serde_json::json!({}))).await
}
