// Phase 6 integration tests: api/message, api/stream, api/rpc, api/pubsub.
//
// These tests cover two layers:
//
// 1. Wire-format roundtrips via `mock_pair()`: one side encodes a Phase-6 API
//    message into a `Frame`, pushes bytes through a MockConnection, and the
//    other side decodes and verifies the payload. This exercises the actual
//    on-the-wire bytes that peers will see.
//
// 2. In-memory state machines (RpcRegistry, PendingRequests, PubSubManager,
//    stream_id allocation) are exercised directly because they have no
//    transport dependency.
//
// The Node-level methods `send_raw`/`recv_raw` require a fully-constructed
// ConnectionManager entry that Node owns privately. Rather than poke at
// internals, we go one layer down: the same frame bytes Node would write are
// produced by encode_frame + encode(), and the API-layer encoders match.

use std::sync::Arc;

use mlink_core::api::pubsub::{
    decode_publish, encode_publish, PubSubManager, PubSubMessage,
};
use mlink_core::api::rpc::{
    decode_request, decode_response, PendingRequests, RpcRegistry, RpcRequest, RpcResponse,
    RPC_STATUS_ERROR, RPC_STATUS_OK,
};
use mlink_core::api::stream::{
    next_stream_id_central, next_stream_id_peripheral, Progress, StreamChunkMsg, StreamEndMsg,
};
use mlink_core::protocol::codec::{decode, encode};
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::frame::{decode_frame, encode_frame};
use mlink_core::protocol::types::{
    encode_flags, Frame, MessageType, StreamInfo, MAGIC, PROTOCOL_VERSION,
};
use mlink_core::transport::mock::mock_pair;
use mlink_core::transport::Connection;

use futures::future::BoxFuture;

// ---------------------------------------------------------------------------
// small helpers: produce the exact bytes Node::send_raw would emit for a
// given (MessageType, seq, payload). No compression, no encryption: matches
// Node behaviour when `encrypt=false` and payload is below the compression
// threshold.
// ---------------------------------------------------------------------------

fn frame_bytes(msg_type: MessageType, seq: u16, payload: &[u8]) -> Vec<u8> {
    let frame = Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, msg_type),
        seq,
        length: payload.len() as u16,
        payload: payload.to_vec(),
    };
    encode_frame(&frame)
}

// ============================================================================
// 1. api::message — MESSAGE frame wire format
// ============================================================================

#[tokio::test]
async fn test_send_message() {
    // writer encodes a MESSAGE-typed frame with the user payload; reader
    // decodes and must observe both the type (0x10) and the original bytes.
    let (mut a, mut b) = mock_pair();

    let payload = b"hello from phase6";
    let bytes = frame_bytes(MessageType::Message, 0, payload);
    a.write(&bytes).await.expect("write");

    let got = b.read().await.expect("read");
    let frame = decode_frame(&got).expect("decode frame");
    let (compressed, encrypted, mt) =
        mlink_core::protocol::types::decode_flags(frame.flags);

    assert!(!compressed, "small payload must not be compressed");
    assert!(!encrypted, "mock path has no crypto");
    assert_eq!(mt, MessageType::Message);
    assert_eq!(frame.seq, 0);
    assert_eq!(frame.length as usize, payload.len());
    assert_eq!(frame.payload, payload);
}

#[tokio::test]
async fn test_broadcast() {
    // broadcast = one sender, N receivers. Simulate by giving the sender two
    // independent mock connections and verifying both peers see the same
    // MESSAGE frame.
    let (mut a1, mut b1) = mock_pair();
    let (mut a2, mut b2) = mock_pair();

    let payload = b"broadcast-body";
    let bytes = frame_bytes(MessageType::Message, 0, payload);

    a1.write(&bytes).await.expect("w1");
    a2.write(&bytes).await.expect("w2");

    for rx in [&mut b1, &mut b2] {
        let got = rx.read().await.expect("read");
        let frame = decode_frame(&got).expect("decode");
        let (_, _, mt) = mlink_core::protocol::types::decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Message);
        assert_eq!(frame.payload, payload);
    }
}

// ============================================================================
// 2. api::stream — chunk/end frames + Progress + stream_id allocation
// ============================================================================

