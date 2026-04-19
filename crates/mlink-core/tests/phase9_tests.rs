// Phase 9 tests — end-to-end integration via mock_pair.
//
// These tests exercise the full stack without a real BLE radio:
//   * mock_pair() gives two Connection endpoints wired through tokio channels.
//   * Frames are encoded/decoded through the real protocol codec.
//   * perform_handshake runs across both ends concurrently.
//   * RPC request/response round-trips using the real PendingRequests/RpcRegistry.
//   * RoomManager integration mirrors how Node would track joined rooms.
//   * Peripheral module is smoke-checked for compile-time presence.

use tokio::sync::Mutex;
use std::sync::Arc;

use mlink_core::core::connection::perform_handshake;
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
    conn: &mut dyn Connection,
    msg_type: MessageType,
    seq: u16,
    payload: &[u8],
) -> Result<()> {
    conn.write(&make_frame(msg_type, seq, payload.to_vec())).await
}

async fn recv_msg(conn: &mut dyn Connection) -> Result<(Frame, Vec<u8>)> {
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
    let (mut a, mut b) = mock_pair();

    let payload = b"hello from A".to_vec();
    send_msg(&mut a, MessageType::Message, 1, &payload).await.expect("send");

    let (frame, got) = recv_msg(&mut b).await.expect("recv");
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
    let (mut a, mut b) = mock_pair();

    send_msg(&mut a, MessageType::Message, 1, b"A->B").await.expect("a->b");
    let (_, got) = recv_msg(&mut b).await.expect("b recv");
    assert_eq!(got, b"A->B");

    send_msg(&mut b, MessageType::Message, 7, b"B->A").await.expect("b->a");
    let (frame, got) = recv_msg(&mut a).await.expect("a recv");
    assert_eq!(got, b"B->A");
    assert_eq!(frame.seq, 7);
}

#[tokio::test]
async fn test_e2e_large_payload_roundtrip() {
    // Multi-KB payload still survives end-to-end through the mock pipe.
    let (mut a, mut b) = mock_pair();

    let payload: Vec<u8> = (0u16..4096).map(|i| (i & 0xff) as u8).collect();
    send_msg(&mut a, MessageType::Message, 0, &payload).await.expect("send");

    let (frame, got) = recv_msg(&mut b).await.expect("recv");
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
    let (mut a, mut b) = mock_pair();
    send_msg(&mut a, MessageType::Message, 0, b"cross-room").await.expect("send");
    let (_, got) = recv_msg(&mut b).await.expect("recv");
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
    }
}

#[tokio::test]
async fn test_e2e_handshake() {
    // Two endpoints each drive perform_handshake concurrently; each must see
    // the peer's announced UUID come back through the real wire format.
    let (a, b) = mock_pair();
    let mut a: Box<dyn Connection> = Box::new(a);
    let mut b: Box<dyn Connection> = Box::new(b);

    let hs_a = sample_handshake("uuid-alpha", 512);
    let hs_b = sample_handshake("uuid-bravo", 256);

    let hs_a_clone = hs_a.clone();
    let hs_b_clone = hs_b.clone();

    let task_a = tokio::spawn(async move {
        let got = perform_handshake(a.as_mut(), &hs_a_clone).await.expect("a hs");
        (got, a)
    });
    let task_b = tokio::spawn(async move {
        let got = perform_handshake(b.as_mut(), &hs_b_clone).await.expect("b hs");
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
    let mut a: Box<dyn Connection> = Box::new(a);
    let mut b: Box<dyn Connection> = Box::new(b);

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

    let err = perform_handshake(a.as_mut(), &hs_a).await.unwrap_err();
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
    let a = Arc::new(Mutex::new(Box::new(a) as Box<dyn Connection>));
    let b = Arc::new(Mutex::new(Box::new(b) as Box<dyn Connection>));

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
    {
        let mut guard = b.lock().await;
        send_msg(&mut **guard, MessageType::Request, 1, &req_bytes)
            .await
            .expect("b send request");
    }

    // A-side task: receive request, dispatch, send response.
    let a_clone = Arc::clone(&a);
    let reg_clone = Arc::clone(&reg);
    let a_task = tokio::spawn(async move {
        let (frame, payload) = {
            let mut guard = a_clone.lock().await;
            recv_msg(&mut **guard).await.expect("a recv")
        };
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
        let mut guard = a_clone.lock().await;
        send_msg(
            &mut **guard,
            MessageType::Response,
            2,
            &resp_bytes,
        )
        .await
        .expect("a send resp");
    });

    // B-side: read response frame, decode, deliver to pending.
    let (frame, payload) = {
        let mut guard = b.lock().await;
        recv_msg(&mut **guard).await.expect("b recv resp")
    };
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
    let mut a: Box<dyn Connection> = Box::new(a);
    let mut b: Box<dyn Connection> = Box::new(b);

    let reg = RpcRegistry::new(); // no handlers registered

    // B sends request for "missing".
    let req = RpcRequest {
        request_id: 42,
        method: "missing".into(),
        data: vec![],
    };
    let req_bytes = rmp_serde::to_vec(&req).unwrap();
    send_msg(b.as_mut(), MessageType::Request, 0, &req_bytes).await.expect("b send");

    // A receives, handle_request returns UnknownMethod; encode error response.
    let (_, payload) = recv_msg(a.as_mut()).await.expect("a recv");
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
    send_msg(a.as_mut(), MessageType::Response, 0, &resp_bytes).await.expect("a send");

    // B reads, checks status.
    let (_, resp_payload) = recv_msg(b.as_mut()).await.expect("b recv");
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
    let mut a: Box<dyn Connection> = Box::new(a);
    let mut b: Box<dyn Connection> = Box::new(b);

    let hs_a = sample_handshake("uuid-alpha", 512);
    let hs_b = sample_handshake("uuid-bravo", 512);

    // Run handshakes concurrently using join!
    let hs_a_c = hs_a.clone();
    let hs_b_c = hs_b.clone();
    let ((got_a, mut a), (got_b, mut b)) = tokio::join!(
        async move {
            let got = perform_handshake(a.as_mut(), &hs_a_c).await.expect("a hs");
            (got, a)
        },
        async move {
            let got = perform_handshake(b.as_mut(), &hs_b_c).await.expect("b hs");
            (got, b)
        },
    );
    assert_eq!(got_a, hs_b);
    assert_eq!(got_b, hs_a);

    // Now swap to plain messages over the same channel.
    send_msg(a.as_mut(), MessageType::Message, 1, b"post-handshake").await.expect("a send");
    let (_, got) = recv_msg(b.as_mut()).await.expect("b recv");
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
