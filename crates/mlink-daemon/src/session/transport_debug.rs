//! Transport debug WS surface (contract §12/§14) — upstream commands
//! `transport_switch` and `disconnect_link`. The third (`transport_list`)
//! lives in `transport_list.rs` to keep each file under the 200-line rule;
//! the event forwarder (`primary_changed` / `stream_progress`) is in
//! `transport_forward.rs`.
//!
//! `transport_switch` takes a link *kind* on the wire (ble/tcp/ipc) — we
//! resolve it to a concrete `link_id` via `Node::peer_link_status` before
//! calling `Node::switch_active_link` (which takes an id). `disconnect_link`
//! is gated behind `MLINK_DAEMON_DEV=1` per contract §12 safety rules.

use axum::extract::ws::WebSocket;
use mlink_core::core::link::TransportKind;
use serde::Deserialize;
use serde_json::Value;

use super::outbound::{send, send_ack_if_id};
use crate::protocol::{codes, encode_frame, ErrorPayload};
use crate::DaemonState;

pub(super) use super::transport_list::handle_transport_list;

#[derive(Debug, Deserialize)]
struct SwitchPayload {
    peer_id: String,
    to_kind: String,
}

#[derive(Debug, Deserialize)]
struct DisconnectPayload {
    peer_id: String,
    link_id: String,
}

pub(super) async fn handle_transport_switch(
    socket: &mut WebSocket,
    state: &DaemonState,
    id: Option<&str>,
    payload: Value,
) -> bool {
    let p: SwitchPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => return send_error(socket, id, codes::BAD_PAYLOAD, e.to_string()).await,
    };
    let Some(target_kind) = TransportKind::from_wire(&p.to_kind) else {
        return send_error(
            socket,
            id,
            codes::BAD_PAYLOAD,
            format!("unknown to_kind '{}': expected ble|tcp|ipc", p.to_kind),
        )
        .await;
    };
    // Resolve kind → concrete link_id. "no {kind} link" is a distinct failure
    // mode from the generic switch error so the UI can render a clean toast.
    let statuses = state.node.peer_link_status(&p.peer_id).await;
    let Some(target) = statuses.iter().find(|s| s.kind == target_kind) else {
        return send_error(
            socket,
            id,
            codes::SEND_FAILED,
            format!("peer {} has no {} link", p.peer_id, p.to_kind),
        )
        .await;
    };
    if let Err(e) = state
        .node
        .switch_active_link(&p.peer_id, &target.link_id)
        .await
    {
        return send_error(
            socket,
            id,
            codes::SEND_FAILED,
            format!("switch failed: {e}"),
        )
        .await;
    }
    send_ack_if_id(socket, id, "transport_switch").await
}

pub(super) async fn handle_disconnect_link(
    socket: &mut WebSocket,
    state: &DaemonState,
    id: Option<&str>,
    payload: Value,
) -> bool {
    if std::env::var("MLINK_DAEMON_DEV").ok().as_deref() != Some("1") {
        return send_error(
            socket,
            id,
            codes::BAD_TYPE,
            "disconnect_link requires MLINK_DAEMON_DEV=1".into(),
        )
        .await;
    }
    let p: DisconnectPayload = match serde_json::from_value(payload) {
        Ok(p) => p,
        Err(e) => return send_error(socket, id, codes::BAD_PAYLOAD, e.to_string()).await,
    };
    if let Err(e) = state.node.disconnect_link(&p.peer_id, &p.link_id).await {
        return send_error(
            socket,
            id,
            codes::SEND_FAILED,
            format!("disconnect_link failed: {e}"),
        )
        .await;
    }
    send_ack_if_id(socket, id, "disconnect_link").await
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
    fn switch_payload_round_trips() {
        let v = serde_json::json!({"peer_id":"p","to_kind":"ble"});
        let p: SwitchPayload = serde_json::from_value(v).expect("parse");
        assert_eq!(p.peer_id, "p");
        assert_eq!(p.to_kind, "ble");
    }

    #[test]
    fn disconnect_payload_requires_both_fields() {
        let bad = serde_json::json!({"peer_id":"p"});
        assert!(serde_json::from_value::<DisconnectPayload>(bad).is_err());
        let ok = serde_json::json!({"peer_id":"p","link_id":"l"});
        assert!(serde_json::from_value::<DisconnectPayload>(ok).is_ok());
    }

    #[test]
    fn transport_kind_from_wire_covers_protocol_set() {
        for (wire, kind) in [
            ("ble", TransportKind::Ble),
            ("tcp", TransportKind::Tcp),
            ("ipc", TransportKind::Ipc),
        ] {
            assert_eq!(TransportKind::from_wire(wire), Some(kind));
        }
        assert_eq!(TransportKind::from_wire("wifi"), None);
    }
}
