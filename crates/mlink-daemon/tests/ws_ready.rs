//! End-to-end smoke test: stand up the axum WS server on a random port, open a
//! client, and verify the `ready` frame shape. This is intentionally not a
//! mock — it drives real tokio-tungstenite against real axum over a real
//! TcpListener. Per project rule: no mocks.

use futures_util::{SinkExt, StreamExt};
use mlink_daemon::{build_state, router, DAEMON_VERSION, WS_PROTOCOL_VERSION};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;

#[tokio::test]
async fn ws_sends_ready_frame_on_connect() {
    // Redirect the persistent rooms file to a temp path so the test cannot
    // mutate the user's real `~/.mlink/rooms.json`.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!("mlink-ready-test-rooms-{}.json", nanos));
    std::env::set_var("MLINK_ROOMS_FILE", &path);

    let state = build_state().await.expect("build_state");
    let app_uuid = state.node.app_uuid().to_string();
    let app = router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Small yield so the serve future begins accepting before we dial.
    tokio::task::yield_now().await;

    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _resp) = tokio_tungstenite::connect_async(url).await.expect("connect");

    let msg = ws.next().await.expect("one message").expect("no ws error");
    let text = match msg {
        Message::Text(t) => t,
        other => panic!("expected text frame, got {other:?}"),
    };

    let v: serde_json::Value = serde_json::from_str(&text).expect("valid json");
    assert_eq!(v["v"], WS_PROTOCOL_VERSION);
    assert_eq!(v["type"], "ready");
    assert_eq!(v["payload"]["version"], DAEMON_VERSION);
    assert_eq!(v["payload"]["app_uuid"], app_uuid);

    // Close cleanly so the server-side task doesn't log a recv error.
    ws.send(Message::Close(None)).await.ok();
}
