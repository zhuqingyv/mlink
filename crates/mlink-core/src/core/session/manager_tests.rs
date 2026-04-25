use super::*;
use crate::core::link::TransportKind;
use crate::transport::mock::mock_pair;
use crate::transport::{Connection, TransportCapabilities};

fn caps() -> TransportCapabilities {
    TransportCapabilities {
        max_peers: 8,
        throughput_bps: 1,
        latency_ms: 1,
        reliable: true,
        bidirectional: true,
    }
}

fn make_link(id: &str, kind: TransportKind) -> Link {
    let (a, _b) = mock_pair();
    let conn: Arc<dyn Connection> = Arc::new(a);
    Link::new(id.into(), conn, kind, caps())
}

/// Confirms MAX_LINKS_PER_PEER enforces even when all four TransportKinds are
/// already distinct — we preload Mock duplicates via Session.links to bypass
/// the kind-uniqueness guard and reach the capacity branch.
#[tokio::test]
async fn rejects_capacity_when_preloaded_to_max() {
    let mgr = SessionManager::new();
    let AttachOutcome::CreatedNew { session_id } = mgr
        .attach_link("p", make_link("l0", TransportKind::Tcp), None)
        .await
    else {
        panic!();
    };
    let session = mgr.get("p").await.unwrap();
    {
        let mut links = session.links.write().await;
        for i in 0..3 {
            let (a, _b) = mock_pair();
            let conn: Arc<dyn Connection> = Arc::new(a);
            links.push(Arc::new(Link::new(
                format!("extra-{i}"),
                conn,
                TransportKind::Mock,
                caps(),
            )));
        }
        assert_eq!(links.len(), MAX_LINKS_PER_PEER);
    }
    let outcome = mgr
        .attach_link("p", make_link("l1", TransportKind::Ble), Some(session_id))
        .await;
    assert_eq!(outcome, AttachOutcome::RejectedCapacity);
}
