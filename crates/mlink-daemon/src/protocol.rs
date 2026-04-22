//! WebSocket protocol v1 envelope + typed payloads.
//!
//! Frame shape: `{"v": 1, "id"?, "type": "...", "payload": {...}}`.
//! Upstream (client → daemon) types: hello, join, leave, send, ping.
//! Downstream (daemon → client) types: ready, ack, error, room_state, message, pong.
//!
//! `payload` is deliberately `serde_json::Value` so the `send` path can pass
//! arbitrary JSON through to peers without a base64 detour.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::WS_PROTOCOL_VERSION;

/// Inbound frame parsed off the WS socket. `v` must equal `WS_PROTOCOL_VERSION`;
/// `id` is an optional client-chosen request id echoed back on ack/error so the
/// caller can correlate request/response on a multiplexed socket.
#[derive(Debug, Deserialize)]
pub struct Envelope {
    pub v: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Deserialize, Default)]
pub struct HelloPayload {
    #[serde(default)]
    pub client_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RoomPayload {
    pub room: String,
}

#[derive(Debug, Deserialize)]
pub struct SendPayload {
    pub room: String,
    pub payload: Value,
    #[serde(default)]
    pub to: Option<String>,
}

/// Well-known error codes. Stable strings — clients match on these.
pub mod codes {
    pub const BAD_JSON: &str = "bad_json";
    pub const BAD_VERSION: &str = "bad_version";
    pub const BAD_TYPE: &str = "bad_type";
    pub const BAD_PAYLOAD: &str = "bad_payload";
    pub const BAD_ROOM: &str = "bad_room";
    pub const NOT_JOINED: &str = "not_joined";
    pub const SEND_FAILED: &str = "send_failed";
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
}

#[derive(Debug, Serialize)]
struct OutFrame<'a, T: Serialize> {
    v: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    #[serde(rename = "type")]
    ty: &'static str,
    payload: T,
}

pub fn encode_frame<T: Serialize>(ty: &'static str, id: Option<&str>, payload: T) -> String {
    let frame = OutFrame { v: WS_PROTOCOL_VERSION, id, ty, payload };
    serde_json::to_string(&frame).unwrap_or_else(|_| {
        // Fallback when payload is non-serializable. Should not happen with our
        // own types; a broken client-supplied Value in `send` is already
        // validated before we reach here.
        format!(
            "{{\"v\":{},\"type\":\"error\",\"payload\":{{\"code\":\"internal\",\"message\":\"serialize failed\"}}}}",
            WS_PROTOCOL_VERSION
        )
    })
}

#[derive(Debug, Serialize)]
pub struct AckPayload<'a> {
    pub id: &'a str,
    #[serde(rename = "type")]
    pub ty: &'a str,
}

#[derive(Debug, Serialize)]
pub struct ErrorPayload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<&'a str>,
    pub code: &'a str,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct PeerInfo {
    pub app_uuid: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct RoomStatePayload<'a> {
    pub room: &'a str,
    pub peers: Vec<PeerInfo>,
    pub joined: bool,
}

#[derive(Debug, Serialize)]
pub struct MessagePayload<'a> {
    pub room: &'a str,
    pub from: String,
    pub payload: Value,
    pub ts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_parses_with_optional_id() {
        let s = r#"{"v":1,"type":"ping","payload":{}}"#;
        let e: Envelope = serde_json::from_str(s).unwrap();
        assert_eq!(e.v, 1);
        assert_eq!(e.ty, "ping");
        assert!(e.id.is_none());
    }

    #[test]
    fn envelope_parses_with_id() {
        let s = r#"{"v":1,"id":"req-7","type":"hello","payload":{}}"#;
        let e: Envelope = serde_json::from_str(s).unwrap();
        assert_eq!(e.id.as_deref(), Some("req-7"));
    }

    #[test]
    fn envelope_rejects_missing_type() {
        let s = r#"{"v":1,"payload":{}}"#;
        assert!(serde_json::from_str::<Envelope>(s).is_err());
    }

    #[test]
    fn room_payload_requires_room() {
        let s = r#"{"room":"123456"}"#;
        let r: RoomPayload = serde_json::from_str(s).unwrap();
        assert_eq!(r.room, "123456");
    }

    #[test]
    fn send_payload_to_is_optional() {
        let s = r#"{"room":"123456","payload":{"hi":1}}"#;
        let p: SendPayload = serde_json::from_str(s).unwrap();
        assert_eq!(p.room, "123456");
        assert!(p.to.is_none());
    }

    #[test]
    fn encode_frame_shape() {
        let s = encode_frame("pong", None, serde_json::json!({}));
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["type"], "pong");
        assert!(v.get("id").is_none());
    }

    #[test]
    fn encode_frame_with_id() {
        let s = encode_frame("ack", Some("req-1"), AckPayload { id: "req-1", ty: "hello" });
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["id"], "req-1");
        assert_eq!(v["payload"]["id"], "req-1");
    }
}
