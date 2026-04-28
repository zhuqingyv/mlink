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
    // Force TCP-only discovery: daemon default is now `Dual`, which would
    // bring up the BLE peripheral and trigger a macOS permission prompt on
    // CI. Protocol tests don't need real peers — TCP alone is enough to
    // exercise the WS surface.
    std::env::set_var("MLINK_DAEMON_TRANSPORT", "tcp");
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

/// Read frames off `ws` until one of type `want` arrives. Skips transport-
/// debug frames (`link_added` / `link_removed` / `primary_changed` etc.) that
/// may race ahead of the expected reply — those are validated by their own
/// dedicated tests.
async fn read_json_of_type(ws: &mut Ws, want: &str) -> Value {
    for _ in 0..16 {
        let frame = read_json(ws).await;
        if frame["type"] == want {
            return frame;
        }
    }
    panic!("did not see a frame of type {want} within 16 frames");
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
    attach_handshaked_peer_with_conn(state).await.0
}

/// Like `attach_handshaked_peer`, but also hands back the peer-side mock
/// connection so the caller can read the frames the daemon writes to it.
/// Used by the `send` broadcast/unicast tests to assert actual delivery.
async fn attach_handshaked_peer_with_conn(
    state: &DaemonState,
) -> (String, mlink_core::transport::mock::MockConnection) {
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
        session_id: None,
        session_last_seq: 0,
    };
    // Drive the peer side of the handshake in-place: write our Handshake
    // frame, read the daemon's reply, then hand `cb` back to the caller so
    // later `Message` frames can still be read off it. We can't use
    // `perform_handshake` here because it takes `&Connection` by reference
    // and a spawned task would need to own `cb`.
    use mlink_core::protocol::codec;
    use mlink_core::protocol::frame::{decode_frame, encode_frame as encode_wire};
    use mlink_core::protocol::types::{encode_flags, Frame, MessageType, MAGIC};
    use mlink_core::transport::Connection as _;
    let payload = codec::encode(&hs).expect("encode hs");
    let hs_frame = Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, MessageType::Handshake),
        seq: 0,
        length: payload.len() as u16,
        payload,
    };
    let hs_bytes = encode_wire(&hs_frame);
    let driver = {
        let app_uuid = peer_app_uuid.clone();
        tokio::spawn(async move {
            cb.write(&hs_bytes).await.expect("cb write hs");
            let reply = cb.read().await.expect("cb read daemon hs");
            let decoded = decode_frame(&reply).expect("decode daemon hs");
            let (_, _, mt) = mlink_core::protocol::types::decode_flags(decoded.flags);
            assert_eq!(mt, MessageType::Handshake, "{app_uuid}: expected Handshake reply");
            cb
        })
    };
    let id = state
        .node
        .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
        .await
        .expect("accept_incoming");
    let cb = driver.await.expect("driver joined");
    assert_eq!(id, peer_app_uuid);
    (peer_app_uuid, cb)
}

