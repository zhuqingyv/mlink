// Phase 8 tests — room codes, scanner room filtering, BLE peripheral.
//
// Tests target three surfaces per the Phase 8 spec:
//   1. core::room         — RoomCode generation, hashing, app_id isolation, RoomManager CRUD.
//   2. core::scanner      — discover_once filters by currently-joined room_hashes.
//   3. transport::ble     — peripheral-advertising surface compiles with expected capabilities.
//
// BLE peripheral tests do NOT require a real BLE adapter; they only assert
// construction + capability metadata. Anything touching a live radio belongs
// in a manual / ignored integration test.

use std::collections::HashSet;

use async_trait::async_trait;
use mlink_core::core::room::{
    generate_room_code, room_app_id, room_hash, RoomManager,
};
use mlink_core::core::scanner::Scanner;
use mlink_core::protocol::errors::{MlinkError, Result};
use mlink_core::transport::{
    Connection, DiscoveredPeer, Transport, TransportCapabilities,
};

// ---------------------------------------------------------------------------
// 1. core::room
// ---------------------------------------------------------------------------

#[test]
fn test_generate_room_code_format() {
    // Must be exactly 6 digits (ASCII 0-9), stable length, no separators.
    for _ in 0..64 {
        let code = generate_room_code();
        assert_eq!(code.len(), 6, "expected 6-char code, got {:?}", code);
        assert!(
            code.chars().all(|c| c.is_ascii_digit()),
            "expected all digits, got {:?}",
            code
        );
    }
}

#[test]
fn test_generate_room_code_unique() {
    // 256 consecutive generations should not all collide. With 10^6 space and
    // 256 draws, expected collisions ≈ 0.03, so any collision-free run is
    // typical. We allow a tiny number of collisions to avoid flakes.
    let mut seen = HashSet::new();
    let mut collisions = 0usize;
    for _ in 0..256 {
        let c = generate_room_code();
        if !seen.insert(c) {
            collisions += 1;
        }
    }
    assert!(
        collisions <= 2,
        "too many collisions ({collisions}/256) — RNG quality likely broken"
    );
}

#[test]
fn test_room_hash_consistency() {
    // Same code → same 8-byte hash, deterministically.
    let a = room_hash("123456");
    let b = room_hash("123456");
    assert_eq!(a, b, "same code must produce same hash");
    assert_eq!(a.len(), 8);
}

#[test]
fn test_room_hash_different_codes() {
    // Different codes → different hashes (collision extremely unlikely in 64 bits).
    let a = room_hash("123456");
    let b = room_hash("654321");
    assert_ne!(a, b, "distinct codes must hash differently");
}