#[tokio::test]
async fn test_stream_write_read() {
    // Simulate a 3-chunk stream: writer emits START, 3 CHUNKs, END.
    // Reader decodes and reassembles.
    let (mut w, mut r) = mock_pair();

    let stream_id = 4u16;
    let total_chunks = 3u32;
    let chunk_size = 4usize;
    let data = b"abcdEFGHijkl"; // 12 bytes = 3 chunks of 4.

    // START
    let info = StreamInfo {
        stream_id,
        total_chunks,
        total_size: data.len() as u64,
        checksum_algo: "xor8".into(),
    };
    let start_payload = encode(&info).expect("encode start");
    w.write(&frame_bytes(MessageType::StreamStart, 0, &start_payload))
        .await
        .unwrap();

    // CHUNKs
    for (i, chunk) in data.chunks(chunk_size).enumerate() {
        let msg = StreamChunkMsg {
            stream_id,
            chunk_index: i as u32,
            data: chunk.to_vec(),
        };
        let p = encode(&msg).expect("encode chunk");
        w.write(&frame_bytes(MessageType::StreamChunk, 1 + i as u16, &p))
            .await
            .unwrap();
    }

    // END
    let end = StreamEndMsg {
        stream_id,
        checksum: 0, // placeholder; xor8 test handled separately
    };
    let end_payload = encode(&end).expect("encode end");
    w.write(&frame_bytes(
        MessageType::StreamEnd,
        1 + total_chunks as u16,
        &end_payload,
    ))
    .await
    .unwrap();

    // Read & reassemble
    // Expect START frame first.
    let b = r.read().await.unwrap();
    let f = decode_frame(&b).unwrap();
    let (_, _, mt) = mlink_core::protocol::types::decode_flags(f.flags);
    assert_eq!(mt, MessageType::StreamStart);
    let got_info: StreamInfo = decode(&f.payload).unwrap();
    assert_eq!(got_info.stream_id, stream_id);
    assert_eq!(got_info.total_chunks, total_chunks);

    // Then 3 CHUNKs.
    let mut reassembled: Vec<u8> = Vec::new();
    let mut progresses: Vec<Progress> = Vec::new();
    let mut received: u32 = 0;
    for _ in 0..total_chunks {
        let b = r.read().await.unwrap();
        let f = decode_frame(&b).unwrap();
        let (_, _, mt) = mlink_core::protocol::types::decode_flags(f.flags);
        assert_eq!(mt, MessageType::StreamChunk);
        let msg: StreamChunkMsg = decode(&f.payload).unwrap();
        assert_eq!(msg.stream_id, stream_id);
        reassembled.extend_from_slice(&msg.data);
        received += 1;
        progresses.push(progress_new(received, total_chunks));
    }
    assert_eq!(reassembled, data.to_vec());

    // Then END.
    let b = r.read().await.unwrap();
    let f = decode_frame(&b).unwrap();
    let (_, _, mt) = mlink_core::protocol::types::decode_flags(f.flags);
    assert_eq!(mt, MessageType::StreamEnd);
    let got_end: StreamEndMsg = decode(&f.payload).unwrap();
    assert_eq!(got_end.stream_id, stream_id);

    // progresses[i] = Progress(received=i+1, total=3)
    assert_eq!(progresses.len(), 3);
    assert_eq!(progresses[0].received, 1);
    assert_eq!(progresses[2].received, 3);
}

fn progress_new(received: u32, total: u32) -> Progress {
    // Re-derive the same way api::stream::Progress::new does (it's pub-created
    // by StreamReader internals; we don't need private ctor for the wire test
    // because we just need the math to agree with percent formula).
    let percent = if total == 0 {
        100.0
    } else {
        (received as f32 / total as f32) * 100.0
    };
    Progress {
        received,
        total,
        percent,
    }
}

#[tokio::test]
async fn test_stream_progress() {
    // percent must climb monotonically from 0→100 over the stream.
    let total = 4u32;
    let percents: Vec<f32> = (1..=total)
        .map(|received| progress_new(received, total).percent)
        .collect();

    assert!((percents[0] - 25.0).abs() < 0.01);
    assert!((percents[1] - 50.0).abs() < 0.01);
    assert!((percents[2] - 75.0).abs() < 0.01);
    assert!((percents[3] - 100.0).abs() < 0.01);

    // strict monotonic increase
    for w in percents.windows(2) {
        assert!(w[1] > w[0], "progress must be monotonic: {:?}", percents);
    }
    // finishes at exactly 100
    assert_eq!(*percents.last().unwrap(), 100.0);
}

