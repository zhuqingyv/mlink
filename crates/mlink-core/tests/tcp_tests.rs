//! 集成测试：TCP transport 层端到端行为。
//!
//! 对应 `.claude/memory/TEST_CASES.md` 中的：
//! - T-I-04 对端 drop → read EOF
//! - T-I-05 对端 drop → write BrokenPipe/ConnectionReset
//! - T-I-06 大 payload（2 MiB）分包组包
//! - T-I-07 连续多帧无间隔往返
//! - T-I-08 MTU 上界（精确 65536）
//! - T-I-11 discover 解析 TXT 中的 room hash（mDNS 可软跳过）
//! - T-I-16 connect 解析非法 peer.id
//! - T-I-17 connect 对方未监听
//! - T-E-03 长度前缀流中断（只 2 字节 EOF）
//! - T-E-04 payload 中断（长度声明 10，实际 5 字节 EOF）

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::tcp::{TcpConnection, TcpTransport};
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};

// ---- T-I-04 ----------------------------------------------------------------
#[tokio::test]
async fn peer_drop_triggers_read_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        // accept 后立即 drop stream（不主动 close）
        let (_stream, _) = listener.accept().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    // 等待服务端 drop 生效
    server.await.unwrap();

    let err = client.read().await.unwrap_err();
    match err {
        MlinkError::Io(io) => {
            assert!(
                matches!(
                    io.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                ),
                "unexpected io kind: {:?}",
                io.kind()
            );
        }
        other => panic!("expected Io(UnexpectedEof-ish), got {other:?}"),
    }
}

// ---- T-I-05 ----------------------------------------------------------------
#[tokio::test]
async fn peer_drop_triggers_write_broken_pipe() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    server.await.unwrap();

    // 第一次 write 可能落入 socket buffer；连续多次写直到报错。
    let mut last_err: Option<MlinkError> = None;
    for _ in 0..100 {
        match client.write(b"hello").await {
            Ok(()) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => {
                last_err = Some(e);
                break;
            }
        }
    }
    let err = last_err.expect("one of the writes should fail after peer drop");
    match err {
        MlinkError::Io(io) => {
            assert!(
                matches!(
                    io.kind(),
                    std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::NotConnected
                ),
                "unexpected io kind on write: {:?}",
                io.kind()
            );
        }
        other => panic!("expected Io(BrokenPipe-ish), got {other:?}"),
    }
}

// ---- T-I-06 ----------------------------------------------------------------
#[tokio::test]
async fn large_payload_round_trip_2mib() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TcpConnection::new(stream, "s");
        let got = conn.read().await.unwrap();
        conn.write(&got).await.unwrap();
        conn.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    let payload = vec![0xABu8; 2 * 1024 * 1024];
    client.write(&payload).await.unwrap();
    let echo = client.read().await.unwrap();
    assert_eq!(echo.len(), payload.len());
    assert_eq!(echo, payload);
    client.close().await.unwrap();
    server.await.unwrap();
}

// ---- T-I-07 ----------------------------------------------------------------
#[tokio::test]
async fn hundred_frames_in_order_no_merge() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TcpConnection::new(stream, "s");
        let mut received: Vec<Vec<u8>> = Vec::with_capacity(100);
        for _ in 0..100u8 {
            let frame = conn.read().await.unwrap();
            received.push(frame);
        }
        received
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    for i in 0..100u8 {
        client.write(&[i; 8]).await.unwrap();
    }
    client.close().await.unwrap();

    let received = server.await.unwrap();
    assert_eq!(received.len(), 100);
    for (i, frame) in received.iter().enumerate() {
        assert_eq!(frame.len(), 8, "frame {i} length wrong");
        assert!(frame.iter().all(|&b| b == i as u8), "frame {i} content wrong");
    }
}

// ---- T-I-08 ----------------------------------------------------------------
#[tokio::test]
async fn mtu_upper_bound_65536_round_trip() {
    const MTU: usize = 65536;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TcpConnection::new(stream, "s");
        let got = conn.read().await.unwrap();
        conn.write(&got).await.unwrap();
        conn.close().await.unwrap();
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    let t = TcpTransport::new();
    assert_eq!(t.mtu(), MTU);
    let payload = vec![0x7Fu8; MTU];
    client.write(&payload).await.unwrap();
    let echo = client.read().await.unwrap();
    assert_eq!(echo.len(), MTU);
    assert_eq!(echo, payload);
    client.close().await.unwrap();
    server.await.unwrap();
}

