use axum::extract::ws::WebSocket;
use mlink_core::protocol::types::MessageType;
use serde_json::Value;

use super::outbound::{send, send_ack_if_id, send_bad_payload};
use super::validate::valid_room_code;
use crate::protocol::{codes, encode_frame, ErrorPayload, SendPayload};
use crate::{DaemonState, SessionHandle};

pub(super) async fn handle_send(
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