/// Read a `Message` frame from a peer-side mock connection and return the
/// decoded payload bytes. Asserts the frame is actually a `Message` and not
/// e.g. a Heartbeat. Payload is expected to be plaintext because the
/// daemon's `accept_incoming` handshake advertises encrypt=false.
async fn read_message_payload(
    conn: &mlink_core::transport::mock::MockConnection,
) -> Vec<u8> {
    use mlink_core::protocol::frame::decode_frame;
    use mlink_core::protocol::types::{decode_flags, MessageType};
    use mlink_core::transport::Connection as _;
    let bytes = tokio::time::timeout(Duration::from_secs(3), conn.read())
        .await
        .expect("timeout reading peer frame")
        .expect("peer read");
    let frame = decode_frame(&bytes).expect("decode frame");
    let (compressed, encrypted, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message, "expected Message, got {mt:?}");
    assert!(!compressed, "small payloads should not be compressed");
    assert!(!encrypted, "handshake was encrypt=false, frames must be plaintext");
    frame.payload
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
    // `link_added` (and any other transport-debug frame) may race ahead of the
    // join ack because the peer handshake above emits it. Skip past them.
    let ack = read_json_of_type(&mut ws, "ack").await;
    assert_eq!(ack["type"], "ack");

    // PeerConnected may race ahead of our join — read frames until we find a
    // room_state for this room, then assert its peer list.
    let mut peers_found: Option<usize> = None;
    for _ in 0..8 {
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
    // Transport-debug frames (link_added etc.) may race ahead; skip them.
    let pushed = read_json_of_type(&mut ws, "room_state").await;
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

    // Expect a room_state with peers = [] eventually. Transport-debug frames
    // (`link_removed`) may race ahead of the room_state — skip them.
    let mut saw_empty = false;
    for _ in 0..8 {
        let frame = read_json(&mut ws).await;
        if frame["type"] != "room_state" {
            continue;
        }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_broadcasts_to_all_peers_and_acks_sender() {
    // Happy path for the `send` handler without a `to` field: with two
    // handshaked peers attached, the daemon must write a Message frame to
    // BOTH peers and return an ack to the WS client that issued `send`.
    // Regression guard for the fan-out loop in handle_send.
    let (mut ws, state) = connect_with_state().await;

    // Join first so both the sub check passes and the peers' handshakes
    // carry the right room_hash.
    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"818181"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;
    let _rs = read_json(&mut ws).await;

    let (peer_a_id, peer_a_conn) = attach_handshaked_peer_with_conn(&state).await;
    let (peer_b_id, peer_b_conn) = attach_handshaked_peer_with_conn(&state).await;

    // Drain any PeerConnected-driven room_state pushes that arrive on the
    // WS before our `send` reply lands.
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_json(
        &mut ws,
        json!({
            "v":1, "id":"s-bcast", "type":"send",
            "payload":{"room":"818181", "payload":{"hello":"all"}}
        }),
    )
    .await;

    // Both peer-side mocks must see the Message frame with our JSON payload.
    for conn in [&peer_a_conn, &peer_b_conn] {
        let payload_bytes = read_message_payload(conn).await;
        let v: Value = serde_json::from_slice(&payload_bytes).expect("json payload");
        assert_eq!(v, json!({"hello":"all"}));
    }

    // The sender should see an ack for the send request. Skip any
    // room_state frames that may have been queued before the ack.
    let mut saw_ack = false;
    for _ in 0..6 {
        let frame = read_json(&mut ws).await;
        if frame["type"] == "ack" && frame["id"] == "s-bcast" {
            assert_eq!(frame["payload"]["type"], "send");
            saw_ack = true;
            break;
        }
    }
    assert!(saw_ack, "expected ack for broadcast send");
    // Sanity: both peers still registered (no accidental disconnect).
    let ids: Vec<String> = state.node.peers().await.into_iter().map(|p| p.id).collect();
    assert!(ids.contains(&peer_a_id) && ids.contains(&peer_b_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_to_specific_peer_unicasts_only_to_target() {
    // `send` with a `to` field must deliver to exactly that peer — the
    // other attached peer must not see any frame. Guards against the
    // trivial bug of falling back to broadcast when `to` is set.
    let (mut ws, state) = connect_with_state().await;

    send_json(
        &mut ws,
        json!({"v":1, "id":"j", "type":"join", "payload":{"room":"919191"}}),
    )
    .await;
    let _ack = read_json(&mut ws).await;
    let _rs = read_json(&mut ws).await;

    let (peer_a_id, peer_a_conn) = attach_handshaked_peer_with_conn(&state).await;
    let (_peer_b_id, peer_b_conn) = attach_handshaked_peer_with_conn(&state).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    send_json(
        &mut ws,
        json!({
            "v":1, "id":"s-unicast", "type":"send",
            "payload":{"room":"919191", "to": peer_a_id, "payload":{"only":"a"}}
        }),
    )
    .await;

    // Target peer A must receive exactly the payload.
    let payload_bytes = read_message_payload(&peer_a_conn).await;
    let v: Value = serde_json::from_slice(&payload_bytes).expect("json payload");
    assert_eq!(v, json!({"only":"a"}));

    // Non-target peer B must see nothing within a sensible window.
    use mlink_core::transport::Connection as _;
    let silent = tokio::time::timeout(Duration::from_millis(150), peer_b_conn.read()).await;
    assert!(
        silent.is_err(),
        "peer B must not receive a unicast addressed to peer A, got: {silent:?}"
    );
}

#[tokio::test]
async fn hello_with_non_object_payload_returns_bad_payload() {
    // `hello.payload` must be an object (HelloPayload), so a JSON string
    // fails deserialization and the handler must emit a `bad_payload`
    // error instead of panicking or silently accepting. Covers the
    // handle_hello error path that was previously untested.
    let mut ws = connect().await;
    send_json(
        &mut ws,
        json!({"v":1, "id":"h", "type":"hello", "payload":"not-an-object"}),
    )
    .await;
    let err = read_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["payload"]["code"], "bad_payload");
    assert_eq!(err["payload"]["id"], "h");
}
