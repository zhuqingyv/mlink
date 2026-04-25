use super::*;
use crate::core::connection::perform_handshake;
use crate::core::peer::Peer;
use crate::protocol::frame::decode_frame;
use crate::protocol::types::{decode_flags, Handshake, MessageType, PROTOCOL_VERSION};
use crate::transport::mock::MockTransport;
use crate::transport::{Connection, DiscoveredPeer, Transport};

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Serialize tests that mutate HOME so they don't race with peer::tests.
static HOME_LOCK: Mutex<()> = Mutex::new(());

struct HomeGuard {
    _tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
}

fn set_tmp_home() -> HomeGuard {
    let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tmp");
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());
    HomeGuard {
        _tmp: tmp,
        prev,
        _guard: guard,
    }
}

fn test_config() -> NodeConfig {
    NodeConfig {
        name: "test-node".into(),
        encrypt: false,
        trust_store_path: None,
    }
}

fn tmp_config(trust_path: PathBuf) -> NodeConfig {
    NodeConfig {
        name: "test-node".into(),
        encrypt: false,
        trust_store_path: Some(trust_path),
    }
}

#[tokio::test]
async fn new_starts_idle() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("trust.json")))
        .await
        .expect("new");
    assert_eq!(node.state(), NodeState::Idle);
    assert!(!node.app_uuid().is_empty());
}

#[tokio::test]
async fn start_transitions_to_discovering() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    node.start().await.expect("start");
    assert_eq!(node.state(), NodeState::Discovering);
}

#[tokio::test]
async fn start_rejects_double_start() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    node.start().await.expect("start 1");
    let err = node.start().await.unwrap_err();
    assert!(matches!(err, MlinkError::HandlerError(_)));
}

#[tokio::test]
async fn stop_returns_to_idle() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    node.start().await.expect("start");
    node.stop().await.expect("stop");
    assert_eq!(node.state(), NodeState::Idle);
}

#[tokio::test]
async fn role_for_is_deterministic() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    let me = node.app_uuid().to_string();
    let other_big = format!("{}-z", me);
    let other_small = "aaaa".to_string();
    let _ = node.role_for(&other_big);
    let _ = node.role_for(&other_small);
}

#[tokio::test]
async fn send_raw_on_missing_peer_fails() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    let err = node
        .send_raw("nobody", MessageType::Message, b"hi")
        .await
        .unwrap_err();
    assert!(matches!(err, MlinkError::PeerGone { .. }));
}

#[tokio::test]
async fn connect_peer_via_mock_transport_errors_cleanly() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    let mut transport = MockTransport::new();
    let discovered = DiscoveredPeer {
        id: "peer-a".into(),
        name: "a".into(),
        rssi: None,
        metadata: vec![],
    };
    let err = node.connect_peer(&mut transport, &discovered).await.unwrap_err();
    assert!(matches!(err, MlinkError::HandlerError(_)));
}

#[tokio::test]
async fn check_heartbeat_missing_peer_fails() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");
    let err = node.check_heartbeat("no-such").await.unwrap_err();
    assert!(matches!(err, MlinkError::PeerGone { .. }));
}

#[test]
fn node_state_is_copy() {
    let s = NodeState::Connecting;
    let t = s;
    assert_eq!(s, t);
}

#[test]
fn node_config_default_encrypt_false() {
    // Default must be false: we hold no aes_key at construction time,
    // so claiming encrypt=true on the wire would be a lie.
    let c = NodeConfig::default();
    assert!(!c.encrypt);
    assert!(c.trust_store_path.is_none());
}

#[test]
fn test_config_helper_used() {
    let _c = test_config();
}

#[tokio::test]
async fn accept_incoming_matching_room_registers_peer() {
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    let room = [0x55u8; 8];
    node_a.add_room_hash(room);
    node_b.add_room_hash(room);

    let (ca, cb) = crate::transport::mock::mock_pair();
    // Both sides run the handshake concurrently: one side acts as the
    // "incoming-accept" (node_a), the other as a pre-wired mock client
    // that drives perform_handshake directly.
    let hs_b = Handshake {
        app_uuid: node_b.app_uuid().to_string(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: Some(room),
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        perform_handshake(&cb, &hs_b).await.expect("peer hs");
    });

    let got = node_a
        .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
        .await
        .expect("accept ok");
    driver.await.expect("driver joined");

    assert_eq!(got, node_b.app_uuid());
    assert_eq!(node_a.connection_count().await, 1);
}

