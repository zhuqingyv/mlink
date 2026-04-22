use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use mlink_core::core::{
    derive_aes_key, derive_shared_secret, derive_verification_code, decrypt, encrypt,
    generate_keypair, negotiate_role, perform_handshake, ConnectionManager, Role, Scanner,
    TrustStore, TrustedPeer,
};
use mlink_core::protocol::codec;
use mlink_core::protocol::errors::{MlinkError, Result};
use mlink_core::protocol::frame::{decode_frame, encode_frame};
use mlink_core::protocol::types::{
    encode_flags, Frame, Handshake, MessageType, MAGIC, PROTOCOL_VERSION,
};
use mlink_core::transport::mock::mock_pair;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport, TransportCapabilities};

// ============================================================================
// test helpers: a scripted Transport that returns canned discovery rounds
// ============================================================================

struct MockDiscoveryTransport {
    rounds: Arc<Mutex<Vec<Vec<DiscoveredPeer>>>>,
}

impl MockDiscoveryTransport {
    fn new(rounds: Vec<Vec<DiscoveredPeer>>) -> Self {
        Self {
            rounds: Arc::new(Mutex::new(rounds)),
        }
    }
}

#[async_trait]
impl Transport for MockDiscoveryTransport {
    fn id(&self) -> &str {
        "mock-discovery"
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
        let mut rounds = self.rounds.lock().unwrap();
        if rounds.is_empty() {
            Ok(Vec::new())
        } else {
            Ok(rounds.remove(0))
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

fn peer(id: &str) -> DiscoveredPeer {
    DiscoveredPeer {
        id: id.into(),
        name: format!("peer-{id}"),
        rssi: Some(-60),
        metadata: Vec::new(),
    }
}

// ============================================================================
// scanner
// ============================================================================

#[tokio::test]
async fn test_discover_once() {
    let transport = MockDiscoveryTransport::new(vec![vec![peer("p1"), peer("p2")]]);
    let mut scanner = Scanner::new(Box::new(transport), "self-uuid");

    let found = scanner.discover_once().await.expect("discover_once");
    assert_eq!(found.len(), 2, "should return both new peers");
    let mut ids: Vec<_> = found.iter().map(|p| p.id.clone()).collect();
    ids.sort();
    assert_eq!(ids, vec!["p1".to_string(), "p2".to_string()]);
    assert_eq!(scanner.seen_count(), 2);
}

#[tokio::test]
async fn test_discover_dedup() {
    let transport = MockDiscoveryTransport::new(vec![
        vec![peer("a"), peer("b")],
        vec![peer("a"), peer("b"), peer("c")],
        vec![peer("a")],
    ]);
    let mut scanner = Scanner::new(Box::new(transport), "self-uuid");

    let first = scanner.discover_once().await.expect("first");
    assert_eq!(first.len(), 2);

    let second = scanner.discover_once().await.expect("second");
    assert_eq!(second.len(), 1, "only c is new");
    assert_eq!(second[0].id, "c");

    let third = scanner.discover_once().await.expect("third");
    assert!(third.is_empty(), "a already seen, no new peers");

    assert_eq!(scanner.seen_count(), 3);
}

#[tokio::test]
async fn test_scanner_reset() {
    let transport = MockDiscoveryTransport::new(vec![
        vec![peer("x")],
        vec![peer("x"), peer("y")],
        vec![peer("x"), peer("y")],
    ]);
    let mut scanner = Scanner::new(Box::new(transport), "self-uuid");

    let r1 = scanner.discover_once().await.unwrap();
    assert_eq!(r1.len(), 1);

    let r2 = scanner.discover_once().await.unwrap();
    assert_eq!(r2.len(), 1, "only y is new");
    assert_eq!(scanner.seen_count(), 2);

    scanner.reset();
    assert_eq!(scanner.seen_count(), 0, "reset clears seen");

    let r3 = scanner.discover_once().await.unwrap();
    assert_eq!(r3.len(), 2, "after reset, both x and y are fresh again");
}

// ============================================================================
// connection: role negotiation
// ============================================================================

#[test]
fn test_negotiate_role_central() {
    assert_eq!(negotiate_role("aaa", "bbb"), Role::Central);
    assert_eq!(negotiate_role("uuid-1", "uuid-2"), Role::Central);
}

#[test]
fn test_negotiate_role_peripheral() {
    assert_eq!(negotiate_role("bbb", "aaa"), Role::Peripheral);
    assert_eq!(negotiate_role("uuid-2", "uuid-1"), Role::Peripheral);
}

#[test]
fn test_negotiate_role_equal() {
    // Equal UUIDs are pathological but the result must still be deterministic.
    assert_eq!(negotiate_role("same", "same"), Role::Peripheral);
}

// ============================================================================
// connection: handshake round-trip using mock_pair
// ============================================================================

fn sample_handshake(uuid: &str) -> Handshake {
    Handshake {
        app_uuid: uuid.into(),
        version: 1,
        mtu: 512,
        compress: true,
        encrypt: true,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: None,
    }
}

#[tokio::test]
async fn test_perform_handshake() {
    let (mut side_a, mut side_b) = mock_pair();

    let hs_a = sample_handshake("uuid-a");
    let hs_b = sample_handshake("uuid-b");

    let hs_a_clone = hs_a.clone();
    let hs_b_clone = hs_b.clone();

    let task_a = tokio::spawn(async move {
        perform_handshake(&mut side_a, &hs_a_clone).await
    });
    let task_b = tokio::spawn(async move {
        perform_handshake(&mut side_b, &hs_b_clone).await
    });

    let got_from_b = task_a.await.expect("task a joined").expect("hs a");
    let got_from_a = task_b.await.expect("task b joined").expect("hs b");

    assert_eq!(got_from_b, hs_b, "side A should have received B's handshake");
    assert_eq!(got_from_a, hs_a, "side B should have received A's handshake");
}

#[tokio::test]
async fn test_perform_handshake_rejects_wrong_message_type() {
    // Construct a Message frame and feed it as "response" to perform_handshake.
    let bogus = Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, MessageType::Message),
        seq: 0,
        length: 0,
        payload: vec![],
    };
    let bogus_bytes = encode_frame(&bogus);

    let (ours, theirs) = mock_pair();
    // Peer writes bogus frame; we then try to handshake.
    theirs.write(&bogus_bytes).await.expect("theirs write");

    let err = perform_handshake(&ours, &sample_handshake("me"))
        .await
        .expect_err("should reject non-handshake reply");
    assert!(matches!(err, MlinkError::CodecError(_)));

    // sanity: the handshake we wrote before reading should decode as Handshake.
    let sent_bytes = theirs.read().await.expect("read our out");
    let decoded = decode_frame(&sent_bytes).expect("decode");
    let _hs: Handshake = codec::decode(&decoded.payload).expect("decode hs");
}

// ============================================================================
// connection: manager CRUD
// ============================================================================

#[tokio::test]
async fn test_connection_manager_crud() {
    let mut mgr = ConnectionManager::new();
    assert_eq!(mgr.count(), 0);

    let (conn_a, _sink_a) = mock_pair();
    let (conn_b, _sink_b) = mock_pair();

    mgr.add("peer-a".into(), std::sync::Arc::new(conn_a));
    mgr.add("peer-b".into(), std::sync::Arc::new(conn_b));
    assert_eq!(mgr.count(), 2);

    let mut ids = mgr.list_ids();
    ids.sort();
    assert_eq!(ids, vec!["peer-a".to_string(), "peer-b".to_string()]);

    let got = mgr.shared("peer-a").expect("peer-a present");
    assert_eq!(got.peer_id(), "mock-peer-a");
    assert!(mgr.shared("peer-missing").is_none());

    let removed = mgr.remove("peer-a");
    assert!(removed.is_some());
    assert_eq!(mgr.count(), 1);
    assert!(mgr.shared("peer-a").is_none());

    assert!(mgr.remove("peer-a").is_none(), "double remove returns None");
}

// ============================================================================
// security: ECDH shared secret agreement
// ============================================================================

#[test]
fn test_ecdh_shared_secret() {
    let kp_a = generate_keypair().expect("kp a");
    let kp_b = generate_keypair().expect("kp b");

    let a_pub = kp_a.public_key_bytes.clone();
    let b_pub = kp_b.public_key_bytes.clone();

    let secret_a = derive_shared_secret(kp_a.private, &b_pub).expect("a derive");
    let secret_b = derive_shared_secret(kp_b.private, &a_pub).expect("b derive");

    assert_eq!(secret_a, secret_b, "ECDH secrets must agree");
    assert!(!secret_a.is_empty());
    assert_eq!(secret_a.len(), 32, "X25519 shared secret is 32 bytes");
}

// ============================================================================
// security: verification code
// ============================================================================

#[test]
fn test_verification_code() {
    let secret = vec![0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];

    let code1 = derive_verification_code(&secret);
    let code2 = derive_verification_code(&secret);
    assert_eq!(code1, code2, "same secret -> same code");

    // 6-digit code means value < 1_000_000.
    assert!(code1 < 1_000_000, "verification code must fit in 6 digits, got {}", code1);

    let different = derive_verification_code(&[0xde, 0xad, 0xbe, 0xef]);
    assert_ne!(code1, different, "different secrets should almost always yield different codes");
}

// ============================================================================
// security: encrypt / decrypt
// ============================================================================

#[test]
fn test_encrypt_decrypt_roundtrip() {
    let key = derive_aes_key(b"unit-test-secret");
    let plaintext = b"hello from phase4 tests";
    let ciphertext = encrypt(plaintext, &key, 7).expect("encrypt");

    assert_ne!(&ciphertext[..], plaintext, "ciphertext must differ from plaintext");
    assert!(
        ciphertext.len() > plaintext.len(),
        "ciphertext must include GCM tag"
    );

    let recovered = decrypt(&ciphertext, &key, 7).expect("decrypt");
    assert_eq!(recovered, plaintext);
}

#[test]
fn test_decrypt_wrong_key() {
    let key_right = derive_aes_key(b"right");
    let key_wrong = derive_aes_key(b"wrong");
    assert_ne!(key_right, key_wrong);

    let ciphertext = encrypt(b"payload", &key_right, 1).expect("encrypt");
    let err = decrypt(&ciphertext, &key_wrong, 1).expect_err("wrong key must fail");
    assert!(matches!(err, MlinkError::SecurityError(_)));
}

#[test]
fn test_decrypt_wrong_seq() {
    let key = derive_aes_key(b"seq-test");
    let ciphertext = encrypt(b"payload", &key, 10).expect("encrypt");

    // seq is the nonce — flipping seq must make decryption fail.
    let err = decrypt(&ciphertext, &key, 11).expect_err("wrong seq must fail");
    assert!(matches!(err, MlinkError::SecurityError(_)));
}

// ============================================================================
// security: trust store CRUD + persistence
// ============================================================================

fn trusted(uuid: &str, name: &str) -> TrustedPeer {
    TrustedPeer {
        app_uuid: uuid.into(),
        public_key: vec![0xAA, 0xBB, 0xCC],
        name: name.into(),
        trusted_at: "2026-04-19T00:00:00Z".into(),
    }
}

#[test]
fn test_trust_store_crud() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("trusted.json");
    let mut store = TrustStore::new(path).expect("new");

