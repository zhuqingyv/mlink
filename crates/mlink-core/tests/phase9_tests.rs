// Phase 9 tests — end-to-end integration via mock_pair.
//
// These tests exercise the full stack without a real BLE radio:
//   * mock_pair() gives two Connection endpoints wired through tokio channels.
//   * Frames are encoded/decoded through the real protocol codec.
//   * perform_handshake runs across both ends concurrently.
//   * RPC request/response round-trips using the real PendingRequests/RpcRegistry.
//   * RoomManager integration mirrors how Node would track joined rooms.
//   * Peripheral module is smoke-checked for compile-time presence.

use std::sync::Arc;

use mlink_core::core::connection::perform_handshake;
use mlink_core::core::node::{Node, NodeConfig, NodeState};
use mlink_core::core::room::{room_hash, RoomManager};
use mlink_core::protocol::codec;
use mlink_core::protocol::errors::{MlinkError, Result};
use mlink_core::protocol::frame::{decode_frame, encode_frame};
use mlink_core::protocol::types::{
    decode_flags, encode_flags, Frame, Handshake, MessageType, MAGIC, PROTOCOL_VERSION,
};
use mlink_core::transport::mock::mock_pair;
use mlink_core::transport::Connection;
use mlink_core::api::rpc::{
    decode_request, decode_response, PendingRequests, RpcRegistry, RpcResponse, RPC_STATUS_OK,
};

// ---------------------------------------------------------------------------
// helpers: wrap a MockConnection pair into a message-level send/recv.
// ---------------------------------------------------------------------------

fn make_frame(msg_type: MessageType, seq: u16, payload: Vec<u8>) -> Vec<u8> {
    let length = payload.len() as u16;
    let frame = Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, msg_type),
        seq,
        length,
        payload,
    };
    encode_frame(&frame)
}

async fn send_msg(
    conn: &dyn Connection,
    msg_type: MessageType,
    seq: u16,
    payload: &[u8],
) -> Result<()> {
    conn.write(&make_frame(msg_type, seq, payload.to_vec())).await
}

async fn recv_msg(conn: &dyn Connection) -> Result<(Frame, Vec<u8>)> {
    let bytes = conn.read().await?;
    let frame = decode_frame(&bytes)?;
    let payload = frame.payload.clone();
    Ok((frame, payload))
}

// ---------------------------------------------------------------------------
// 1. end-to-end message over mock transport.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_message_roundtrip() {
    // Two endpoints wired via mock_pair. A sends a Message frame, B decodes it,
    // verifies magic/version/type/payload match.
    let (a, b) = mock_pair();

    let payload = b"hello from A".to_vec();
    send_msg(&a, MessageType::Message, 1, &payload).await.expect("send");

    let (frame, got) = recv_msg(&b).await.expect("recv");
    assert_eq!(frame.magic, MAGIC);
    assert_eq!(frame.version, PROTOCOL_VERSION);
    assert_eq!(frame.seq, 1);
    let (_compressed, _encrypted, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message);
    assert_eq!(got, payload);
}

#[tokio::test]
async fn test_e2e_bidirectional() {
    // A→B then B→A, independent sequence numbers.
    let (a, b) = mock_pair();

    send_msg(&a, MessageType::Message, 1, b"A->B").await.expect("a->b");
    let (_, got) = recv_msg(&b).await.expect("b recv");
    assert_eq!(got, b"A->B");

    send_msg(&b, MessageType::Message, 7, b"B->A").await.expect("b->a");
    let (frame, got) = recv_msg(&a).await.expect("a recv");
    assert_eq!(got, b"B->A");
    assert_eq!(frame.seq, 7);
}

#[tokio::test]
async fn test_e2e_large_payload_roundtrip() {
    // Multi-KB payload still survives end-to-end through the mock pipe.
    let (a, b) = mock_pair();

    let payload: Vec<u8> = (0u16..4096).map(|i| (i & 0xff) as u8).collect();
    send_msg(&a, MessageType::Message, 0, &payload).await.expect("send");

    let (frame, got) = recv_msg(&b).await.expect("recv");
    assert_eq!(frame.length as usize, payload.len());
    assert_eq!(got, payload);
}

