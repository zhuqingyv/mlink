use std::sync::Mutex;
use std::time::Duration;

use mlink_core::core::node::{Node, NodeConfig, NodeState};
use mlink_core::core::reconnect::{
    ReconnectManager, ReconnectMode, ReconnectPolicy, StreamProgress,
};
use mlink_core::protocol::errors::MlinkError;
use mlink_core::protocol::types::MessageType;
use mlink_core::transport::mock::MockTransport;
use mlink_core::transport::DiscoveredPeer;

// ============================================================================
// HOME env serialization: tests that mutate HOME must not race with peer.rs
// unit tests or node.rs unit tests that do the same.
// ============================================================================

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
    let tmp = tempfile::tempdir().expect("tmp home");
    let prev = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());
    HomeGuard {
        _tmp: tmp,
        prev,
        _guard: guard,
    }
}

fn tmp_trust_config() -> (NodeConfig, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tmp trust");
    let cfg = NodeConfig {
        name: "phase5-test".into(),
        encrypt: false,
        trust_store_path: Some(dir.path().join("trust.json")),
    };
    (cfg, dir)
}

// ============================================================================
// 1. reconnect::ReconnectPolicy
// ============================================================================

// delay sequence for foreground should be: 0, 1s, 2s, 4s, 8s, 16s, 32s, 60s, 60s, ...
// (attempt increments AFTER each call, so the Nth call uses attempt=N-1)
#[test]
fn test_exponential_backoff() {
    let mut p = ReconnectPolicy::new();
    let d0 = p.next_delay().expect("fg");
    assert_eq!(d0, Duration::ZERO, "first attempt should be immediate");

    let d1 = p.next_delay().expect("fg");
    assert_eq!(d1, Duration::from_secs(1), "2nd attempt = 1s");

    let d2 = p.next_delay().expect("fg");
    assert_eq!(d2, Duration::from_secs(2), "3rd attempt = 2s");

    let d3 = p.next_delay().expect("fg");
    assert_eq!(d3, Duration::from_secs(4), "4th attempt = 4s");

    let d4 = p.next_delay().expect("fg");
    assert_eq!(d4, Duration::from_secs(8), "5th attempt = 8s");
}

#[test]
fn test_backoff_cap_60s() {
    let mut p = ReconnectPolicy::new();
    // drain many attempts; none may exceed 60s in foreground mode
    for _ in 0..20 {
        if let Some(d) = p.next_delay() {
            assert!(
                d <= Duration::from_secs(60),
                "foreground delay exceeded 60s cap: {:?}",
                d
            );
        }
    }
    // at least one attempt should have saturated the cap
    let capped = p.next_delay().unwrap_or(Duration::ZERO);
    assert_eq!(capped, Duration::from_secs(60), "should cap at 60s");
}

// The foreground→background transition triggers at 5 minutes of wall-clock.
// We can't fake Instant::now() without modifying the implementation, so this
// test is ignored by default. Run manually with:
//   cargo test --test phase5_tests -- --ignored test_foreground_to_background
#[test]
#[ignore = "requires 5+ minutes of wall-clock time"]
fn test_foreground_to_background() {
    let mut p = ReconnectPolicy::new();
    assert_eq!(p.mode(), ReconnectMode::Foreground);
    // simulate time passing by sleeping
    std::thread::sleep(Duration::from_secs(5 * 60 + 1));
    let _ = p.next_delay();
    assert_eq!(p.mode(), ReconnectMode::Background);
}

// Background-gives-up also requires wall-clock progression (5 + 30 minutes).
#[test]
#[ignore = "requires 35+ minutes of wall-clock time"]
fn test_background_gives_up() {
    let mut p = ReconnectPolicy::new();
    std::thread::sleep(Duration::from_secs(5 * 60 + 1));
    let _ = p.next_delay(); // triggers switch to Background
    assert_eq!(p.mode(), ReconnectMode::Background);
    std::thread::sleep(Duration::from_secs(30 * 60 + 1));
    let d = p.next_delay();
    assert!(d.is_none(), "background should give up after 30 min");
}

#[test]
fn test_reset() {
    let mut p = ReconnectPolicy::new();
    for _ in 0..5 {
        p.next_delay();
    }
    assert!(p.attempt() > 0, "attempt should have incremented");

    p.reset();
    assert_eq!(p.attempt(), 0, "reset should zero attempt");
    assert_eq!(p.mode(), ReconnectMode::Foreground, "reset to foreground");

    let d = p.next_delay().expect("post-reset delay");
    assert_eq!(d, Duration::ZERO, "first attempt after reset = 0");
}