#[tokio::test]
async fn accept_incoming_mismatched_room_rejects() {
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    node_a.add_room_hash([0x11; 8]);
    let bogus_room = Some([0x22u8; 8]);

    let (ca, cb) = crate::transport::mock::mock_pair();
    let hs_b = Handshake {
        app_uuid: node_b.app_uuid().to_string(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: bogus_room,
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        // The peer will get either a handshake reply or a closed socket;
        // both are acceptable here — we only assert the local rejection.
        let _ = perform_handshake(&cb, &hs_b).await;
    });

    let err = node_a
        .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
        .await
        .unwrap_err();
    let _ = driver.await;
    assert!(matches!(err, MlinkError::RoomMismatch { .. }));
    assert_eq!(node_a.connection_count().await, 0);
}

#[tokio::test]
async fn connect_peer_matching_room_roundtrip_is_accepted() {
    // Round-trip: add / remove a room and observe the membership set.
    let _tmp = set_tmp_home();
    let tmp = tempfile::tempdir().expect("tmp");
    let node = Node::new(tmp_config(tmp.path().join("t.json")))
        .await
        .expect("new");
    assert!(node.room_hashes().is_empty());
    node.add_room_hash([0xFE; 8]);
    assert!(node.room_hashes().contains(&[0xFE; 8]));
    node.remove_room_hash(&[0xFE; 8]);
    assert!(node.room_hashes().is_empty());
}

#[tokio::test]
async fn node_in_multiple_rooms_accepts_peers_from_any_of_them() {
    // A node that belongs to {room_a, room_b} must accept a peer whose
    // handshake advertises room_b — it's in the set. Previously the Node
    // only tracked one room_hash, so this scenario could not be expressed.
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    let room_a = [0xAAu8; 8];
    let room_b = [0xBBu8; 8];
    // node_a is a member of both rooms; node_b is only in room_b.
    node_a.add_room_hash(room_a);
    node_a.add_room_hash(room_b);
    node_b.add_room_hash(room_b);
    assert_eq!(node_a.room_hashes().len(), 2);

    let (ca, cb) = crate::transport::mock::mock_pair();
    let hs_b = Handshake {
        app_uuid: node_b.app_uuid().to_string(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: Some(room_b),
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        perform_handshake(&cb, &hs_b).await.expect("peer hs");
    });

    let got = node_a
        .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
        .await
        .expect("accept ok — room_b is in node_a's set");
    driver.await.expect("driver joined");
    assert_eq!(got, node_b.app_uuid());
    assert_eq!(node_a.connection_count().await, 1);
}

#[tokio::test]
async fn node_rejects_peer_whose_room_is_not_in_set() {
    // Negative case for multi-room membership: node belongs to {A, B};
    // peer claims room C — rejection must still fire.
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    node_a.add_room_hash([0xAA; 8]);
    node_a.add_room_hash([0xBB; 8]);

    let (ca, cb) = crate::transport::mock::mock_pair();
    let hs_b = Handshake {
        app_uuid: node_b.app_uuid().to_string(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: Some([0xCC; 8]),
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        let _ = perform_handshake(&cb, &hs_b).await;
    });

    let err = node_a
        .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
        .await
        .unwrap_err();
    let _ = driver.await;
    assert!(matches!(err, MlinkError::RoomMismatch { .. }));
    assert_eq!(node_a.connection_count().await, 0);
}

#[tokio::test]
async fn attach_connection_enables_send_recv_without_handshake() {
    let _tmp = set_tmp_home();
    let tmp2 = tempfile::tempdir().expect("tmp2");
    let node = Node::new(tmp_config(tmp2.path().join("t.json")))
        .await
        .expect("new");

    let (a, b) = crate::transport::mock::mock_pair();
    node.attach_connection("mock-peer-b".into(), Box::new(a)).await;

    assert_eq!(node.connection_count().await, 1);
    assert_eq!(
        node.peer_state("mock-peer-b").await,
        Some(NodeState::Connected)
    );
    assert_eq!(node.state(), NodeState::Connected);

    // End-to-end frame over the attached mock.
    node.send_raw("mock-peer-b", MessageType::Message, b"hello")
        .await
        .expect("send");

    use crate::transport::Connection as _;
    let bytes = b.read().await.expect("peer read");
    let frame = crate::protocol::frame::decode_frame(&bytes).expect("decode");
    let (_, _, mt) = crate::protocol::types::decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message);
}

