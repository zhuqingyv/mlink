//! End-to-end protocol smoke tests. We spin up a real daemon (random port,
//! real axum, real tokio-tungstenite) and exercise every ws message type the
//! protocol defines. No mocks — per project rule.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mlink_core::core::room::room_hash;
use mlink_daemon::queue::MessageEntry;
use mlink_daemon::{build_state, router, DaemonState};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::WebSocketStream;

type Ws = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Stand up a fresh daemon on localhost, return a connected WS with the
/// auto-sent `ready` frame already drained.
async fn connect() -> Ws {
    // MLINK_DAEMON_TRANSPORT=tcp-nop: the discovery loop actually binds a TCP
    // listener + mDNS service; for protocol tests we don't need real peers, but
    // we do need the daemon to start without hitting BLE permissions on macOS.
    // TCP is the default and is safe to run headlessly — nothing else in these
    // tests drives real peer connections.
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
    // Drain the server-sent `ready` frame.
    let ready = read_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    ws
}

/// Point `MLINK_ROOMS_FILE` at a per-process temp path so these tests never
/// touch the user's real `~/.mlink/rooms.json`. Idempotent — set once per
/// process.
fn redirect_rooms_file() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "mlink-ws-test-rooms-{}-{}.json",
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
        .expect("timeout reading ws frame")
        .expect("stream ended")
        .expect("ws error");
    let text = match msg {
        Message::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    };
    serde_json::from_str(&text).expect("valid json")
}

#[tokio::test]
async fn hello_echoes_ready_and_acks_when_id_present() {
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"r1", "type":"hello", "payload":{"client_name":"test"}}),
    )
    .await;
    let first = read_json(&mut ws).await;
    assert_eq!(first["type"], "ready");
    assert!(first["payload"]["app_uuid"].as_str().is_some());
    let second = read_json(&mut ws).await;
    assert_eq!(second["type"], "ack");
    assert_eq!(second["id"], "r1");
    assert_eq!(second["payload"]["type"], "hello");
}

#[tokio::test]
async fn ping_without_id_returns_pong() {
    let mut ws = connect().await;
    send_json(&mut ws, json!({"v":1, "type":"ping", "payload":{}})).await;
    let frame = read_json(&mut ws).await;
    assert_eq!(frame["type"], "pong");
    assert!(frame.get("id").is_none() || frame["id"].is_null());
}

#[tokio::test]
async fn ping_with_id_echoes_id_in_pong() {
    let mut ws = connect().await;
    send_json(&mut ws, json!({"v":1, "id":"p-1", "type":"ping", "payload":{}})).await;
    let frame = read_json(&mut ws).await;
    assert_eq!(frame["type"], "pong");
    assert_eq!(frame["id"], "p-1");
}

#[tokio::test]
async fn join_acks_and_returns_room_state() {
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"j-1", "type":"join", "payload":{"room":"123456"}}),
    )
    .await;
    let ack = read_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["id"], "j-1");
    assert_eq!(ack["payload"]["type"], "join");
    let state = read_json(&mut ws).await;
    assert_eq!(state["type"], "room_state");
    assert_eq!(state["payload"]["room"], "123456");
    assert_eq!(state["payload"]["joined"], true);
    assert!(state["payload"]["peers"].is_array());
}

#[tokio::test]
async fn join_rejects_bad_room_code() {
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"j-x", "type":"join", "payload":{"room":"abc"}}),
    )
    .await;
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "bad_room");
    assert_eq!(err["payload"]["id"], "j-x");
}

#[tokio::test]
async fn leave_emits_ack_and_room_state_joined_false() {
    let mut ws = connect().await;
    // First join, then leave.
    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"654321"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;
    let _rs = read_json(&mut ws).await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"l", "type":"leave", "payload":{"room":"654321"}}),
    )
    .await;
    let ack = read_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");
    assert_eq!(ack["payload"]["type"], "leave");
    let state = read_json(&mut ws).await;
    assert_eq!(state["type"], "room_state");
    assert_eq!(state["payload"]["joined"], false);
}