#[test]
fn test_stream_progress() {
    let mut m = ReconnectManager::new();
    let p1 = StreamProgress {
        stream_id: 1,
        received_bitmap: vec![0xff, 0x0f],
        total_chunks: 12,
    };
    let p2 = StreamProgress {
        stream_id: 2,
        received_bitmap: vec![0xaa],
        total_chunks: 8,
    };
    m.record_stream_progress(p1);
    m.record_stream_progress(p2);

    let mut taken = m.take_resume_streams();
    taken.sort_by_key(|s| s.stream_id);
    assert_eq!(taken.len(), 2);
    assert_eq!(taken[0].stream_id, 1);
    assert_eq!(taken[0].received_bitmap, vec![0xff, 0x0f]);
    assert_eq!(taken[0].total_chunks, 12);
    assert_eq!(taken[1].stream_id, 2);
    assert_eq!(taken[1].received_bitmap, vec![0xaa]);
    assert_eq!(taken[1].total_chunks, 8);

    // after take_resume_streams, the manager should be empty
    let again = m.take_resume_streams();
    assert!(again.is_empty(), "take should drain the manager");
}

// ============================================================================
// 2. node::Node lifecycle (via MockTransport)
// ============================================================================

#[tokio::test]
async fn test_node_initial_state() {
    let _home = set_tmp_home();
    let (cfg, _dir) = tmp_trust_config();
    let node = Node::new(cfg).await.expect("node new");
    assert_eq!(node.state(), NodeState::Idle, "new node must start Idle");
    assert!(!node.app_uuid().is_empty(), "app_uuid must be populated");
    assert_eq!(node.connection_count().await, 0);
}

#[tokio::test]
async fn test_node_start_discovering() {
    let _home = set_tmp_home();
    let (cfg, _dir) = tmp_trust_config();
    let node = Node::new(cfg).await.expect("node new");
    node.start().await.expect("start");
    assert_eq!(node.state(), NodeState::Discovering);

    // calling start() twice should reject
    let err = node.start().await.unwrap_err();
    assert!(matches!(err, MlinkError::HandlerError(_)));
}

// A complete discover→connect→connected flow requires a transport whose
// connect() returns a working Connection that also speaks the handshake.
// The real handshake codec is exercised in phase4_tests.rs; here we verify
// that connect_peer against the unimplemented MockTransport::connect fails
// cleanly and leaves the node in a sane state (no peers, no connections,
// state not stuck in Connecting). This is the state-machine contract.
#[tokio::test]
async fn test_node_state_transitions() {
    let _home = set_tmp_home();
    let (cfg, _dir) = tmp_trust_config();
    let node = Node::new(cfg).await.expect("node new");

    // Idle → Discovering
    assert_eq!(node.state(), NodeState::Idle);
    node.start().await.expect("start");
    assert_eq!(node.state(), NodeState::Discovering);

    // Discovering → (attempt connect) → connect fails → state reverts cleanly
    let mut transport = MockTransport::new();
    let discovered = DiscoveredPeer {
        id: "peer-a".into(),
        name: "a".into(),
        rssi: None,
        metadata: vec![],
    };
    let err = node
        .connect_peer(&mut transport, &discovered)
        .await
        .unwrap_err();
    assert!(matches!(err, MlinkError::HandlerError(_)));

    // no peers registered, no connections held
    assert_eq!(node.peers().await.len(), 0);
    assert_eq!(node.connection_count().await, 0);

    // stop() returns to Idle
    node.stop().await.expect("stop");
    assert_eq!(node.state(), NodeState::Idle);
}

#[tokio::test]
async fn test_node_stop() {
    let _home = set_tmp_home();
    let (cfg, _dir) = tmp_trust_config();
    let node = Node::new(cfg).await.expect("node new");
    node.start().await.expect("start");
    node.stop().await.expect("stop");
    assert_eq!(node.state(), NodeState::Idle);

    // stop is idempotent (calling again stays Idle)
    node.stop().await.expect("stop-again");
    assert_eq!(node.state(), NodeState::Idle);
}

#[tokio::test]
async fn test_node_config() {
    let _home = set_tmp_home();
    let dir = tempfile::tempdir().expect("tmp");
    let trust_path = dir.path().join("my-trust.json");
    let cfg = NodeConfig {
        name: "alice".into(),
        encrypt: false,
        trust_store_path: Some(trust_path.clone()),
    };
    let node = Node::new(cfg).await.expect("node new");
    assert_eq!(node.config().name, "alice");
    assert!(!node.config().encrypt);
    assert_eq!(node.config().trust_store_path.as_ref(), Some(&trust_path));

    // NodeConfig::default() must have encrypt=false — without an aes_key
    // there is nothing to encrypt with, so claiming encrypt=true on the wire
    // would be a lie (the original bug this was pinning in place).
    let d = NodeConfig::default();
    assert!(!d.encrypt);
    assert!(d.trust_store_path.is_none());
    assert!(d.name.is_empty());
}

// Also verify send_raw on a missing peer produces PeerGone (state-machine contract).
#[tokio::test]
async fn test_send_raw_missing_peer() {
    let _home = set_tmp_home();
    let (cfg, _dir) = tmp_trust_config();
    let node = Node::new(cfg).await.expect("node new");
    let err = node
        .send_raw("never-connected", MessageType::Message, b"hi")
        .await
        .unwrap_err();
    assert!(matches!(err, MlinkError::PeerGone { .. }));
}