// ---------------------------------------------------------------------------
// 2. room-code integration.
// ---------------------------------------------------------------------------

#[test]
fn test_room_join_creates_state() {
    // RoomManager.join records the room; is_joined + list reflect it.
    let mut mgr = RoomManager::new();
    assert!(!mgr.is_joined("424242"));

    let state = mgr.join("424242");
    assert_eq!(state.code, "424242");
    assert_eq!(state.hash, room_hash("424242"));
    assert!(mgr.is_joined("424242"));
    assert_eq!(mgr.list().len(), 1);
}

#[tokio::test]
async fn test_room_isolation() {
    // Two independent RoomManagers with different codes produce different
    // hashes; messages on a mock_pair tagged for one room must not look like
    // they belong to the other (mlink's actual filter happens at the scanner
    // level using room_hashes, but we verify the hash property here).
    let mut room_a = RoomManager::new();
    let mut room_b = RoomManager::new();
    room_a.join("111111");
    room_b.join("222222");

    let ha = room_a.active_hashes();
    let hb = room_b.active_hashes();
    assert_eq!(ha.len(), 1);
    assert_eq!(hb.len(), 1);
    assert_ne!(ha[0], hb[0], "distinct codes must not share a hash");

    // And on an actual mock_pair channel, a frame written from one "room's"
    // endpoint arrives at the other endpoint byte-identical — the isolation
    // lives at the *scanner filter* layer, not in the bytes. Verify that: the
    // frame itself carries no room_hash, so payload pass-through is total.
    let (a, b) = mock_pair();
    send_msg(&a, MessageType::Message, 0, b"cross-room").await.expect("send");
    let (_, got) = recv_msg(&b).await.expect("recv");
    assert_eq!(got, b"cross-room");
}

#[test]
fn test_room_distinct_managers_do_not_share_state() {
    // Joining room X on manager-1 must not make manager-2 think it's joined.
    let mut a = RoomManager::new();
    let b = RoomManager::new();
    a.join("123456");
    assert!(a.is_joined("123456"));
    assert!(!b.is_joined("123456"));
}

// ---------------------------------------------------------------------------
// 3. handshake end-to-end across mock_pair.
// ---------------------------------------------------------------------------

fn sample_handshake(uuid: &str, mtu: u16) -> Handshake {
    Handshake {
        app_uuid: uuid.into(),
        version: PROTOCOL_VERSION,
        mtu,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: None,
    }
}

#[tokio::test]
async fn test_e2e_handshake() {
    // Two endpoints each drive perform_handshake concurrently; each must see
    // the peer's announced UUID come back through the real wire format.
    let (a, b) = mock_pair();

    let hs_a = sample_handshake("uuid-alpha", 512);
    let hs_b = sample_handshake("uuid-bravo", 256);

    let hs_a_clone = hs_a.clone();
    let hs_b_clone = hs_b.clone();

    let task_a = tokio::spawn(async move {
        let got = perform_handshake(&a, &hs_a_clone).await.expect("a hs");
        (got, a)
    });
    let task_b = tokio::spawn(async move {
        let got = perform_handshake(&b, &hs_b_clone).await.expect("b hs");
        (got, b)
    });

    let (got_on_a, _a_conn) = task_a.await.expect("task a");
    let (got_on_b, _b_conn) = task_b.await.expect("task b");

    // Each side receives the *other* side's handshake.
    assert_eq!(got_on_a, hs_b);
    assert_eq!(got_on_b, hs_a);
}

#[tokio::test]
async fn test_e2e_handshake_rejects_wrong_message_type() {
    // If B sends a Message instead of a Handshake, A's perform_handshake must
    // fail with CodecError. Drive A's perform_handshake while B hand-rolls a
    // non-handshake reply.
    let (a, b) = mock_pair();

    let hs_a = sample_handshake("uuid-a", 512);

    // Send a bogus Message frame from B before A calls perform_handshake, so
    // when A writes its handshake and then reads, it pulls the Message.
    let bogus = make_frame(MessageType::Message, 0, b"not a handshake".to_vec());
    b.write(&bogus).await.expect("b write bogus");

    // B also needs to drain the incoming handshake or A will block on write.
    // Since mock_pair has bounded buffer (64), A's write will usually fit; we
    // read-drain on B in a task to be safe.
    let drain = tokio::spawn(async move {
        let _ = b.read().await;
    });

    let err = perform_handshake(&a, &hs_a).await.unwrap_err();
    assert!(matches!(err, MlinkError::CodecError(_)));

    let _ = drain.await;
}