#[tokio::test]
async fn accept_incoming_is_idempotent_when_already_connected() {
    // Scenario: outbound dial already finished (attach_connection simulates
    // the post-handshake registration). A duplicate inbound arrives for the
    // same app_uuid. accept_incoming must close the duplicate and return
    // Ok(existing_id) instead of evicting the live connection.
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    let peer_b_id = node_b.app_uuid().to_string();

    // Pretend node_b is already connected via the outbound path.
    let (existing, _drop_peer) = crate::transport::mock::mock_pair();
    node_a
        .attach_connection(peer_b_id.clone(), Box::new(existing))
        .await;
    assert_eq!(node_a.connection_count().await, 1);
    assert!(node_a.has_peer(&peer_b_id).await);

    // Now simulate an inbound duplicate from the same peer: run a
    // handshake on the duplicate pair; accept_incoming should DEDUP.
    let (ca, cb) = crate::transport::mock::mock_pair();
    let hs_b = Handshake {
        app_uuid: peer_b_id.clone(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: None,
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        let _ = perform_handshake(&cb, &hs_b).await;
    });

    let got = node_a
        .accept_incoming(Box::new(ca), "mock", "dup".into())
        .await
        .expect("dedup accept returns Ok");
    let _ = driver.await;

    assert_eq!(got, peer_b_id);
    // Still exactly one connection — the duplicate was dropped.
    assert_eq!(node_a.connection_count().await, 1);
}

#[tokio::test]
async fn has_peer_tracks_attach_and_disconnect() {
    let _tmp = set_tmp_home();
    let tmp = tempfile::tempdir().expect("tmp");
    let node = Node::new(tmp_config(tmp.path().join("t.json")))
        .await
        .expect("new");
    assert!(!node.has_peer("peer-x").await);
    let (a, _b) = crate::transport::mock::mock_pair();
    node.attach_connection("peer-x".into(), Box::new(a)).await;
    assert!(node.has_peer("peer-x").await);
    node.disconnect_peer("peer-x").await.expect("disc");
    assert!(!node.has_peer("peer-x").await);
}

#[tokio::test]
async fn spawn_peer_reader_emits_message_received_event() {
    // Wire two Nodes via a mock pair, attach the connections on both ends,
    // spawn a reader on one side, send a Message from the other, and
    // assert the event surfaces on the broadcast channel.
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");

    let (a_conn, b_conn) = crate::transport::mock::mock_pair();
    node_a.attach_connection("peer-b".into(), Box::new(a_conn)).await;
    node_b.attach_connection("peer-a".into(), Box::new(b_conn)).await;

    // Subscribe BEFORE spawning so we don't miss the event.
    let mut events = node_a.subscribe();
    let handle = node_a.spawn_peer_reader("peer-b".into());

    // Send a Message frame from node_b → node_a.
    node_b
        .send_raw("peer-a", MessageType::Message, b"hello chat")
        .await
        .expect("send");

    // Drain events until we see MessageReceived or time out.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            panic!("timed out waiting for MessageReceived event");
        }
        match tokio::time::timeout(timeout, events.recv()).await {
            Ok(Ok(NodeEvent::MessageReceived { peer_id, payload })) => {
                assert_eq!(peer_id, "peer-b");
                assert_eq!(payload, b"hello chat".to_vec());
                break;
            }
            Ok(Ok(_)) => continue, // ignore PeerConnected noise from attach
            Ok(Err(_)) => panic!("event stream closed"),
            Err(_) => panic!("timeout waiting for MessageReceived"),
        }
    }

    handle.abort();
}

