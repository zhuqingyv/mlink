//! Integration tests for the dual-transport debug WS surface (contract §12):
//! `transport_list`, `transport_switch`, `disconnect_link`. No mocks — we spin
//! up a real daemon and exercise the wire-level protocol end-to-end.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mlink_daemon::{build_state, router};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

type Ws = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect() -> Ws {
    redirect_rooms_file();
    let state = build_state().await.expect("build_state");
    let app = router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::task::yield_now().await;

    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");
    let _ready = read_json(&mut ws).await;
    ws
}

fn redirect_rooms_file() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mlink-ws-transport-test-rooms-{}-{}.json",
            std::process::id(),
            nanos
        ));
        std::env::set_var("MLINK_ROOMS_FILE", &path);
    });
}

async fn send_json(ws: &mut Ws, v: Value) {
    ws.send(Message::Text(v.to_string())).await.expect("send");
}

async fn read_json(ws: &mut Ws) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("timeout")
        .expect("stream end")
        .expect("ws error");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).expect("json"),
        other => panic!("expected text, got {other:?}"),
    }
}

async fn read_until_type(ws: &mut Ws, want: &str) -> Value {
    for _ in 0..16 {
        let f = read_json(ws).await;
        if f["type"] == want {
            return f;
        }
    }
    panic!("no frame of type {want} within 16 reads");
}

#[tokio::test]
async fn transport_list_returns_empty_snapshot_on_fresh_daemon() {
    // With no peers attached, `transport_list` should return a transport_state
    // frame carrying an empty peers array, followed by the ack.
    let mut ws = connect().await;
    send_json(&mut ws, json!({"v":1,"id":"t1","type":"transport_list","payload":{}})).await;
    let snap = read_until_type(&mut ws, "transport_state").await;
    assert_eq!(snap["payload"]["peers"].as_array().unwrap().len(), 0);
    let ack = read_until_type(&mut ws, "ack").await;
    assert_eq!(ack["id"], "t1");
    assert_eq!(ack["payload"]["type"], "transport_list");
}

#[tokio::test]
async fn transport_switch_rejects_unknown_kind() {
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({
            "v":1, "id":"s1", "type":"transport_switch",
            "payload":{"peer_id":"x","to_kind":"wifi"}
        }),
    )
    .await;
    let err = read_until_type(&mut ws, "error").await;
    assert_eq!(err["payload"]["code"], "bad_payload");
    assert_eq!(err["id"], "s1");
}

#[tokio::test]
async fn transport_switch_fails_for_unknown_peer() {
    // Unknown peer → SessionManager has no status → handler surfaces
    // `send_failed` with a "no {kind} link" message the UI can render.
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({
            "v":1, "id":"s2", "type":"transport_switch",
            "payload":{"peer_id":"ghost","to_kind":"tcp"}
        }),
    )
    .await;
    let err = read_until_type(&mut ws, "error").await;
    assert_eq!(err["payload"]["code"], "send_failed");
    assert!(err["payload"]["message"]
        .as_str()
        .unwrap()
        .contains("no tcp link"));
}

#[tokio::test]
async fn transport_switch_rejects_malformed_payload() {
    // `to_kind` missing — should surface as bad_payload, not panic.
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"s3", "type":"transport_switch", "payload":{"peer_id":"x"}}),
    )
    .await;
    let err = read_until_type(&mut ws, "error").await;
    assert_eq!(err["payload"]["code"], "bad_payload");
}

#[tokio::test]
async fn disconnect_link_blocked_without_dev_flag() {
    // `disconnect_link` is dev-only. With MLINK_DAEMON_DEV unset (or !="1"),
    // the daemon must reject it with bad_type per contract §12 safety notes.
    std::env::remove_var("MLINK_DAEMON_DEV");
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({
            "v":1, "id":"d1", "type":"disconnect_link",
            "payload":{"peer_id":"x","link_id":"l"}
        }),
    )
    .await;
    let err = read_until_type(&mut ws, "error").await;
    assert_eq!(err["payload"]["code"], "bad_type");
    assert!(err["payload"]["message"]
        .as_str()
        .unwrap()
        .contains("MLINK_DAEMON_DEV=1"));
}

#[tokio::test]
async fn unknown_wire_still_gets_bad_type_via_dispatch() {
    // Regression guard: making sure adding 3 new types didn't accidentally
    // make the default branch (for anything else) stop working.
    let mut ws = connect().await;
    send_json(&mut ws, json!({"v":1,"id":"u","type":"jukebox","payload":{}})).await;
    let err = read_until_type(&mut ws, "error").await;
    assert_eq!(err["payload"]["code"], "bad_type");
    assert_eq!(err["id"], "u");
}