#[tokio::test]
async fn send_before_join_returns_not_joined_error() {
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"s", "type":"send", "payload":{"room":"111111","payload":{"hi":1}}}),
    )
    .await;
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "not_joined");
}

#[tokio::test]
async fn send_to_empty_room_returns_send_failed() {
    // A client can join locally even when no peers have connected — the
    // daemon still accepts and filters at the handshake layer. With no peers
    // connected, `send` has nowhere to deliver and must surface that.
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"999999"}}),
    )
    .await;
    let _ = read_json(&mut ws).await;
    let _ = read_json(&mut ws).await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"s", "type":"send", "payload":{"room":"999999","payload":"hi"}}),
    )
    .await;
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "send_failed");
}

#[tokio::test]
async fn unknown_type_returns_bad_type_error() {
    let mut ws = connect().await;
    send_json(&mut ws, json!({"v":1, "id":"u", "type":"nope", "payload":{}})).await;
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "bad_type");
    assert_eq!(err["id"], "u");
}

#[tokio::test]
async fn bad_json_returns_bad_json_error() {
    let mut ws = connect().await;
    // Not valid JSON at all.
    ws.send(Message::Text("not json".into())).await.unwrap();
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "bad_json");
}

#[tokio::test]
async fn wrong_version_returns_bad_version_error() {
    let mut ws = connect().await;
    send_json(&mut ws, json!({"v":99, "type":"ping", "payload":{}})).await;
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "bad_version");
}

#[tokio::test]
async fn oversize_frame_returns_payload_too_large() {
    let mut ws = connect().await;
    // 2 MB string — well over the 1 MB cap.
    let big: String = "x".repeat(2 * 1024 * 1024);
    let frame = format!(r#"{{"v":1,"type":"ping","payload":{{"x":"{big}"}}}}"#);
    ws.send(Message::Text(frame)).await.unwrap();
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "payload_too_large");
}

#[tokio::test]
async fn multi_client_each_has_own_rooms() {
    // Two clients attached to the same daemon. One joins, the other should
    // still be able to independently send `join`/`leave` and see its own acks.
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
    let (mut a, _) = tokio_tungstenite::connect_async(url.clone()).await.unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let _ = read_json(&mut a).await;
    let _ = read_json(&mut b).await;

    send_json(
        &mut a,
        json!({"v":1, "id":"ja", "type":"join", "payload":{"room":"100000"}}),
    )
    .await;
    let ack_a = read_json(&mut a).await;
    assert_eq!(ack_a["id"], "ja");
    let _ = read_json(&mut a).await;

    // B joins a different room — must see *its own* ack, not A's.
    send_json(
        &mut b,
        json!({"v":1, "id":"jb", "type":"join", "payload":{"room":"200000"}}),
    )
    .await;
    let ack_b = read_json(&mut b).await;
    assert_eq!(ack_b["id"], "jb");
    assert_eq!(ack_b["payload"]["type"], "join");
}

/// Stand up a daemon and hand back both a connected WS and the `DaemonState`
/// so tests can poke the queue / RoomStore directly. Callers are responsible
/// for redirecting `MLINK_ROOMS_FILE` beforehand when they need isolation.
async fn connect_with_state() -> (Ws, DaemonState) {
    redirect_rooms_file();
    let state = build_state().await.expect("build_state");
    let app = router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::task::yield_now().await;

    let url = format!("ws://127.0.0.1:{port}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");
    let ready = read_json(&mut ws).await;
    assert_eq!(ready["type"], "ready");
    (ws, state)
}