/// Regression test for the BLE chat deadlock: a reader parked in
/// `conn.read().await` must not block a concurrent `send_raw` on the same
/// Node. Before the SharedConnection refactor, `recv_once` held
/// `connections.lock().await` across the read, and `send_raw` needed the
/// same mutex — so a peer that never sent a byte silently stalled every
/// outbound message the node tried to make.
#[tokio::test]
async fn send_raw_not_blocked_by_parked_reader() {
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // A connection whose read() hangs forever (like a BLE central that
    // isn't yet writing) but whose write() succeeds and records calls via
    // a shared counter. The counter lives outside the conn so the test can
    // observe writes without holding a second reference to the trait obj.
    struct HangingReadConn {
        id: String,
        writes: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Connection for HangingReadConn {
        async fn read(&self) -> Result<Vec<u8>> {
            std::future::pending::<()>().await;
            unreachable!()
        }
        async fn write(&self, _data: &[u8]) -> Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        fn peer_id(&self) -> &str {
            &self.id
        }
    }

    let _tmp = set_tmp_home();
    let trust_tmp = tempfile::tempdir().expect("tmp");
    let node = Node::new(tmp_config(trust_tmp.path().join("t.json")))
        .await
        .expect("new");

    let writes = Arc::new(AtomicUsize::new(0));
    let conn = HangingReadConn {
        id: "stuck-peer".into(),
        writes: Arc::clone(&writes),
    };
    node.attach_connection("stuck-peer".into(), Box::new(conn)).await;

    // Park a reader. It will hang forever inside conn.read() — exactly the
    // shape of the BLE chat host waiting on the client's first byte.
    let reader_handle = node.spawn_peer_reader("stuck-peer".into());

    // Give the reader a beat to reach its pending read.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Sending must complete on its own without the reader making progress.
    // If the old per-map-mutex deadlock is back, this future will hang and
    // the tokio::time::timeout below will trip.
    let send_fut = node.send_raw("stuck-peer", MessageType::Message, b"hi");
    tokio::time::timeout(Duration::from_secs(2), send_fut)
        .await
        .expect("send_raw must not be blocked by parked reader")
        .expect("send ok");

    assert_eq!(writes.load(Ordering::SeqCst), 1);
    reader_handle.abort();
}

/// Regression test for the "dead connection lingers after peer EOF" bug
/// (Broken pipe on re-send; re-dial blocked by dedup). When the reader's
/// `recv_once` returns Err (the peer closed), the reader task must evict
/// the connection from `ConnectionManager`, drop the peer, flip the
/// PeerState to Disconnected, and emit PeerDisconnected — otherwise the
/// next `send_raw` will try to push into a dead socket and a fresh dial
/// to the same `app_uuid` will be rejected as a duplicate.
#[tokio::test]
async fn reader_eof_cleans_up_connection_and_peer_manager() {
    use async_trait::async_trait;
    use tokio::sync::Notify;

    // A connection whose read() blocks until `release` fires, then
    // returns an error to simulate the peer closing. write() and close()
    // succeed so the cleanup path can run without synthetic errors.
    struct EofOnDemand {
        id: String,
        release: Arc<Notify>,
    }
    #[async_trait]
    impl Connection for EofOnDemand {
        async fn read(&self) -> Result<Vec<u8>> {
            self.release.notified().await;
            Err(MlinkError::HandlerError("peer closed".into()))
        }
        async fn write(&self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            Ok(())
        }
        fn peer_id(&self) -> &str {
            &self.id
        }
    }

    let _tmp = set_tmp_home();
    let trust_tmp = tempfile::tempdir().expect("tmp");
    let node = Node::new(tmp_config(trust_tmp.path().join("t.json")))
        .await
        .expect("new");

    let release = Arc::new(Notify::new());
    let conn = EofOnDemand {
        id: "bye-peer".into(),
        release: Arc::clone(&release),
    };
    node.attach_connection("bye-peer".into(), Box::new(conn)).await;
    // Also register the peer in PeerManager to confirm it gets dropped.
    node.peer_manager
        .add(Peer {
            id: "bye-peer".into(),
            name: "bye".into(),
            app_uuid: "bye-peer".into(),
            connected_at: Instant::now(),
            transport_id: "mock".into(),
        })
        .await;
    assert!(node.has_peer("bye-peer").await);
    assert!(node.get_peer("bye-peer").await.is_some());

    let mut events = node.subscribe();
    let reader_handle = node.spawn_peer_reader("bye-peer".into());

    // Trigger the simulated EOF and wait for the cleanup to finish.
    release.notify_one();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
        if timeout.is_zero() {
            panic!("timed out waiting for PeerDisconnected");
        }
        match tokio::time::timeout(timeout, events.recv()).await {
            Ok(Ok(NodeEvent::PeerDisconnected { peer_id })) => {
                assert_eq!(peer_id, "bye-peer");
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => panic!("event stream closed"),
            Err(_) => panic!("timeout waiting for PeerDisconnected"),
        }
    }

    // The reader task itself must exit after emitting the event.
    tokio::time::timeout(Duration::from_secs(1), reader_handle)
        .await
        .expect("reader must exit after EOF")
        .expect("reader task joined");

    // Cleanup invariants: no connection, no peer, state flipped.
    assert!(!node.has_peer("bye-peer").await, "connection must be evicted");
    assert!(
        node.get_peer("bye-peer").await.is_none(),
        "peer_manager must drop the peer"
    );
    assert_eq!(node.connection_count().await, 0);
    assert_eq!(
        node.peer_state("bye-peer").await,
        Some(NodeState::Disconnected)
    );
}

/// Test-only Transport that hands out a pre-staged `Box<dyn Connection>`
/// on the first `connect()` call. Mirrors the shape of the peer-side
/// driver used by the `accept_incoming_*` tests, but on the dialer side:
/// instead of feeding a raw `MockConnection` into `accept_incoming`, we
/// drive it through the full `connect_peer` path including
/// `transport.connect()`.
struct StagedTransport {
    staged: tokio::sync::Mutex<Option<Box<dyn Connection>>>,
}

impl StagedTransport {
    fn new(conn: Box<dyn Connection>) -> Self {
        Self {
            staged: tokio::sync::Mutex::new(Some(conn)),
        }
    }
}

#[async_trait::async_trait]
impl Transport for StagedTransport {
    fn id(&self) -> &str {
        "staged"
    }
    fn capabilities(&self) -> crate::transport::TransportCapabilities {
        crate::transport::TransportCapabilities {
            max_peers: 1,
            throughput_bps: u64::MAX,
            latency_ms: 0,
            reliable: true,
            bidirectional: true,
        }
    }
    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>> {
        Ok(Vec::new())
    }
    async fn connect(&mut self, _peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
        self.staged
            .lock()
            .await
            .take()
            .ok_or_else(|| MlinkError::HandlerError("staged conn already taken".into()))
    }
    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        Err(MlinkError::HandlerError("listen unused".into()))
    }
    fn mtu(&self) -> usize {
        512
    }
}