// ---------------------------------------------------------------------------
// 4. RPC end-to-end: A registers handler, B sends request, A responds.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_rpc() {
    // A runs an RpcRegistry with an "echo" handler. B builds a request, ships
    // it as a Request frame over mock_pair. A receives, dispatches, writes
    // back a Response frame. B decodes, matches request_id, and the pending
    // waiter resolves with the handler's output.
    let (a, b) = mock_pair();
    let a: Arc<dyn Connection> = Arc::new(a);
    let b: Arc<dyn Connection> = Arc::new(b);

    // A-side: registry with echo handler.
    let reg = Arc::new(RpcRegistry::new());
    reg.register(
        "echo".into(),
        Box::new(|data: Vec<u8>| Box::pin(async move { Ok(data) })),
    )
    .await;

    // B-side: pending requests.
    let pending = Arc::new(PendingRequests::new());
    let request_id = pending.next_id().await;
    let waiter = pending.register(request_id).await;

    // B sends the request.
    let req_bytes = {
        use mlink_core::api::rpc::RpcRequest;
        rmp_serde::to_vec(&RpcRequest {
            request_id,
            method: "echo".into(),
            data: b"ping".to_vec(),
        })
        .unwrap()
    };
    send_msg(&*b, MessageType::Request, 1, &req_bytes)
        .await
        .expect("b send request");

    // A-side task: receive request, dispatch, send response.
    let a_clone = Arc::clone(&a);
    let reg_clone = Arc::clone(&reg);
    let a_task = tokio::spawn(async move {
        let (frame, payload) = recv_msg(&*a_clone).await.expect("a recv");
        let (_, _, mt) = decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Request);

        let req = decode_request(&payload).expect("decode req");
        assert_eq!(req.method, "echo");

        let out = reg_clone
            .handle_request(&req.method, req.data)
            .await
            .expect("handle");
        let resp = RpcResponse {
            request_id: req.request_id,
            status: RPC_STATUS_OK,
            data: out,
        };
        let resp_bytes = rmp_serde::to_vec(&resp).expect("encode resp");
        send_msg(&*a_clone, MessageType::Response, 2, &resp_bytes)
            .await
            .expect("a send resp");
    });

    // B-side: read response frame, decode, deliver to pending.
    let (frame, payload) = recv_msg(&*b).await.expect("b recv resp");
    let (_, _, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Response);

    let resp = decode_response(&payload).expect("decode resp");
    assert_eq!(resp.request_id, request_id);
    assert_eq!(resp.status, RPC_STATUS_OK);
    assert!(pending.complete(resp).await);

    let got = waiter.await.expect("waiter resolved");
    assert_eq!(got.data, b"ping".to_vec());

    a_task.await.expect("a task");
}

#[tokio::test]
async fn test_e2e_rpc_unknown_method_errors() {
    // Same wiring as above but the method isn't registered → dispatcher must
    // produce an RPC_STATUS_ERROR response; B must surface that as an error.
    use mlink_core::api::rpc::{RpcRequest, RPC_STATUS_ERROR};

    let (a, b) = mock_pair();

    let reg = RpcRegistry::new(); // no handlers registered

    // B sends request for "missing".
    let req = RpcRequest {
        request_id: 42,
        method: "missing".into(),
        data: vec![],
    };
    let req_bytes = rmp_serde::to_vec(&req).unwrap();
    send_msg(&b, MessageType::Request, 0, &req_bytes).await.expect("b send");

    // A receives, handle_request returns UnknownMethod; encode error response.
    let (_, payload) = recv_msg(&a).await.expect("a recv");
    let req_back = decode_request(&payload).unwrap();
    let err = reg
        .handle_request(&req_back.method, req_back.data)
        .await
        .unwrap_err();
    assert!(matches!(err, MlinkError::UnknownMethod(_)));

    let resp = RpcResponse {
        request_id: req_back.request_id,
        status: RPC_STATUS_ERROR,
        data: b"missing".to_vec(),
    };
    let resp_bytes = rmp_serde::to_vec(&resp).unwrap();
    send_msg(&a, MessageType::Response, 0, &resp_bytes).await.expect("a send");

    // B reads, checks status.
    let (_, resp_payload) = recv_msg(&b).await.expect("b recv");
    let resp_back = decode_response(&resp_payload).unwrap();
    assert_eq!(resp_back.status, RPC_STATUS_ERROR);
    assert_eq!(resp_back.request_id, 42);
}

