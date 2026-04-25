use std::sync::Arc;

use mlink_core::core::link::{Link, TransportKind, LINK_UNHEALTHY_ERR_MILLI};
use mlink_core::transport::mock::mock_pair;
use mlink_core::transport::{Connection, TransportCapabilities};

fn caps(throughput: u64) -> TransportCapabilities {
    TransportCapabilities {
        max_peers: 8,
        throughput_bps: throughput,
        latency_ms: 1,
        reliable: true,
        bidirectional: true,
    }
}

fn make_link(kind: TransportKind, throughput: u64) -> Link {
    let (a, _b) = mock_pair();
    let conn: Arc<dyn Connection> = Arc::new(a);
    Link::new(
        format!("{}:test:1", kind.as_wire()),
        conn,
        kind,
        caps(throughput),
    )
}

#[test]
fn transport_kind_wire_roundtrip() {
    for k in [
        TransportKind::Ble,
        TransportKind::Tcp,
        TransportKind::Ipc,
        TransportKind::Mock,
    ] {
        assert_eq!(TransportKind::from_wire(k.as_wire()), Some(k));
    }
    assert_eq!(TransportKind::from_wire("zigbee"), None);
}

#[test]
fn initial_state_is_healthy() {
    let l = make_link(TransportKind::Tcp, 1_000_000);
    assert!(l.is_healthy());
    assert_eq!(l.health().inflight, 0);
}

#[test]
fn score_rewards_throughput_penalises_rtt_and_errors() {
    let fast = make_link(TransportKind::Tcp, 1_000_000);
    let slow = make_link(TransportKind::Ble, 100_000);
    assert!(fast.score() > slow.score());

    let high_rtt = make_link(TransportKind::Tcp, 1_000_000);
    high_rtt.record_rtt(1_000_000);
    let low_rtt = make_link(TransportKind::Tcp, 1_000_000);
    low_rtt.record_rtt(1_000);
    assert!(low_rtt.score() > high_rtt.score());

    let erroring = make_link(TransportKind::Tcp, 1_000_000);
    for _ in 0..64 {
        erroring.record_error();
    }
    let clean = make_link(TransportKind::Tcp, 1_000_000);
    assert!(clean.score() > erroring.score());
}

#[test]
fn is_healthy_falls_after_sustained_errors() {
    let l = make_link(TransportKind::Tcp, 1_000_000);
    for _ in 0..64 {
        l.record_error();
    }
    assert!(l.health().err_rate_milli >= LINK_UNHEALTHY_ERR_MILLI);
    assert!(!l.is_healthy());
}

#[test]
fn record_success_recovers_health() {
    let l = make_link(TransportKind::Tcp, 1_000_000);
    for _ in 0..64 {
        l.record_error();
    }
    assert!(!l.is_healthy());
    for _ in 0..200 {
        l.record_success();
    }
    assert!(l.health().err_rate_milli < LINK_UNHEALTHY_ERR_MILLI);
    assert!(l.is_healthy());
}

#[test]
fn record_rtt_updates_ema_and_last_ok() {
    let l = make_link(TransportKind::Tcp, 1_000_000);
    l.record_rtt(800);
    assert_eq!(l.health().rtt_ema_us, 800);
    l.record_rtt(1600);
    assert_eq!(l.health().rtt_ema_us, 900);
}

#[test]
fn inflight_counters_are_saturating() {
    let l = make_link(TransportKind::Tcp, 1_000_000);
    l.dec_inflight();
    assert_eq!(l.health().inflight, 0);
    l.inc_inflight();
    l.inc_inflight();
    assert_eq!(l.health().inflight, 2);
    l.dec_inflight();
    assert_eq!(l.health().inflight, 1);
}

#[tokio::test]
async fn send_and_recv_keep_link_healthy() {
    let (a, b) = mock_pair();
    let a_conn: Arc<dyn Connection> = Arc::new(a);
    let b_conn: Arc<dyn Connection> = Arc::new(b);
    let la = Link::new(
        "tcp:a:1".into(),
        a_conn,
        TransportKind::Tcp,
        caps(1_000_000),
    );
    let lb = Link::new(
        "tcp:b:1".into(),
        b_conn,
        TransportKind::Tcp,
        caps(1_000_000),
    );

    la.send(b"ping").await.unwrap();
    let got = lb.recv().await.unwrap();
    assert_eq!(got, b"ping");
    assert!(la.is_healthy());
    assert!(lb.is_healthy());
}

#[tokio::test]
async fn send_after_close_records_error() {
    let (a, _b) = mock_pair();
    let conn: Arc<dyn Connection> = Arc::new(a);
    let l = Link::new(
        "tcp:x:1".into(),
        conn,
        TransportKind::Tcp,
        caps(1_000_000),
    );
    l.close().await;
    let err = l.send(b"data").await;
    assert!(err.is_err());
    assert!(l.health().err_rate_milli > 0);
}