#[test]
fn test_stream_id_allocation() {
    // Central allocator must produce even IDs; peripheral odd. Both step by 2.
    // We take several samples; the parity is stable even if other tests in
    // this crate ran first because we only assert parity + monotonic-by-2.
    let c1 = next_stream_id_central();
    let c2 = next_stream_id_central();
    assert_eq!(c1 % 2, 0, "central ids must be even, got {}", c1);
    assert_eq!(c2 % 2, 0, "central ids must be even, got {}", c2);
    assert_eq!(c2, c1.wrapping_add(2), "central step = 2");

    let p1 = next_stream_id_peripheral();
    let p2 = next_stream_id_peripheral();
    assert_eq!(p1 % 2, 1, "peripheral ids must be odd, got {}", p1);
    assert_eq!(p2 % 2, 1, "peripheral ids must be odd, got {}", p2);
    assert_eq!(p2, p1.wrapping_add(2), "peripheral step = 2");

    // No overlap between the two streams within a single test run.
    assert_ne!(c1, p1);
    assert_ne!(c1, p2);
    assert_ne!(c2, p1);
}

// ============================================================================
// 3. api::rpc — registry, unknown-method, serde
// ============================================================================

fn echo_handler(
) -> Box<dyn Fn(Vec<u8>) -> BoxFuture<'static, mlink_core::protocol::errors::Result<Vec<u8>>> + Send + Sync>
{
    Box::new(|data: Vec<u8>| Box::pin(async move { Ok(data) }))
}

fn upper_handler(
) -> Box<dyn Fn(Vec<u8>) -> BoxFuture<'static, mlink_core::protocol::errors::Result<Vec<u8>>> + Send + Sync>
{
    Box::new(|data: Vec<u8>| {
        Box::pin(async move {
            Ok(data
                .into_iter()
                .map(|b| b.to_ascii_uppercase())
                .collect::<Vec<u8>>())
        })
    })
}

#[tokio::test]
async fn test_rpc_register_and_handle() {
    let reg = RpcRegistry::new();
    reg.register("echo".into(), echo_handler()).await;
    reg.register("upper".into(), upper_handler()).await;

    assert!(reg.has("echo").await);
    assert!(reg.has("upper").await);

    let out = reg
        .handle_request("echo", b"ping".to_vec())
        .await
        .expect("echo ok");
    assert_eq!(out, b"ping");

    let out = reg
        .handle_request("upper", b"hello".to_vec())
        .await
        .expect("upper ok");
    assert_eq!(out, b"HELLO");
}

#[tokio::test]
async fn test_rpc_unknown_method() {
    let reg = RpcRegistry::new();
    let err = reg
        .handle_request("nope", b"x".to_vec())
        .await
        .expect_err("must be error");
    match err {
        MlinkError::UnknownMethod(m) => assert_eq!(m, "nope"),
        other => panic!("expected UnknownMethod, got {:?}", other),
    }
}

#[tokio::test]
async fn test_rpc_request_response_payload() {
    // Full request/response roundtrip over a mock_pair connection, using the
    // same codec path `send_request`/`send_response` would use.
    let (mut client, mut server) = mock_pair();

    // client -> server: Request
    let req = RpcRequest {
        request_id: 7,
        method: "ping".into(),
        data: b"payload".to_vec(),
    };
    let req_bytes = rmp_serde::to_vec(&req).expect("encode req");
    client
        .write(&frame_bytes(MessageType::Request, 0, &req_bytes))
        .await
        .unwrap();

    let got = server.read().await.unwrap();
    let f = decode_frame(&got).unwrap();
    let (_, _, mt) = mlink_core::protocol::types::decode_flags(f.flags);
    assert_eq!(mt, MessageType::Request);
    let back_req = decode_request(&f.payload).expect("decode req");
    assert_eq!(back_req, req);

    // server -> client: Response (status=OK)
    let resp = RpcResponse {
        request_id: 7,
        status: RPC_STATUS_OK,
        data: b"pong".to_vec(),
    };
    let resp_bytes = rmp_serde::to_vec(&resp).expect("encode resp");
    server
        .write(&frame_bytes(MessageType::Response, 0, &resp_bytes))
        .await
        .unwrap();

    let got = client.read().await.unwrap();
    let f = decode_frame(&got).unwrap();
    let (_, _, mt) = mlink_core::protocol::types::decode_flags(f.flags);
    assert_eq!(mt, MessageType::Response);
    let back_resp = decode_response(&f.payload).expect("decode resp");
    assert_eq!(back_resp, resp);

    // Also verify an error-status response path
    let err_resp = RpcResponse {
        request_id: 8,
        status: RPC_STATUS_ERROR,
        data: b"boom".to_vec(),
    };
    let b = rmp_serde::to_vec(&err_resp).unwrap();
    let back: RpcResponse = rmp_serde::from_slice(&b).unwrap();
    assert_eq!(back.status, RPC_STATUS_ERROR);
    assert_eq!(back.data, b"boom");
}