#[tokio::test]
async fn disconnect_keeps_daemon_room_joined() {
    // With the subscription-model refactor, a WS disconnect must NOT retire
    // the room from the Node / RoomStore — the daemon owns membership so
    // peer connections stay alive across client restarts.
    let (mut ws, state) = connect_with_state().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"314159"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;
    let _rs = read_json(&mut ws).await;

    // Client vanishes without issuing `leave`.
    drop(ws);
    // Give the server a beat to notice the socket is gone.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let hashes = state.node.room_hashes();
    assert!(
        hashes.contains(&room_hash("314159")),
        "daemon must stay joined after WS disconnect"
    );
    let listed = state.rooms.lock().unwrap().list();
    assert!(
        listed.iter().any(|c| c == "314159"),
        "RoomStore must retain room after WS disconnect"
    );
}

#[tokio::test]
async fn explicit_leave_without_other_subscribers_retires_room() {
    // `leave` is an explicit "stop caring about this room" signal. When the
    // last live subscriber leaves, the daemon must remove the hash from the
    // Node and drop it from the persistent store.
    let (mut ws, state) = connect_with_state().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"272727"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;
    let _rs = read_json(&mut ws).await;

    send_json(
        &mut ws,
        json!({"v":1, "id":"l", "type":"leave", "payload":{"room":"272727"}}),
    )
    .await;
    let _lack = read_json(&mut ws).await;
    let _lrs = read_json(&mut ws).await;

    let hashes = state.node.room_hashes();
    assert!(
        !hashes.contains(&room_hash("272727")),
        "explicit leave must retire the room hash"
    );
    let listed = state.rooms.lock().unwrap().list();
    assert!(!listed.iter().any(|c| c == "272727"));
}

