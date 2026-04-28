//! `enable_transport` / `disable_transport` WS handlers — runtime switches
//! for the per-radio discovery loops. Payload is a `{kind: "ble"|"tcp"}`
//! object; the Node itself isn't touched, so existing connections keep
//! running when a transport is disabled (only the scanner / accept /
//! auto-connect loops for that radio stop).

use axum::extract::ws::WebSocket;
use serde::Deserialize;
use serde_json::Value;

use super::outbound::{send, send_ack_if_id};
use crate::discovery::TransportKindWire;
use crate::protocol::{codes, encode_frame, ErrorPayload};
use crate::DaemonState;

#[derive(Debug, Deserialize)]
struct TogglePayload {
    kind: String,
}

pub(super) async fn handle_enable_transport(
    socket: &mut WebSocket,
    state: &DaemonState,
    id: Option<&str>,
    payload: Value,
) -> bool {
    let Some(kind) = parse_kind(&payload) else {
        return send_error(socket, id, codes::BAD_PAYLOAD, kind_err(&payload)).await;
    };
    match state.transports.enable(kind) {
        Ok(_started) => send_ack_if_id(socket, id, "enable_transport").await,
        Err(msg) => send_error(socket, id, codes::SEND_FAILED, msg).await,
    }
}

pub(super) async fn handle_disable_transport(
    socket: &mut WebSocket,
    state: &DaemonState,
    id: Option<&str>,
    payload: Value,
) -> bool {
    let Some(kind) = parse_kind(&payload) else {
        return send_error(socket, id, codes::BAD_PAYLOAD, kind_err(&payload)).await;
    };
    state.transports.disable(kind);
    send_ack_if_id(socket, id, "disable_transport").await
}

fn parse_kind(payload: &Value) -> Option<TransportKindWire> {
    let p: TogglePayload = serde_json::from_value(payload.clone()).ok()?;
    TransportKindWire::from_wire(&p.kind)
}

fn kind_err(payload: &Value) -> String {
    match serde_json::from_value::<TogglePayload>(payload.clone()) {
        Ok(p) => format!("unknown kind '{}': expected ble|tcp", p.kind),
        Err(e) => e.to_string(),
    }
}

async fn send_error(
    socket: &mut WebSocket,
    id: Option<&str>,
    code: &'static str,
    message: String,
) -> bool {
    send(
        socket,
        encode_frame("error", id, ErrorPayload { id, code, message }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kind_accepts_both() {
        assert_eq!(
            parse_kind(&serde_json::json!({"kind":"tcp"})),
            Some(TransportKindWire::Tcp)
        );
        assert_eq!(
            parse_kind(&serde_json::json!({"kind":"ble"})),
            Some(TransportKindWire::Ble)
        );
    }

    #[test]
    fn parse_kind_rejects_unknown() {
        assert_eq!(parse_kind(&serde_json::json!({"kind":"wifi"})), None);
        assert_eq!(parse_kind(&serde_json::json!({})), None);
    }
}
