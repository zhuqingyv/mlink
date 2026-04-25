use axum::extract::ws::WebSocket;

use super::handlers::{handle_hello, handle_join, handle_leave, handle_ping};
use super::handlers_send::handle_send;
use super::outbound::send;
use super::transport_debug::{
    handle_disconnect_link, handle_transport_list, handle_transport_switch,
};
use crate::protocol::{codes, encode_frame, Envelope, ErrorPayload};
use crate::{DaemonState, SessionHandle, WS_PROTOCOL_VERSION};

/// Process one inbound text frame. Returns `false` when the socket went away
/// mid-send, signalling the caller to exit the outer loop.
pub(super) async fn handle_text(
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
        "transport_list" => handle_transport_list(socket, state, env.id.as_deref()).await,
        "transport_switch" => {
            handle_transport_switch(socket, state, env.id.as_deref(), env.payload).await
        }
        "disconnect_link" => {
            handle_disconnect_link(socket, state, env.id.as_deref(), env.payload).await
        }
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