/// Drive `accept_incoming` on the daemon's Node against a mock connection.
/// Returns once the Node has registered the peer — i.e. PeerManager now holds
/// an entry and a `PeerConnected` event has been broadcast. Uses the same
/// in-crate handshake driver pattern as `node.rs` integration tests.
///
/// The peer's advertised `room_hash` is picked from the daemon's current set
/// (or `None` if the daemon hasn't joined any room yet), so the handshake
/// passes the room-intersection check regardless of when the caller joined.
async fn attach_handshaked_peer(state: &DaemonState) -> String {
    use mlink_core::core::connection::perform_handshake;
    use mlink_core::core::peer::generate_app_uuid;
    use mlink_core::protocol::types::{Handshake, PROTOCOL_VERSION};
    use mlink_core::transport::mock::mock_pair;

    let (ca, cb) = mock_pair();
    let peer_app_uuid = generate_app_uuid();
    let room_hash = state.node.room_hashes().into_iter().next();
    let hs = Handshake {
        app_uuid: peer_app_uuid.clone(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash,
    };
    let driver = tokio::spawn(async move {
        let _ = perform_handshake(&cb, &hs).await;
    });
    let id = state
        .node
        .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
        .await
        .expect("accept_incoming");
    driver.await.expect("driver joined");
    assert_eq!(id, peer_app_uuid);
    peer_app_uuid
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_returns_connected_peers_in_room_state() {
    // Once a real peer has handshaked through `accept_incoming`, a fresh
    // `join` must surface that peer in the returned `room_state.peers` list.
    // Regression: previously this was always `[]` because session.rs never
    // consulted `node.peers()` on the event-driven path.
    //
    // multi_thread runtime is required because accept_incoming drives its
    // handshake via a spawned driver; running everything on one thread plus
    // axum::serve + session loops tends to starve one of them under test load.
    let (mut ws, state) = connect_with_state().await;
    let peer_app_uuid = attach_handshaked_peer(&state).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"515151"}}),
    )
    .await;
    let ack = read_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");

    // PeerConnected may race ahead of our join — read frames until we find a
    // room_state for this room, then assert its peer list.
    let mut peers_found: Option<usize> = None;
    for _ in 0..6 {
        let frame = read_json(&mut ws).await;
        if frame["type"] != "room_state" {
            continue;
        }
        if frame["payload"]["room"] == "515151" {
            peers_found = Some(frame["payload"]["peers"].as_array().unwrap().len());
            break;
        }
    }
    assert_eq!(
        peers_found.unwrap_or(0),
        1,
        "expected the handshaked peer ({}) to appear in peers list",
        peer_app_uuid
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_connected_event_pushes_room_state_with_peers() {
    // PeerConnected must push a `room_state` whose `peers` field is the
    // current `Node::peers()` — not an empty placeholder. This is the exact
    // regression that left the WS `peers` pane blank even after handshake OK.
    let (mut ws, state) = connect_with_state().await;

    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"616161"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;
    let initial_rs = read_json(&mut ws).await;
    assert_eq!(initial_rs["type"], "room_state");
    assert_eq!(
        initial_rs["payload"]["peers"].as_array().unwrap().len(),
        0,
        "no peers attached yet — empty list expected"
    );

    // Run a real handshake so `Node::peers()` actually contains an entry.
    attach_handshaked_peer(&state).await;

    // Session loop should observe PeerConnected and fan a fresh room_state.
    let pushed = read_json(&mut ws).await;
    assert_eq!(pushed["type"], "room_state");
    assert_eq!(pushed["payload"]["room"], "616161");
    assert_eq!(pushed["payload"]["joined"], true);
    let peers = pushed["payload"]["peers"].as_array().expect("peers array");
    assert_eq!(
        peers.len(),
        1,
        "PeerConnected must push the real peer list, not []"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_disconnected_event_pushes_updated_room_state() {
    // Symmetric: after a peer drops, the next fanned room_state must reflect
    // the shrunken peers list.
    let (mut ws, state) = connect_with_state().await;
    let peer_id = attach_handshaked_peer(&state).await;

    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"717171"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;

    // Drain any room_state frames in flight until we've seen the one
    // reflecting the attached peer (peers.len == 1), then issue disconnect.
    let mut saw_one = false;
    for _ in 0..4 {
        let frame = read_json(&mut ws).await;
        if frame["type"] == "room_state" && frame["payload"]["peers"].as_array().unwrap().len() == 1 {
            saw_one = true;
            break;
        }
    }
    assert!(saw_one, "expected a room_state with one peer before disconnect");

    state.node.disconnect_peer(&peer_id).await.expect("disconnect");

    // Expect a room_state with peers = [] eventually.
    let mut saw_empty = false;
    for _ in 0..4 {
        let frame = read_json(&mut ws).await;
        assert_eq!(frame["type"], "room_state", "unexpected frame while waiting for empty peers");
        if frame["payload"]["peers"].as_array().unwrap().is_empty() {
            saw_empty = true;
            break;
        }
    }
    assert!(saw_empty, "PeerDisconnected must push an updated room_state with empty peers");
}

#[tokio::test]
async fn join_drains_backlog_from_queue() {
    // A message pushed into the queue before any client has subscribed must
    // be delivered to the first client that joins the room. This is the
    // whole point of the daemon-level backlog: offline clients catch up on
    // reconnect.
    let (mut ws, state) = connect_with_state().await;
    state.queue.lock().unwrap().push(MessageEntry {
        room: "424242".into(),
        from: "peer-x".into(),
        payload: json!({"hi": "there"}),
        ts: 1234,
    });

    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"424242"}}),
    )
    .await;
    let ack = read_json(&mut ws).await;
    assert_eq!(ack["type"], "ack");
    let rs = read_json(&mut ws).await;
    assert_eq!(rs["type"], "room_state");
    let backlog = read_json(&mut ws).await;
    assert_eq!(backlog["type"], "message");
    assert_eq!(backlog["payload"]["room"], "424242");
    assert_eq!(backlog["payload"]["from"], "peer-x");
    assert_eq!(backlog["payload"]["payload"]["hi"], "there");

    // Second `join` by the same client must not re-deliver the backlog —
    // drain left the bucket empty.
    assert_eq!(state.queue.lock().unwrap().len("424242"), 0);
}