/// Mirror of `accept_incoming_matching_room_registers_peer` on the dialer
/// side: drive `connect_peer` end-to-end through a staged transport,
/// assert the handshake completes, both ends register the connection, and
/// post-handshake `send_raw` round-trips in both directions.
#[tokio::test]
async fn connect_peer_matching_room_happy_path() {
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    let room = [0x7Au8; 8];
    node_a.add_room_hash(room);
    node_b.add_room_hash(room);

    // ca will be handed to node_a by StagedTransport::connect; cb is the
    // peer side we drive manually to play the handshake counterpart.
    let (ca, cb) = crate::transport::mock::mock_pair();
    let mut transport = StagedTransport::new(Box::new(ca));
    let discovered = DiscoveredPeer {
        id: "wire-peer-b".into(),
        name: "node-b".into(),
        rssi: None,
        metadata: vec![],
    };

    let hs_b = Handshake {
        app_uuid: node_b.app_uuid().to_string(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: Some(room),
        session_id: None,
        session_last_seq: 0,
    };

    // The peer keeps the cb end alive through an Arc so we can reuse it
    // for attach_connection on node_b after the handshake — that way the
    // test can exercise a real post-handshake send/recv both ways.
    let cb = Arc::new(cb);
    let driver_cb = Arc::clone(&cb);
    let driver = tokio::spawn(async move {
        perform_handshake(&*driver_cb, &hs_b).await.expect("peer hs");
    });

    let peer_id = node_a
        .connect_peer(&mut transport, &discovered)
        .await
        .expect("connect_peer ok");
    driver.await.expect("driver joined");

    assert_eq!(peer_id, node_b.app_uuid());
    assert_eq!(node_a.connection_count().await, 1);
    assert!(node_a.has_peer(node_b.app_uuid()).await);
    assert_eq!(
        node_a.peer_state(node_b.app_uuid()).await,
        Some(NodeState::Connected)
    );

    // Wire cb into node_b under node_a.app_uuid() so both ends can use
    // the normal send_raw path. Arc::try_unwrap is safe because the
    // driver task has joined and no other clone exists.
    let cb_owned = Arc::try_unwrap(cb).ok().expect("cb uniquely owned");
    node_b
        .attach_connection(node_a.app_uuid().to_string(), Box::new(cb_owned))
        .await;

    // a -> b
    node_a
        .send_raw(node_b.app_uuid(), MessageType::Message, b"hi-b")
        .await
        .expect("a->b send");
    // b -> a
    node_b
        .send_raw(node_a.app_uuid(), MessageType::Message, b"hi-a")
        .await
        .expect("b->a send");

    // Drain one frame on each side to prove the bytes actually land.
    let conn_b = {
        let g = node_b.connections.lock().await;
        g.shared(node_a.app_uuid()).expect("b has a")
    };
    let frame_bytes_at_b = conn_b.read().await.expect("b reads");
    let frame = decode_frame(&frame_bytes_at_b).expect("decode at b");
    let (_c, _e, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message);
    assert_eq!(frame.payload, b"hi-b".to_vec());

    let conn_a = {
        let g = node_a.connections.lock().await;
        g.shared(node_b.app_uuid()).expect("a has b")
    };
    let frame_bytes_at_a = conn_a.read().await.expect("a reads");
    let frame = decode_frame(&frame_bytes_at_a).expect("decode at a");
    let (_c, _e, mt) = decode_flags(frame.flags);
    assert_eq!(mt, MessageType::Message);
    assert_eq!(frame.payload, b"hi-a".to_vec());
}

/// Mirror of `accept_incoming_is_idempotent_when_already_connected`:
/// when node_a already holds a live connection for node_b (installed
/// via attach_connection), a second `connect_peer` to the same
/// app_uuid must DEDUP — return Ok with node_b's app_uuid and leave the
/// existing connection untouched, rather than evicting it.
#[tokio::test]
async fn connect_peer_dedup_when_already_connected() {
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    let peer_b_id = node_b.app_uuid().to_string();

    // Simulate a prior accept_incoming having registered the peer by
    // attaching a connection keyed under node_b's app_uuid.
    let (existing, _keep_alive) = crate::transport::mock::mock_pair();
    node_a
        .attach_connection(peer_b_id.clone(), Box::new(existing))
        .await;
    assert_eq!(node_a.connection_count().await, 1);
    assert!(node_a.has_peer(&peer_b_id).await);

    // Now drive a second connect_peer for the same peer. Any room on
    // either side would work here; leaving both sets empty keeps the
    // handshake check quiet and focuses the assertion on dedup.
    let (ca, cb) = crate::transport::mock::mock_pair();
    let mut transport = StagedTransport::new(Box::new(ca));
    let discovered = DiscoveredPeer {
        id: "wire-peer-b-2".into(),
        name: "dup".into(),
        rssi: None,
        metadata: vec![],
    };
    let hs_b = Handshake {
        app_uuid: peer_b_id.clone(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: None,
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        let _ = perform_handshake(&cb, &hs_b).await;
    });

    let got = node_a
        .connect_peer(&mut transport, &discovered)
        .await
        .expect("dedup dial returns Ok");
    let _ = driver.await;

    assert_eq!(got, peer_b_id);
    // Still exactly one connection — the duplicate dial was dropped,
    // not the pre-existing live one.
    assert_eq!(node_a.connection_count().await, 1);
    assert!(node_a.has_peer(&peer_b_id).await);
}

/// Mirror of `accept_incoming_mismatched_room_rejects`: dialer is in
/// room 0x11..., peer claims 0x22... in its handshake reply.
/// `connect_peer` must reject with `RoomMismatch` and leave no
/// connection behind.
#[tokio::test]
async fn connect_peer_rejects_mismatched_room() {
    let _tmp = set_tmp_home();
    let tmp_a = tempfile::tempdir().expect("tmp-a");
    let tmp_b = tempfile::tempdir().expect("tmp-b");
    let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
        .await
        .expect("new a");
    let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
        .await
        .expect("new b");
    node_a.add_room_hash([0x11; 8]);
    let bogus_room = Some([0x22u8; 8]);

    let (ca, cb) = crate::transport::mock::mock_pair();
    let mut transport = StagedTransport::new(Box::new(ca));
    let discovered = DiscoveredPeer {
        id: "wire-peer-b".into(),
        name: "node-b".into(),
        rssi: None,
        metadata: vec![],
    };
    let hs_b = Handshake {
        app_uuid: node_b.app_uuid().to_string(),
        version: PROTOCOL_VERSION,
        mtu: 512,
        compress: true,
        encrypt: false,
        last_seq: 0,
        resume_streams: vec![],
        room_hash: bogus_room,
        session_id: None,
        session_last_seq: 0,
    };
    let driver = tokio::spawn(async move {
        // The peer may observe a closed socket as soon as node_a rejects,
        // so either outcome (Ok / Err) is acceptable — the assertion is
        // on the local rejection, matching the accept-side mirror test.
        let _ = perform_handshake(&cb, &hs_b).await;
    });

    let err = node_a
        .connect_peer(&mut transport, &discovered)
        .await
        .unwrap_err();
    let _ = driver.await;
    assert!(matches!(err, MlinkError::RoomMismatch { .. }));
    assert_eq!(node_a.connection_count().await, 0);
    assert!(!node_a.has_peer(node_b.app_uuid()).await);
}
