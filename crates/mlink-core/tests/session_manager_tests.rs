use std::sync::Arc;

use mlink_core::core::link::{Link, TransportKind};
use mlink_core::core::session::{AttachOutcome, SessionManager};
use mlink_core::core::session::types::LinkRole;
use mlink_core::transport::mock::mock_pair;
use mlink_core::transport::{Connection, TransportCapabilities};

fn caps(throughput: u64) -> TransportCapabilities {
    TransportCapabilities {
        max_peers: 8,
        throughput_bps: throughput,
        latency_ms: 5,
        reliable: true,
        bidirectional: true,
    }
}

fn new_link(id: &str, kind: TransportKind) -> Link {
    let (a, _b) = mock_pair();
    let conn: Arc<dyn Connection> = Arc::new(a);
    Link::new(id.to_string(), conn, kind, caps(1_000_000))
}

#[tokio::test]
async fn attach_first_link_creates_new_session() {
    let mgr = SessionManager::new();
    let outcome = mgr
        .attach_link("peer-1", new_link("tcp:p1:0", TransportKind::Tcp), None)
        .await;
    match outcome {
        AttachOutcome::CreatedNew { session_id } => assert_ne!(session_id, [0u8; 16]),
        other => panic!("expected CreatedNew, got {:?}", other),
    }
    assert!(mgr.contains("peer-1").await);
    assert_eq!(mgr.count().await, 1);
    let status = mgr.peer_link_status("peer-1").await;
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].role, LinkRole::Active);
    assert_eq!(status[0].kind, TransportKind::Tcp);
}

#[tokio::test]
async fn attach_second_link_with_matching_session_id() {
    let mgr = SessionManager::new();
    let AttachOutcome::CreatedNew { session_id } = mgr
        .attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await
    else {
        panic!("expected CreatedNew");
    };
    let outcome = mgr
        .attach_link("p", new_link("ble:p:1", TransportKind::Ble), Some(session_id))
        .await;
    assert_eq!(outcome, AttachOutcome::AttachedExisting);
    assert_eq!(mgr.peer_link_status("p").await.len(), 2);
}

#[tokio::test]
async fn attach_rejects_duplicate_transport_kind() {
    let mgr = SessionManager::new();
    let AttachOutcome::CreatedNew { session_id } = mgr
        .attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await
    else {
        panic!();
    };
    let outcome = mgr
        .attach_link("p", new_link("tcp:p:1", TransportKind::Tcp), Some(session_id))
        .await;
    assert_eq!(outcome, AttachOutcome::RejectedDuplicateKind);
    assert_eq!(mgr.peer_link_status("p").await.len(), 1);
}

#[tokio::test]
async fn attach_rejects_wrong_session_id_hint() {
    let mgr = SessionManager::new();
    mgr.attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await;
    let outcome = mgr
        .attach_link("p", new_link("ble:p:1", TransportKind::Ble), Some([0u8; 16]))
        .await;
    assert_eq!(outcome, AttachOutcome::RejectedSessionMismatch);
    assert_eq!(mgr.peer_link_status("p").await.len(), 1);
}

#[tokio::test]
async fn attach_first_link_rejects_foreign_session_id() {
    let mgr = SessionManager::new();
    let outcome = mgr
        .attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), Some([9u8; 16]))
        .await;
    assert_eq!(outcome, AttachOutcome::RejectedSessionMismatch);
    assert!(!mgr.contains("p").await);
}

#[tokio::test]
async fn attach_accepts_up_to_max_distinct_kinds() {
    // MAX_LINKS_PER_PEER=4 equals the count of TransportKind variants, so
    // the duplicate-kind guard is what enforces the cap in practice.
    let mgr = SessionManager::new();
    let AttachOutcome::CreatedNew { session_id } = mgr
        .attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await
    else {
        panic!();
    };
    for (i, kind) in [TransportKind::Ble, TransportKind::Ipc, TransportKind::Mock]
        .into_iter()
        .enumerate()
    {
        let outcome = mgr
            .attach_link("p", new_link(&format!("x:{}", i), kind), Some(session_id))
            .await;
        assert_eq!(outcome, AttachOutcome::AttachedExisting, "iter {}", i);
    }
    assert_eq!(mgr.peer_link_status("p").await.len(), 4);
    // at max capacity, any further attach (even dup kind) is rejected as capacity first.
    let outcome = mgr
        .attach_link("p", new_link("x:dup", TransportKind::Mock), Some(session_id))
        .await;
    assert_eq!(outcome, AttachOutcome::RejectedCapacity);
    assert_eq!(mgr.peer_link_status("p").await.len(), 4);
}

#[tokio::test]
async fn detach_link_removes_link_and_drops_empty_peer() {
    let mgr = SessionManager::new();
    let AttachOutcome::CreatedNew { session_id } = mgr
        .attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await
    else {
        panic!();
    };
    mgr.attach_link("p", new_link("ble:p:1", TransportKind::Ble), Some(session_id))
        .await;
    assert!(!mgr.detach_link("p", "tcp:p:0").await);
    let remaining = mgr.peer_link_status("p").await;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].link_id, "ble:p:1");
    assert!(mgr.detach_link("p", "ble:p:1").await);
    assert!(!mgr.contains("p").await);
}

#[tokio::test]
async fn detach_unknown_link_returns_false() {
    let mgr = SessionManager::new();
    mgr.attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await;
    assert!(!mgr.detach_link("p", "does-not-exist").await);
    assert!(!mgr.detach_link("no-peer", "any").await);
}

#[tokio::test]
async fn detach_active_link_downgrades_role_of_remaining() {
    let mgr = SessionManager::new();
    let AttachOutcome::CreatedNew { session_id } = mgr
        .attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await
    else {
        panic!();
    };
    mgr.attach_link("p", new_link("ble:p:1", TransportKind::Ble), Some(session_id))
        .await;
    let status = mgr.peer_link_status("p").await;
    let active_count = status.iter().filter(|s| s.role == LinkRole::Active).count();
    assert_eq!(active_count, 1, "one active before detach");
    // detach the active (tcp) link
    assert!(!mgr.detach_link("p", "tcp:p:0").await);
    let status = mgr.peer_link_status("p").await;
    assert_eq!(status.len(), 1);
    // active slot was cleared — remaining link is Standby until io layer picks a new one
    assert_eq!(status[0].role, LinkRole::Standby);
}

#[tokio::test]
async fn drop_peer_removes_session() {
    let mgr = SessionManager::new();
    mgr.attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None)
        .await;
    assert!(mgr.drop_peer("p").await.is_some());
    assert!(!mgr.contains("p").await);
    assert!(mgr.drop_peer("p").await.is_none());
}

#[tokio::test]
async fn peers_and_list_status_snapshot() {
    let mgr = SessionManager::new();
    mgr.attach_link("a", new_link("tcp:a:0", TransportKind::Tcp), None).await;
    mgr.attach_link("b", new_link("tcp:b:0", TransportKind::Tcp), None).await;
    let mut peers = mgr.peers().await;
    peers.sort();
    assert_eq!(peers, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(mgr.list_status().await.len(), 2);
}

#[tokio::test]
async fn count_reflects_create_and_drop() {
    let mgr = SessionManager::new();
    assert_eq!(mgr.count().await, 0);
    mgr.attach_link("p", new_link("tcp:p:0", TransportKind::Tcp), None).await;
    assert_eq!(mgr.count().await, 1);
    mgr.drop_peer("p").await;
    assert_eq!(mgr.count().await, 0);
}
