use axum::extract::ws::{Message, WebSocket};

use crate::protocol::{
    codes, encode_frame, AckPayload, ErrorPayload, MessagePayload, PeerInfo, RoomStatePayload,
};
use crate::queue::MessageEntry;
use crate::DaemonState;

pub(super) async fn send_backlog_entry(socket: &mut WebSocket, entry: &MessageEntry) -> bool {
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

pub(super) async fn send_room_state(
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

pub(super) async fn send_ack_if_id(socket: &mut WebSocket, id: Option<&str>, ty: &str) -> bool {
    let Some(id) = id else { return true; };
    let frame = encode_frame("ack", Some(id), AckPayload { id, ty });
    send(socket, frame).await
}

pub(super) async fn send_bad_payload(
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

pub(super) async fn send(socket: &mut WebSocket, text: String) -> bool {
    socket.send(Message::Text(text)).await.is_ok()
}