// ---------------------------------------------------------------------------
// 5. peripheral module smoke-check.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
#[test]
fn test_peripheral_module_exists() {
    // On macOS the peripheral module must expose MacPeripheral +
    // MacPeripheralConnection + every PeripheralEvent variant the delegate
    // emits. We don't start a real advertisement (that needs a radio + user
    // permission), we only check type + method presence at compile time.
    //
    // If the delegate work in task #1 ever drops a variant or renames a
    // pub API, this test stops compiling.
    use mlink_core::transport::peripheral::{
        MacPeripheral, MacPeripheralConnection, PeripheralEvent,
    };

    // Force monomorphization so the module's types really exist.
    fn _assert_types(
        _p: &MacPeripheral,
        _c: &MacPeripheralConnection,
        _e: &PeripheralEvent,
    ) {
    }

    // Every PeripheralEvent variant the delegate emits must be constructible.
    let _e = PeripheralEvent::Received(vec![1, 2, 3]);
    let _e = PeripheralEvent::CentralSubscribed { central_id: "cx".into() };
    let _e = PeripheralEvent::ServiceAdded(Ok(()));
    let _e = PeripheralEvent::ServiceAdded(Err("nope".into()));
    let _e = PeripheralEvent::AdvertisingStarted(Ok(()));
    let _e = PeripheralEvent::AdvertisingStarted(Err("nope".into()));
    // StateChanged takes a CBManagerState; only check the variant exists by
    // pattern-matching (we can't cheaply construct a CBManagerState here).
    fn _match_state(e: &PeripheralEvent) -> bool {
        matches!(e, PeripheralEvent::StateChanged(_))
    }

    // MacPeripheral::notify_subscribed is the write-path API task #1 added.
    // Reference it as a function pointer to force signature verification.
    let _notify: fn(&MacPeripheral, &[u8]) -> bool = MacPeripheral::notify_subscribed;

    // Accessors we rely on.
    let _ln: fn(&MacPeripheral) -> &str = MacPeripheral::local_name;
    let _rh: fn(&MacPeripheral) -> Option<&[u8; 8]> = MacPeripheral::room_hash;
    let _sa: fn(&MacPeripheral) = MacPeripheral::stop_advertising;
}

#[cfg(not(target_os = "macos"))]
#[test]
fn test_peripheral_module_exists() {
    // On non-macOS the peripheral module is cfg'd out; this test is a no-op
    // placeholder so the test count is stable across platforms.
}

// ---------------------------------------------------------------------------
// 6. a tiny "full" chain: handshake then bidirectional message exchange on
// the same mock_pair, proving nothing in perform_handshake leaves the channel
// in a bad state.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_e2e_handshake_then_messages() {
    let (a, b) = mock_pair();

    let hs_a = sample_handshake("uuid-alpha", 512);
    let hs_b = sample_handshake("uuid-bravo", 512);

    // Run handshakes concurrently using join!
    let hs_a_c = hs_a.clone();
    let hs_b_c = hs_b.clone();
    let ((got_a, a), (got_b, b)) = tokio::join!(
        async move {
            let got = perform_handshake(&a, &hs_a_c).await.expect("a hs");
            (got, a)
        },
        async move {
            let got = perform_handshake(&b, &hs_b_c).await.expect("b hs");
            (got, b)
        },
    );
    assert_eq!(got_a, hs_b);
    assert_eq!(got_b, hs_a);

    // Now swap to plain messages over the same channel.
    send_msg(&a, MessageType::Message, 1, b"post-handshake").await.expect("a send");
    let (_, got) = recv_msg(&b).await.expect("b recv");
    assert_eq!(got, b"post-handshake");
}