#[test]
fn test_room_app_id_isolation() {
    // Same underlying app_uuid but different room codes must yield different
    // app_ids — this is the cross-room unlinkability property.
    let uuid = "11111111-2222-3333-4444-555555555555";
    let id_a = room_app_id(uuid, "123456");
    let id_b = room_app_id(uuid, "654321");

    assert_ne!(id_a, id_b, "same uuid + different rooms must unlink");
    // Deterministic within a (uuid, code) pair.
    assert_eq!(id_a, room_app_id(uuid, "123456"));
    // 16-byte hex → 32 chars.
    assert_eq!(id_a.len(), 32, "expected 16-byte hex id, got {id_a:?}");
    assert!(id_a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_room_manager_join_leave() {
    let mut mgr = RoomManager::new();
    assert!(!mgr.is_joined("123456"));
    assert!(mgr.list().is_empty());

    let state = mgr.join("123456");
    assert_eq!(state.code, "123456");
    assert_eq!(state.hash, room_hash("123456"));
    assert!(mgr.is_joined("123456"));
    assert_eq!(mgr.list().len(), 1);

    // re-join same code must not grow the set
    mgr.join("123456");
    assert_eq!(mgr.list().len(), 1);

    mgr.join("654321");
    let mut codes: Vec<String> = mgr.list().iter().map(|r| r.code.clone()).collect();
    codes.sort();
    assert_eq!(codes, vec!["123456".to_string(), "654321".to_string()]);

    let gone = mgr.leave("123456");
    assert!(gone.is_some(), "leave must return the removed state");
    assert!(!mgr.is_joined("123456"));
    assert!(mgr.is_joined("654321"));

    mgr.leave("654321");
    assert!(mgr.list().is_empty());
    // Idempotent leave on missing room.
    assert!(mgr.leave("nope").is_none());
}

#[test]
fn test_room_manager_peers() {
    let mut mgr = RoomManager::new();
    mgr.join("123456");

    mgr.add_peer("123456", "peer-a");
    mgr.add_peer("123456", "peer-b");

    let mut peers = mgr.peers("123456");
    peers.sort();
    assert_eq!(peers, vec!["peer-a".to_string(), "peer-b".to_string()]);

    // idempotent add_peer
    mgr.add_peer("123456", "peer-a");
    assert_eq!(mgr.peers("123456").len(), 2);

    mgr.remove_peer("123456", "peer-a");
    assert_eq!(mgr.peers("123456"), vec!["peer-b".to_string()]);

    // peers() for a room we never joined returns empty, not a panic.
    assert!(mgr.peers("999999").is_empty());
}

#[test]
fn test_room_manager_active_hashes() {
    let mut mgr = RoomManager::new();
    mgr.join("111111");
    mgr.join("222222");
    mgr.join("333333");

    let mut got = mgr.active_hashes();
    got.sort();

    let mut expected = vec![
        room_hash("111111"),
        room_hash("222222"),
        room_hash("333333"),
    ];
    expected.sort();

    assert_eq!(got, expected, "active_hashes must cover all joined rooms");

    // Leaving removes the corresponding hash.
    mgr.leave("222222");
    let got = mgr.active_hashes();
    assert_eq!(got.len(), 2);
    assert!(!got.contains(&room_hash("222222")));
}

// ---------------------------------------------------------------------------
// 2. scanner room filtering
// ---------------------------------------------------------------------------

struct ScriptedTransport {
    rounds: Vec<Vec<DiscoveredPeer>>,
}

impl ScriptedTransport {
    fn new(rounds: Vec<Vec<DiscoveredPeer>>) -> Self {
        Self { rounds }
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    fn id(&self) -> &str {
        "scripted"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 10,
            throughput_bps: 0,
            latency_ms: 0,
            reliable: true,
            bidirectional: true,
        }
    }

    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>> {
        if self.rounds.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(self.rounds.remove(0))
        }
    }

    async fn connect(&mut self, _peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
        Err(MlinkError::HandlerError("not used".into()))
    }

    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        Err(MlinkError::HandlerError("not used".into()))
    }

    fn mtu(&self) -> usize {
        512
    }
}

fn peer_with_metadata(id: &str, metadata: Vec<u8>) -> DiscoveredPeer {
    DiscoveredPeer {
        id: id.into(),
        name: format!("name-{id}"),
        rssi: Some(-50),
        metadata,
    }
}

#[tokio::test]
async fn test_scanner_room_filter() {
    // Two peers advertise; only the one carrying the matching room_hash in
    // metadata must survive the scan.
    let target = room_hash("111111");
    let other = room_hash("222222");

    let matching = peer_with_metadata("peer-match", target.to_vec());
    let non_matching = peer_with_metadata("peer-miss", other.to_vec());

    let t = ScriptedTransport::new(vec![vec![matching.clone(), non_matching]]);
    let mut s = Scanner::new(Box::new(t), "self-uuid");
    s.set_room_hashes(vec![target]);

    let got = s.discover_once().await.unwrap();
    assert_eq!(got.len(), 1, "only matching peer must pass the filter");
    assert_eq!(got[0].id, "peer-match");
}

#[tokio::test]
async fn test_scanner_no_filter_when_empty() {
    // With no room_hashes set, scanner behaves like pre-Phase-8 and returns
    // every non-self peer regardless of metadata contents.
    let a = peer_with_metadata("peer-a", b"anything".to_vec());
    let b = peer_with_metadata("peer-b", Vec::new());

    let t = ScriptedTransport::new(vec![vec![a, b]]);
    let mut s = Scanner::new(Box::new(t), "self-uuid");
    // Explicit empty set — same as default.
    s.set_room_hashes(Vec::new());

    let got = s.discover_once().await.unwrap();
    let mut ids: Vec<String> = got.into_iter().map(|p| p.id).collect();
    ids.sort();
    assert_eq!(ids, vec!["peer-a".to_string(), "peer-b".to_string()]);
}

// ---------------------------------------------------------------------------
// 3. BLE peripheral surface (no live radio)
// ---------------------------------------------------------------------------

#[test]
fn test_ble_peripheral_exists() {
    // Sanity: the BLE transport still constructs and reports the expected
    // capability profile after Phase 8 peripheral work. We deliberately do
    // NOT exercise the live adapter here.
    let t = mlink_core::transport::ble::BleTransport::new();
    assert_eq!(t.id(), "ble");
    let caps = t.capabilities();
    assert!(caps.reliable);
    assert!(caps.bidirectional);
    assert!(caps.max_peers > 0);
}