    assert!(store.list().is_empty());
    assert!(!store.is_trusted("uuid-x"));

    store.add(trusted("uuid-x", "Alice")).expect("add x");
    store.add(trusted("uuid-y", "Bob")).expect("add y");

    assert!(store.is_trusted("uuid-x"));
    assert!(store.is_trusted("uuid-y"));
    assert!(!store.is_trusted("uuid-z"));
    assert_eq!(store.list().len(), 2);

    store.remove("uuid-x").expect("remove x");
    assert!(!store.is_trusted("uuid-x"));
    assert!(store.is_trusted("uuid-y"));
    assert_eq!(store.list().len(), 1);
}

#[test]
fn test_trust_store_persistence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("trusted.json");

    {
        let mut store = TrustStore::new(path.clone()).expect("new");
        store.add(trusted("uuid-a", "Alice")).expect("add a");
        store.add(trusted("uuid-b", "Bob")).expect("add b");
    }

    // File must exist on disk after the first store is dropped.
    assert!(path.exists(), "persistence should write the trust file");

    let reloaded = TrustStore::new(path).expect("reload");
    assert_eq!(reloaded.list().len(), 2);
    assert!(reloaded.is_trusted("uuid-a"));
    assert!(reloaded.is_trusted("uuid-b"));
    let a = reloaded.get("uuid-a").expect("present");
    assert_eq!(a.name, "Alice");
    assert_eq!(a.public_key, vec![0xAA, 0xBB, 0xCC]);
}
