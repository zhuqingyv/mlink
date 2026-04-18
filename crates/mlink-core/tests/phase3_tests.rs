use std::time::Duration;

use mlink_core::transport::ble::{
    BleTransport, CTRL_CHAR_UUID, MLINK_SERVICE_UUID, RX_CHAR_UUID, TX_CHAR_UUID,
};
use mlink_core::transport::ipc::IpcTransport;
use mlink_core::transport::mock::{mock_pair, MockTransport};
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};

// ============================================================================
// mock transport
// ============================================================================

#[tokio::test]
async fn test_mock_pair_bidirectional() {
    let (mut a, mut b) = mock_pair();

    a.write(b"a->b").await.expect("a write");
    let got_b = b.read().await.expect("b read");
    assert_eq!(got_b, b"a->b");

    b.write(b"b->a").await.expect("b write");
    let got_a = a.read().await.expect("a read");
    assert_eq!(got_a, b"b->a");

    assert_ne!(a.peer_id(), b.peer_id());
}

#[tokio::test]
async fn test_mock_pair_large_message() {
    let (mut a, mut b) = mock_pair();

    let payload: Vec<u8> = (0..10_240).map(|i| (i & 0xFF) as u8).collect();
    a.write(&payload).await.expect("large write");
    let got = b.read().await.expect("large read");
    assert_eq!(got.len(), 10_240);
    assert_eq!(got, payload);
}

#[tokio::test]
async fn test_mock_capabilities() {
    let t = MockTransport::new();
    assert_eq!(t.id(), "mock");
    assert!(t.mtu() > 0, "mtu should be positive, got {}", t.mtu());

    let caps = t.capabilities();
    assert!(caps.max_peers > 0, "max_peers should be positive");
    assert!(caps.reliable, "mock should be reliable");
    assert!(caps.bidirectional, "mock should be bidirectional");
}

// ============================================================================
// ble transport
// ============================================================================

#[test]
fn test_ble_transport_new() {
    let t = BleTransport::new();
    assert_eq!(t.id(), "ble");
    let _default = BleTransport::default();
    let _configured = BleTransport::new().with_scan_duration(Duration::from_secs(1));
}

#[test]
fn test_ble_capabilities() {
    let t = BleTransport::new();
    let caps = t.capabilities();
    assert_eq!(
        caps.throughput_bps, 1_200_000,
        "ARCHITECTURE.md requires 1.2 Mbps (1_200_000 bps)"
    );
    assert_eq!(
        caps.latency_ms, 15,
        "ARCHITECTURE.md requires 15ms typical latency"
    );
    assert!(caps.reliable);
    assert!(caps.bidirectional);
    assert!(caps.max_peers > 0);
}

#[test]
fn test_ble_service_uuids() {
    // Format: 8-4-4-4-12 hex digits.
    let expected_parts = [8, 4, 4, 4, 12];
    for (name, uuid) in [
        ("service", MLINK_SERVICE_UUID),
        ("tx", TX_CHAR_UUID),
        ("rx", RX_CHAR_UUID),
        ("ctrl", CTRL_CHAR_UUID),
    ] {
        let s = uuid.to_string();
        assert_eq!(s.len(), 36, "{name} uuid len should be 36: {s}");
        let parts: Vec<&str> = s.split('-').collect();
        assert_eq!(parts.len(), 5, "{name} uuid should have 5 parts: {s}");
        for (i, expected) in expected_parts.iter().enumerate() {
            assert_eq!(
                parts[i].len(),
                *expected,
                "{name} uuid part {i} wrong length: {s}"
            );
            assert!(
                parts[i].chars().all(|c| c.is_ascii_hexdigit()),
                "{name} uuid part {i} not hex: {s}"
            );
        }
    }

    // All four UUIDs must be distinct.
    let all = [MLINK_SERVICE_UUID, TX_CHAR_UUID, RX_CHAR_UUID, CTRL_CHAR_UUID];
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            assert_ne!(all[i], all[j], "uuid collision at {i},{j}");
        }
    }
}

// ============================================================================
// ipc transport
// ============================================================================

fn ipc_peer(path: &str) -> DiscoveredPeer {
    DiscoveredPeer {
        id: path.to_string(),
        name: path.to_string(),
        rssi: None,
        metadata: Vec::new(),
    }
}

#[tokio::test]
async fn test_ipc_listen_connect() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("listen_connect.sock");
    let path_str = path.to_string_lossy().into_owned();

    let server_path = path_str.clone();
    let server = tokio::spawn(async move {
        let mut t = IpcTransport::new(server_path);
        let mut conn = t.listen().await.expect("listen");
        let got = conn.read().await.expect("server read");
        assert_eq!(got, b"client->server");
        conn.write(b"server->client").await.expect("server write");
        conn.close().await.expect("server close");
    });

    // Give listener time to bind.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client_t = IpcTransport::new(path_str.clone());
    let mut client = client_t
        .connect(&ipc_peer(&path_str))
        .await
        .expect("connect");
    client.write(b"client->server").await.expect("client write");
    let echo = client.read().await.expect("client read");
    assert_eq!(echo, b"server->client");
    client.close().await.expect("client close");
    server.await.expect("server task join");
}

#[tokio::test]
async fn test_ipc_large_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("large.sock");
    let path_str = path.to_string_lossy().into_owned();

    let payload: Vec<u8> = (0..100_000).map(|i| (i & 0xFF) as u8).collect();
    let expected = payload.clone();

    let server_path = path_str.clone();
    let server = tokio::spawn(async move {
        let mut t = IpcTransport::new(server_path);
        let mut conn = t.listen().await.expect("listen");
        let got = conn.read().await.expect("server read");
        assert_eq!(got.len(), 100_000);
        assert_eq!(got, expected);
        conn.write(&got).await.expect("echo");
        conn.close().await.expect("close");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client_t = IpcTransport::new(path_str.clone());
    let mut client = client_t
        .connect(&ipc_peer(&path_str))
        .await
        .expect("connect");
    client.write(&payload).await.expect("client write large");
    let echo = client.read().await.expect("client read large");
    assert_eq!(echo.len(), 100_000);
    assert_eq!(echo, payload);
    client.close().await.expect("close");
    server.await.expect("server join");
}

#[tokio::test]
async fn test_ipc_multiple_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("multi.sock");
    let path_str = path.to_string_lossy().into_owned();

    let messages: Vec<Vec<u8>> = vec![
        b"first".to_vec(),
        b"second-message".to_vec(),
        vec![0xFFu8; 2048],
        b"".to_vec(),
        b"final".to_vec(),
    ];
    let server_msgs = messages.clone();

    let server_path = path_str.clone();
    let server = tokio::spawn(async move {
        let mut t = IpcTransport::new(server_path);
        let mut conn = t.listen().await.expect("listen");
        for (i, expected) in server_msgs.iter().enumerate() {
            let got = conn.read().await.expect("server read frame");
            assert_eq!(got, *expected, "server frame {i} mismatch");
        }
        conn.close().await.expect("close");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut client_t = IpcTransport::new(path_str.clone());
    let mut client = client_t
        .connect(&ipc_peer(&path_str))
        .await
        .expect("connect");

    for (i, msg) in messages.iter().enumerate() {
        client
            .write(msg)
            .await
            .unwrap_or_else(|e| panic!("client write {i}: {e:?}"));
    }
    client.close().await.expect("client close");
    server.await.expect("server join");
}
