use async_trait::async_trait;
use std::time::Instant;

use mlink_core::core::peer::{generate_app_uuid, Peer, PeerManager};
use mlink_core::protocol::errors::{MlinkError, Result};
use mlink_core::protocol::frame::{decode_frame, encode_frame};
use mlink_core::protocol::types::{
    encode_flags, Frame, MessageType, HEADER_SIZE, MAGIC, PROTOCOL_VERSION,
};
use mlink_core::transport::{
    Connection, DiscoveredPeer, Transport, TransportCapabilities,
};

// ============================================================================
// frame module
// ============================================================================

fn make_frame(flags: u8, seq: u16, payload: Vec<u8>) -> Frame {
    let length = payload.len() as u16;
    Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags,
        seq,
        length,
        payload,
    }
}

#[test]
fn test_frame_encode_decode_roundtrip() {
    let frame = make_frame(
        encode_flags(false, false, MessageType::Message),
        0xABCD,
        vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
    );
    let bytes = encode_frame(&frame);
    let back = decode_frame(&bytes).expect("decode");
    assert_eq!(back, frame);

    // sanity on wire layout
    assert_eq!(bytes.len(), HEADER_SIZE + 8);
    assert_eq!(&bytes[0..2], &MAGIC.to_be_bytes());
    assert_eq!(bytes[2], PROTOCOL_VERSION);
    assert_eq!(&bytes[4..6], &0xABCDu16.to_be_bytes());
    assert_eq!(&bytes[6..8], &8u16.to_be_bytes());
}

#[test]
fn test_frame_invalid_magic() {
    let frame = make_frame(
        encode_flags(false, false, MessageType::Heartbeat),
        1,
        vec![0xAA],
    );
    let mut bytes = encode_frame(&frame);
    // Corrupt the magic bytes.
    bytes[0] = 0xFF;
    bytes[1] = 0xEE;

    let err = decode_frame(&bytes).expect_err("bad magic should fail");
    match err {
        MlinkError::CodecError(msg) => {
            assert!(
                msg.to_lowercase().contains("magic"),
                "error should mention magic, got: {}",
                msg
            );
        }
        other => panic!("expected CodecError, got {:?}", other),
    }
}

#[test]
fn test_frame_invalid_version() {
    let frame = make_frame(
        encode_flags(false, false, MessageType::Ack),
        2,
        vec![0xBB],
    );
    let mut bytes = encode_frame(&frame);
    // Corrupt the version byte (index 2).
    bytes[2] = 0xFE;

    let err = decode_frame(&bytes).expect_err("bad version should fail");
    match err {
        MlinkError::CodecError(msg) => {
            assert!(
                msg.to_lowercase().contains("version"),
                "error should mention version, got: {}",
                msg
            );
        }
        other => panic!("expected CodecError, got {:?}", other),
    }
}

#[test]
fn test_frame_empty_payload() {
    let frame = make_frame(
        encode_flags(false, false, MessageType::Heartbeat),
        0,
        vec![],
    );
    let bytes = encode_frame(&frame);
    assert_eq!(bytes.len(), HEADER_SIZE);
    assert_eq!(&bytes[6..8], &[0x00, 0x00]);

    let back = decode_frame(&bytes).expect("decode empty");
    assert_eq!(back, frame);
    assert_eq!(back.payload.len(), 0);
    assert_eq!(back.length, 0);
}