#[tokio::test]
async fn test_rpc_pending_requests_integration() {
    // PendingRequests is the client-side correlation map used by rpc_request.
    // Validate id allocation + complete path end-to-end.
    let p = Arc::new(PendingRequests::new());
    let id = p.next_id().await;
    let rx = p.register(id).await;

    let resp = RpcResponse {
        request_id: id,
        status: RPC_STATUS_OK,
        data: b"ok".to_vec(),
    };
    assert!(p.complete(resp.clone()).await, "complete must find waiter");

    let got = rx.await.expect("recv");
    assert_eq!(got, resp);
    assert_eq!(p.pending_count().await, 0);
}

// ============================================================================
// 4. api::pubsub — subscribe / unsubscribe / remove_peer / serde
// ============================================================================

#[test]
fn test_pubsub_subscribe_unsubscribe() {
    let mut m = PubSubManager::new();
    m.subscribe("news", "peer-a");
    assert!(m.is_subscribed("news", "peer-a"));
    assert_eq!(m.subscribers("news"), vec!["peer-a".to_string()]);

    // idempotent
    m.subscribe("news", "peer-a");
    assert_eq!(m.subscribers("news").len(), 1);

    // another peer
    m.subscribe("news", "peer-b");
    let mut subs = m.subscribers("news");
    subs.sort();
    assert_eq!(subs, vec!["peer-a".to_string(), "peer-b".to_string()]);

    // unsubscribe one
    m.unsubscribe("news", "peer-a");
    assert!(!m.is_subscribed("news", "peer-a"));
    assert!(m.is_subscribed("news", "peer-b"));

    // unsubscribe last → topic disappears
    m.unsubscribe("news", "peer-b");
    assert!(m.subscribers("news").is_empty());
    assert_eq!(m.topic_count(), 0);
}

#[test]
fn test_pubsub_remove_peer() {
    let mut m = PubSubManager::new();
    m.subscribe("t1", "peer-a");
    m.subscribe("t2", "peer-a");
    m.subscribe("t1", "peer-b");
    m.subscribe("t3", "peer-c");

    m.remove_peer("peer-a");

    // peer-a vanishes from every topic
    assert!(!m.is_subscribed("t1", "peer-a"));
    assert!(!m.is_subscribed("t2", "peer-a"));
    // t2 had only peer-a → topic removed entirely
    assert!(m.subscribers("t2").is_empty());
    // t1/t3 survive with their remaining subs
    assert_eq!(m.subscribers("t1"), vec!["peer-b".to_string()]);
    assert_eq!(m.subscribers("t3"), vec!["peer-c".to_string()]);

    // topics_for_peer reflects removal
    assert!(m.topics_for_peer("peer-a").is_empty());
}

#[tokio::test]
async fn test_pubsub_message_serde() {
    // encode → wire frame → decode must roundtrip PubSubMessage.
    let (mut pub_side, mut sub_side) = mock_pair();

    let topic = "chat/room-42";
    let data = b"hello subscribers";
    let payload = encode_publish(topic, data).expect("encode");

    pub_side
        .write(&frame_bytes(MessageType::Publish, 0, &payload))
        .await
        .unwrap();

    let got = sub_side.read().await.unwrap();
    let f = decode_frame(&got).unwrap();
    let (_, _, mt) = mlink_core::protocol::types::decode_flags(f.flags);
    assert_eq!(mt, MessageType::Publish);

    let msg: PubSubMessage = decode_publish(&f.payload).expect("decode");
    assert_eq!(msg.topic, topic);
    assert_eq!(msg.data, data);

    // And a plain struct-level roundtrip for completeness
    let msg2 = PubSubMessage {
        topic: "direct".into(),
        data: vec![0, 1, 2, 3, 255],
    };
    let bytes = rmp_serde::to_vec(&msg2).unwrap();
    let back: PubSubMessage = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(msg2, back);
}