// ---- T-I-11 ----------------------------------------------------------------
/// discover 解析 TXT 中的 room hash；如果 mDNS 被环境屏蔽，软跳过。
#[tokio::test]
async fn discover_parses_room_hash_from_txt() {
    let mut server_t = TcpTransport::new().with_discover_duration(Duration::from_millis(500));
    server_t.set_local_name("mlink-tcp-room");
    server_t.set_room_hash([0x11; 8]);

    let listener_task = tokio::spawn(async move {
        // listen() 会绑定 + 注册 mDNS，然后等一次连接；这里不期待真的有人连，
        // 主要是保持 transport 及其 AdvertiseHandle 存活。
        let _ = tokio::time::timeout(Duration::from_secs(4), server_t.listen()).await;
        server_t
    });

    // 给 mDNS 发布一点时间
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut client_t = TcpTransport::new().with_discover_duration(Duration::from_secs(2));
    let peers = client_t.discover().await.unwrap();

    let matched = peers
        .into_iter()
        .find(|p| p.metadata == vec![0x11; 8] && p.name == "mlink-tcp-room");
    match matched {
        Some(peer) => {
            assert_eq!(peer.metadata, vec![0x11; 8]);
            assert_eq!(peer.name, "mlink-tcp-room");
        }
        None => {
            eprintln!("[tcp-test] T-I-11: mDNS browse returned no match; skipping strict assertion");
        }
    }
    listener_task.abort();
    let _ = listener_task.await;
}

// ---- T-I-16 ----------------------------------------------------------------
#[tokio::test]
async fn connect_rejects_invalid_peer_id() {
    let mut t = TcpTransport::new();
    let peer = DiscoveredPeer {
        id: "not-a-socket-addr".into(),
        name: "".into(),
        rssi: None,
        metadata: vec![],
    };
    let res = t.connect(&peer).await;
    match res {
        Ok(_) => panic!("connect should fail on invalid peer id"),
        Err(MlinkError::HandlerError(msg)) => {
            assert!(
                msg.contains("tcp connect: invalid peer id"),
                "unexpected error message: {msg}"
            );
            assert!(
                msg.contains("not-a-socket-addr"),
                "original id should appear in message: {msg}"
            );
        }
        Err(other) => panic!("expected HandlerError, got {other:?}"),
    }
}

// ---- T-I-17 ----------------------------------------------------------------
#[tokio::test]
async fn connect_returns_error_when_no_listener() {
    // 找一个没人监听的端口：绑 0 拿端口号后立即释放。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut t = TcpTransport::new();
    let peer = DiscoveredPeer {
        id: addr.to_string(),
        name: "".into(),
        rssi: None,
        metadata: vec![],
    };
    let result = tokio::time::timeout(Duration::from_secs(3), t.connect(&peer)).await;
    let result = result.expect("connect should not hang");
    let err = match result {
        Ok(_) => panic!("connect should fail against closed port"),
        Err(e) => e,
    };
    match err {
        MlinkError::Io(io) => {
            assert!(
                matches!(
                    io.kind(),
                    std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::ConnectionReset
                ),
                "unexpected io kind: {:?}",
                io.kind()
            );
        }
        other => panic!("expected Io error, got {other:?}"),
    }
}

// ---- T-E-03 ----------------------------------------------------------------
#[tokio::test]
async fn length_prefix_truncated_returns_io_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // 只写 2 字节就 drop
        stream.write_all(&[0x00, 0x00]).await.unwrap();
        stream.flush().await.unwrap();
        drop(stream);
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    server.await.unwrap();

    let err = client.read().await.unwrap_err();
    match err {
        MlinkError::Io(io) => {
            assert_eq!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof,
                "expected UnexpectedEof, got {:?}",
                io.kind()
            );
        }
        other => panic!("expected Io(UnexpectedEof), got {other:?}"),
    }
}

// ---- T-E-04 ----------------------------------------------------------------
#[tokio::test]
async fn payload_truncated_returns_io_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // 声明 10 字节，实际只发 5 字节
        stream
            .write_all(&[0x00, 0x00, 0x00, 0x0A, 0x01, 0x02, 0x03, 0x04, 0x05])
            .await
            .unwrap();
        stream.flush().await.unwrap();
        drop(stream);
    });

    let stream = TcpStream::connect(addr).await.unwrap();
    let mut client = TcpConnection::new(stream, addr.to_string());
    server.await.unwrap();

    let err = client.read().await.unwrap_err();
    match err {
        MlinkError::Io(io) => {
            assert_eq!(
                io.kind(),
                std::io::ErrorKind::UnexpectedEof,
                "expected UnexpectedEof, got {:?}",
                io.kind()
            );
        }
        other => panic!("expected Io(UnexpectedEof), got {other:?}"),
    }
}