// ---------------------------------------------------------------------------
// 7. Handshake encode round-trip with codec — belt-and-suspenders on the
// payload-level contract that RPC relies on.
// ---------------------------------------------------------------------------

#[test]
fn test_handshake_codec_roundtrip() {
    let hs = sample_handshake("uuid-codec", 1024);
    let bytes = codec::encode(&hs).expect("encode");
    let back: Handshake = codec::decode(&bytes).expect("decode");
    assert_eq!(back, hs);
}

// ---------------------------------------------------------------------------
// 8. Node-level end-to-end via Node::attach_connection (dev-delegate's hook).
//
// Uses the real Node public API: send_raw / recv_raw / set_peer_aes_key /
// disconnect_peer / connection_count. mock_pair gives us two Connection
// endpoints; we attach one to a Node and drive the other directly.
// ---------------------------------------------------------------------------

// Node::new reads/writes an app_uuid under $HOME. std::env is process-global,
// so parallel tests racing to stage HOME would clobber each other. We gate
// the HOME swap (and Node::new, which reads HOME) with a process-wide sync
// Mutex — but we MUST release it before awaiting anything async the test
// does afterwards, or other tests' tokio tasks on the same runtime thread
// deadlock. The lock lives only for the duration of make_node's body.
//
// Because each Node caches its app_uuid after construction, the second Node
// (built under a different HOME) gets a distinct uuid — we drop the HOME
// guard immediately after Node::new so later make_node calls in the same
// test are race-free with other tests too.
static NODE_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

async fn make_node(encrypt: bool) -> (Node, tempfile::TempDir) {
    // Acquire lock, swap HOME, build node, restore HOME, drop lock — all in
    // a tight synchronous window. Node::new *is* async, but it only touches
    // std::env::var and writes a uuid file; no cross-task yield that needs
    // cooperation from other tests. We spawn_blocking would be nuclear; a
    // single synchronous HOME-scope inside this fn is enough.
    let trust_tmp = tempfile::tempdir().expect("trust tmp");
    let trust_path = trust_tmp.path().join("trust.json");
    let home_tmp = tempfile::tempdir().expect("home tmp");
    let cfg = NodeConfig {
        name: "p9-node".into(),
        encrypt,
        trust_store_path: Some(trust_path),
    };

    let node = {
        let _g = NODE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", home_tmp.path());
        let node = Node::new(cfg).await.expect("new node");
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        node
        // _g dropped here, releasing the lock.
    };

    // Keep home_tmp alive as long as the Node is alive in case anything
    // lazily re-reads $HOME (currently nothing does — uuid is cached).
    // Dropping home_tmp early is fine since HOME has already been restored.
    drop(home_tmp);
    (node, trust_tmp)
}

#[tokio::test]
async fn test_node_attach_and_send_raw() {
    // Attach one mock endpoint to a Node, drive the other endpoint directly,
    // observe that send_raw produces a wire-format frame the peer can decode.
    let (node, _tmp) = make_node(false).await;
    let (local, remote) = mock_pair();
    node.attach_connection("peer-x".into(), Box::new(local)).await;

    assert_eq!(node.connection_count().await, 1);
    assert_eq!(node.peer_state("peer-x").await, Some(NodeState::Connected));
    assert_eq!(node.state(), NodeState::Connected);

    node.send_raw("peer-x", MessageType::Message, b"from-node")
        .await
        .expect("send");

    let bytes = remote.read().await.expect("read");
    let frame = decode_frame(&bytes).expect("decode");
    let (_compressed, _encrypted, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message);
    assert_eq!(frame.payload, b"from-node");
}

