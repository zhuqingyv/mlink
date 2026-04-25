//! `transport_list` WS handler. Emits a `transport_state` snapshot (one entry
//! per connected peer, each with its links / active link / RTT / err rate)
//! followed by the ack. Split out of `transport_debug.rs` so each file stays
//! within the 200-line rule.

use axum::extract::ws::WebSocket;
use mlink_core::core::session::types::LinkRole;
use serde::Serialize;

use super::outbound::{send, send_ack_if_id};
use crate::protocol::encode_frame;
use crate::DaemonState;

#[derive(Debug, Serialize)]
struct LinkWire<'a> {
    link_id: &'a str,
    transport_id: &'static str,
    role: &'static str,
    rtt_ema_us: u32,
    err_rate_milli: u16,
}

#[derive(Debug, Serialize)]
struct PeerLinks<'a> {
    peer_id: &'a str,
    active_link_id: Option<String>,
    links: Vec<LinkWire<'a>>,
}

pub(super) async fn handle_transport_list(
    socket: &mut WebSocket,
    state: &DaemonState,
    id: Option<&str>,
) -> bool {
    let peers = state.node.peers().await;
    let mut entries: Vec<serde_json::Value> = Vec::with_capacity(peers.len());
    for peer in &peers {
        let statuses = state.node.peer_link_status(&peer.app_uuid).await;
        let (links, active_id) = if statuses.is_empty() {
            // Single-link peer without a Session yet: synthesize one entry
            // from `Peer.transport_id`. Multi-link peers use SessionManager data.
            (
                vec![LinkWire {
                    link_id: &peer.id,
                    transport_id: wire_from_label(&peer.transport_id),
                    role: "active",
                    rtt_ema_us: 0,
                    err_rate_milli: 0,
                }],
                Some(peer.id.clone()),
            )
        } else {
            let links: Vec<LinkWire> = statuses
                .iter()
                .map(|s| LinkWire {
                    link_id: &s.link_id,
                    transport_id: s.kind.as_wire(),
                    role: match s.role {
                        LinkRole::Active => "active",
                        LinkRole::Standby => "standby",
                    },
                    rtt_ema_us: s.health.rtt_ema_us,
                    err_rate_milli: s.health.err_rate_milli,
                })
                .collect();
            let active = statuses
                .iter()
                .find(|s| s.role == LinkRole::Active)
                .map(|s| s.link_id.clone());
            (links, active)
        };
        entries.push(
            serde_json::to_value(PeerLinks {
                peer_id: &peer.app_uuid,
                active_link_id: active_id,
                links,
            })
            .unwrap_or(serde_json::Value::Null),
        );
    }

    if !send(
        socket,
        encode_frame(
            "transport_state",
            id,
            serde_json::json!({ "peers": entries }),
        ),
    )
    .await
    {
        return false;
    }
    send_ack_if_id(socket, id, "transport_list").await
}

fn wire_from_label(label: &str) -> &'static str {
    match label {
        "ble" => "ble",
        "tcp" => "tcp",
        "ipc" => "ipc",
        _ => "tcp",
    }
}

#[cfg(test)]
mod tests {
    use super::wire_from_label;

    #[test]
    fn wire_from_label_defaults_to_tcp() {
        assert_eq!(wire_from_label("ble"), "ble");
        assert_eq!(wire_from_label("tcp"), "tcp");
        assert_eq!(wire_from_label("ipc"), "ipc");
        assert_eq!(wire_from_label("zigbee"), "tcp");
        assert_eq!(wire_from_label(""), "tcp");
    }
}