#[test]
fn test_frame_with_flags() {
    // compress + encrypt bits both set on a Message frame.
    let flags = encode_flags(true, true, MessageType::Message);
    assert_eq!(flags & 0b1100_0000, 0b1100_0000);

    let frame = make_frame(flags, 0x0042, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    let bytes = encode_frame(&frame);
    let back = decode_frame(&bytes).expect("decode flags");
    assert_eq!(back, frame);
    assert_eq!(back.flags & 0b1000_0000, 0b1000_0000, "compress bit lost");
    assert_eq!(back.flags & 0b0100_0000, 0b0100_0000, "encrypt bit lost");
}

// ============================================================================
// transport trait compile checks
// ============================================================================

struct MockConnection {
    peer_id: String,
    closed: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl Connection for MockConnection {
    async fn read(&self) -> Result<Vec<u8>> {
        Ok(b"mock-read".to_vec())
    }

    async fn write(&self, _data: &[u8]) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

struct MockTransport {
    id: String,
    mtu: usize,
}

#[async_trait]
impl Transport for MockTransport {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 8,
            throughput_bps: 1_000_000,
            latency_ms: 20,
            reliable: true,
            bidirectional: true,
        }
    }

    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>> {
        Ok(vec![DiscoveredPeer {
            id: "peer-mock".into(),
            name: "Mock Peer".into(),
            rssi: Some(-50),
            metadata: vec![],
        }])
    }

    async fn connect(&mut self, peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
        Ok(Box::new(MockConnection {
            peer_id: peer.id.clone(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        Ok(Box::new(MockConnection {
            peer_id: "listener".into(),
            closed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    fn mtu(&self) -> usize {
        self.mtu
    }
}

#[tokio::test]
async fn test_mock_transport_compiles() {
    let mut t = MockTransport {
        id: "mock-transport".into(),
        mtu: 512,
    };
    assert_eq!(t.id(), "mock-transport");
    assert_eq!(t.mtu(), 512);

    let caps = t.capabilities();
    assert_eq!(caps.max_peers, 8);
    assert!(caps.reliable);
    assert!(caps.bidirectional);

    let peers = t.discover().await.expect("discover");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].id, "peer-mock");

    // Also exercise the Transport impl as a trait object to ensure object safety.
    let boxed: Box<dyn Transport> = Box::new(MockTransport {
        id: "boxed".into(),
        mtu: 256,
    });
    assert_eq!(boxed.id(), "boxed");
    assert_eq!(boxed.mtu(), 256);
}

#[tokio::test]
async fn test_mock_connection_compiles() {
    let conn: Box<dyn Connection> = Box::new(MockConnection {
        peer_id: "peer-42".into(),
        closed: std::sync::atomic::AtomicBool::new(false),
    });
    assert_eq!(conn.peer_id(), "peer-42");

    let data = conn.read().await.expect("read");
    assert_eq!(data, b"mock-read");

    conn.write(b"hello").await.expect("write");
    conn.close().await.expect("close");
}

// ============================================================================
// peer module
// ============================================================================

fn sample_peer(id: &str) -> Peer {
    Peer {
        id: id.into(),
        name: format!("peer-{}", id),
        app_uuid: generate_app_uuid(),
        connected_at: Instant::now(),
        transport_id: "mock".into(),
    }
}

#[tokio::test]
async fn test_peer_manager_add_get() {
    let mgr = PeerManager::new();
    let peer = sample_peer("alpha");
    mgr.add(peer.clone()).await;

    let got = mgr.get("alpha").await.expect("peer should exist after add");
    assert_eq!(got.id, "alpha");
    assert_eq!(got.name, "peer-alpha");
    assert_eq!(got.transport_id, "mock");
    assert_eq!(mgr.count().await, 1);
}

#[tokio::test]
async fn test_peer_manager_remove() {
    let mgr = PeerManager::new();
    mgr.add(sample_peer("beta")).await;
    assert!(mgr.get("beta").await.is_some());

    let removed = mgr.remove("beta").await;
    assert!(removed.is_some(), "remove should return the removed peer");
    assert_eq!(removed.unwrap().id, "beta");

    assert!(
        mgr.get("beta").await.is_none(),
        "get after remove should return None"
    );
    assert_eq!(mgr.count().await, 0);

    // Removing a missing id yields None without panicking.
    assert!(mgr.remove("beta").await.is_none());
}

#[tokio::test]
async fn test_peer_manager_list() {
    let mgr = PeerManager::new();
    mgr.add(sample_peer("a")).await;
    mgr.add(sample_peer("b")).await;
    mgr.add(sample_peer("c")).await;

    let peers = mgr.list().await;
    assert_eq!(peers.len(), 3);

    let mut ids: Vec<_> = peers.iter().map(|p| p.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["a".to_string(), "b".into(), "c".into()]);
    assert_eq!(mgr.count().await, 3);
}

#[test]
fn test_generate_app_uuid() {
    // Format: 8-4-4-4-12 hyphen-separated hex.
    let uuid = generate_app_uuid();
    assert_eq!(uuid.len(), 36, "uuid length should be 36, got {}", uuid);

    let parts: Vec<&str> = uuid.split('-').collect();
    assert_eq!(parts.len(), 5, "uuid should have 5 hyphen-separated parts: {}", uuid);
    assert_eq!(parts[0].len(), 8, "part 0 should be 8 chars: {}", parts[0]);
    assert_eq!(parts[1].len(), 4, "part 1 should be 4 chars: {}", parts[1]);
    assert_eq!(parts[2].len(), 4, "part 2 should be 4 chars: {}", parts[2]);
    assert_eq!(parts[3].len(), 4, "part 3 should be 4 chars: {}", parts[3]);
    assert_eq!(parts[4].len(), 12, "part 4 should be 12 chars: {}", parts[4]);

    for part in parts {
        assert!(
            part.chars().all(|c| c.is_ascii_hexdigit()),
            "non-hex char in part: {}",
            part
        );
    }

    // Two calls should produce different UUIDs.
    let a = generate_app_uuid();
    let b = generate_app_uuid();
    assert_ne!(a, b, "uuids should be unique per call");
}