#[tokio::test]
async fn test_node_recv_raw_over_mock() {
    // Inverse direction: remote writes a valid frame, Node::recv_raw yields it.
    let (node, _tmp) = make_node(false).await;
    let (local, remote) = mock_pair();
    node.attach_connection("peer-y".into(), Box::new(local)).await;

    remote
        .write(&make_frame(MessageType::Message, 9, b"to-node".to_vec()))
        .await
        .expect("remote write");

    let (frame, payload) = node.recv_raw("peer-y").await.expect("recv_raw");
    assert_eq!(frame.seq, 9);
    let (_c, _e, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message);
    assert_eq!(payload, b"to-node");
}

#[tokio::test]
async fn test_node_bidirectional_attached() {
    // Two separate Nodes, each with one endpoint of the same mock_pair.
    // A.send_raw → B.recv_raw and back. Because make_node mutates HOME we
    // build both nodes back-to-back while holding the HOME lock — after
    // Node::new returns, HOME can change and the node keeps working (uuid
    // was already loaded into the struct).
    let (node_a, _ta) = make_node(false).await;
    let (node_b, _tb) = make_node(false).await;

    let (ca, cb) = mock_pair();
    node_a.attach_connection("peer-b-from-a".into(), Box::new(ca)).await;
    node_b.attach_connection("peer-a-from-b".into(), Box::new(cb)).await;

    // A → B
    node_a.send_raw("peer-b-from-a", MessageType::Message, b"A->B")
        .await.expect("a send");
    let (_f, got) = node_b.recv_raw("peer-a-from-b").await.expect("b recv");
    assert_eq!(got, b"A->B");

    // B → A
    node_b.send_raw("peer-a-from-b", MessageType::Message, b"B->A")
        .await.expect("b send");
    let (_f, got) = node_a.recv_raw("peer-b-from-a").await.expect("a recv");
    assert_eq!(got, b"B->A");
}

#[tokio::test]
async fn test_node_isolated_pairs() {
    // Two independent mock_pair channels wired to two separate Node pairs
    // (nodes X↔Y on pair 1, nodes U↔V on pair 2). A message sent on pair 1
    // must not leak into pair 2 — since the channels are physically disjoint
    // this would only fail if a Node globally multicast send_raw (it doesn't).
    let (node_x, _tx) = make_node(false).await;
    let (node_y, _ty) = make_node(false).await;
    let (node_u, _tu) = make_node(false).await;
    let (node_v, _tv) = make_node(false).await;

    let (cx, cy) = mock_pair();
    let (cu, cv) = mock_pair();
    node_x.attach_connection("y".into(), Box::new(cx)).await;
    node_y.attach_connection("x".into(), Box::new(cy)).await;
    node_u.attach_connection("v".into(), Box::new(cu)).await;
    node_v.attach_connection("u".into(), Box::new(cv)).await;

    node_x.send_raw("y", MessageType::Message, b"xy-only").await.expect("send xy");

    // Y must see it.
    let (_f, got) = node_y.recv_raw("x").await.expect("y recv");
    assert_eq!(got, b"xy-only");

    // V must NOT see it — recv would block forever, so poll with a short
    // timeout and expect it to elapse.
    let poll = tokio::time::timeout(
        std::time::Duration::from_millis(50),
        node_v.recv_raw("u"),
    )
    .await;
    assert!(poll.is_err(), "v must not receive an xy-pair message");
}

#[tokio::test]
async fn test_node_send_raw_missing_peer() {
    // send_raw against a peer that was never attached must return PeerGone.
    let (node, _t) = make_node(false).await;
    let err = node
        .send_raw("never-attached", MessageType::Message, b"x")
        .await
        .unwrap_err();
    assert!(matches!(err, MlinkError::PeerGone { .. }));
}

#[tokio::test]
async fn test_node_disconnect_removes_connection() {
    let (node, _t) = make_node(false).await;
    let (local, _remote) = mock_pair();
    node.attach_connection("pz".into(), Box::new(local)).await;
    assert_eq!(node.connection_count().await, 1);

    node.disconnect_peer("pz").await.expect("disconnect");
    assert_eq!(node.connection_count().await, 0);
}

#[tokio::test]
async fn test_node_encrypted_send_recv() {
    // Encryption path: matching AES key on both sides → Node.send_raw produces
    // an encrypted frame that the receiving Node decrypts transparently.
    let (node_a, _ta) = make_node(true).await;
    let (node_b, _tb) = make_node(true).await;

    let (ca, cb) = mock_pair();
    node_a.attach_connection("b".into(), Box::new(ca)).await;
    node_b.attach_connection("a".into(), Box::new(cb)).await;

    // Shared 32-byte key (derive_aes_key's output length); any 32-byte buffer
    // works for round-trip since both sides agree.
    let key: Vec<u8> = (0u8..32).collect();
    node_a.set_peer_aes_key("b", key.clone()).await;
    node_b.set_peer_aes_key("a", key).await;

    node_a
        .send_raw("b", MessageType::Message, b"secret-hello")
        .await
        .expect("a send encrypted");
    let (_f, got) = node_b.recv_raw("a").await.expect("b recv encrypted");
    assert_eq!(got, b"secret-hello");
}

#[tokio::test]
async fn test_node_rpc_end_to_end_attached() {
    // Full RPC round-trip across two attached Nodes, using the real
    // send_request / send_response / dispatch_request helpers.
    use mlink_core::api::rpc::{dispatch_request, rpc_request};

    let (node_a, _ta) = make_node(false).await;
    let (node_b, _tb) = make_node(false).await;

    let (ca, cb) = mock_pair();
    node_a.attach_connection("b".into(), Box::new(ca)).await;
    node_b.attach_connection("a".into(), Box::new(cb)).await;

    // A registers an "uppercase" handler.
    let reg_a = Arc::new(RpcRegistry::new());
    reg_a
        .register(
            "uppercase".into(),
            Box::new(|data: Vec<u8>| {
                Box::pin(async move {
                    let s = String::from_utf8(data).unwrap_or_default();
                    Ok(s.to_uppercase().into_bytes())
                })
            }),
        )
        .await;

    // We need two background tasks:
    //  * A-side: receive the Request frame and dispatch it, which writes a
    //    Response back over the mock.
    //  * B-side: receive the Response frame and complete the PendingRequests
    //    entry so rpc_request's oneshot waiter wakes up.
    let node_a = Arc::new(node_a);
    let node_b = Arc::new(node_b);
    let pending = Arc::new(PendingRequests::new());

    let node_a_c = Arc::clone(&node_a);
    let reg_a_c = Arc::clone(&reg_a);
    let dispatch_task = tokio::spawn(async move {
        let (frame, payload) = node_a_c.recv_raw("b").await.expect("a recv");
        let (_, _, mt) = decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Request);
        dispatch_request(&*node_a_c, &reg_a_c, "b", &payload)
            .await
            .expect("a dispatch");
    });

    let node_b_recv = Arc::clone(&node_b);
    let pending_recv = Arc::clone(&pending);
    let response_reader = tokio::spawn(async move {
        let (frame, payload) = node_b_recv.recv_raw("a").await.expect("b recv resp");
        let (_, _, mt) = decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Response);
        let resp = decode_response(&payload).expect("decode resp");
        pending_recv.complete(resp).await;
    });

    let out = rpc_request(&*node_b, &pending, "a", "uppercase", b"mlink", 2000)
        .await
        .expect("rpc");
    assert_eq!(out, b"MLINK");

    dispatch_task.await.expect("dispatch task");
    response_reader.await.expect("response reader");
}

#[tokio::test]
async fn test_node_heartbeat_roundtrip() {
    // send_heartbeat produces a Heartbeat-typed frame; when the peer decodes
    // it, recv_raw bumps last_heartbeat. check_heartbeat returns true.
    let (node_a, _ta) = make_node(false).await;
    let (node_b, _tb) = make_node(false).await;

    let (ca, cb) = mock_pair();
    node_a.attach_connection("b".into(), Box::new(ca)).await;
    node_b.attach_connection("a".into(), Box::new(cb)).await;

    node_a.send_heartbeat("b").await.expect("send hb");
    let (frame, _payload) = node_b.recv_raw("a").await.expect("b recv hb");
    let (_, _, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Heartbeat);

    assert!(node_b.check_heartbeat("a").await.expect("check"));
}
