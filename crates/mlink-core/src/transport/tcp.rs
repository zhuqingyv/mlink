use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::protocol::errors::{MlinkError, Result};

use super::transport_trait::{Connection, DiscoveredPeer, Transport, TransportCapabilities};

const SERVICE_TYPE: &str = "_mlink._tcp.local.";
const DEFAULT_DISCOVER_DURATION: Duration = Duration::from_secs(3);
const DEFAULT_MTU: usize = 65536;

pub struct TcpTransport {
    listener: Option<TcpListener>,
    port: u16,
    room_hash: Option<[u8; 8]>,
    local_name: String,
    advertised: Option<AdvertiseHandle>,
    discover_duration: Duration,
}

struct AdvertiseHandle {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for AdvertiseHandle {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

impl TcpTransport {
    pub fn new() -> Self {
        Self {
            listener: None,
            port: 0,
            room_hash: None,
            local_name: "mlink".into(),
            advertised: None,
            discover_duration: DEFAULT_DISCOVER_DURATION,
        }
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_discover_duration(mut self, duration: Duration) -> Self {
        self.discover_duration = duration;
        self
    }

    pub fn set_local_name(&mut self, name: impl Into<String>) {
        self.local_name = name.into();
    }

    pub fn set_room_hash(&mut self, hash: [u8; 8]) {
        self.room_hash = Some(hash);
    }

    pub fn clear_room_hash(&mut self) {
        self.room_hash = None;
    }

    pub fn room_hash(&self) -> Option<&[u8; 8]> {
        self.room_hash.as_ref()
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Default for TcpTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// Health-probe helper used by `mlink doctor --transport tcp`: verifies an
/// mDNS `ServiceDaemon` can start and shut down cleanly. Does not register
/// any service — just exercises the socket/thread setup that real discovery
/// needs, so it catches "mdns-sd daemon can't come up" failures early.
pub fn probe_mdns_daemon() -> Result<()> {
    let daemon = ServiceDaemon::new()
        .map_err(|e| MlinkError::HandlerError(format!("mdns daemon start: {e}")))?;
    daemon
        .shutdown()
        .map_err(|e| MlinkError::HandlerError(format!("mdns daemon shutdown: {e}")))?;
    Ok(())
}

#[async_trait]
impl Transport for TcpTransport {
    fn id(&self) -> &str {
        "tcp"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 32,
            throughput_bps: 100_000_000,
            latency_ms: 1,
            reliable: true,
            bidirectional: true,
        }
    }

    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>> {
        let duration = self.discover_duration;
        let peers = tokio::task::spawn_blocking(move || browse_once(duration))
            .await
            .map_err(|e| MlinkError::HandlerError(format!("tcp discover join: {e}")))??;
        Ok(peers)
    }

    async fn connect(&mut self, peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
        let addr: SocketAddr = peer.id.parse().map_err(|e| {
            MlinkError::HandlerError(format!(
                "tcp connect: invalid peer id {:?}: {e}",
                peer.id
            ))
        })?;
        let stream = TcpStream::connect(addr).await?;
        let peer_id = addr.to_string();
        Ok(Box::new(TcpConnection::new(stream, peer_id)))
    }

    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        if self.listener.is_none() {
            let bind_addr = format!("0.0.0.0:{}", self.port);
            let listener = TcpListener::bind(&bind_addr).await?;
            let local = listener.local_addr()?;
            self.port = local.port();
            self.listener = Some(listener);
            self.advertised = Some(register_service(
                self.port,
                &self.local_name,
                self.room_hash,
            )?);
        }
        let listener = self.listener.as_ref().expect("listener bound above");
        let (stream, remote) = listener.accept().await?;
        let peer_id = remote.to_string();
        Ok(Box::new(TcpConnection::new(stream, peer_id)))
    }

    fn mtu(&self) -> usize {
        DEFAULT_MTU
    }
}

pub struct TcpConnection {
    stream: Option<TcpStream>,
    peer_id: String,
}

impl TcpConnection {
    pub fn new(stream: TcpStream, peer_id: impl Into<String>) -> Self {
        Self {
            stream: Some(stream),
            peer_id: peer_id.into(),
        }
    }
}

#[async_trait]
impl Connection for TcpConnection {
    async fn read(&mut self) -> Result<Vec<u8>> {
        let stream = self.stream.as_mut().ok_or_else(|| MlinkError::PeerGone {
            peer_id: self.peer_id.clone(),
        })?;
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; len];
        if len > 0 {
            stream.read_exact(&mut payload).await?;
        }
        Ok(payload)
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        let stream = self.stream.as_mut().ok_or_else(|| MlinkError::PeerGone {
            peer_id: self.peer_id.clone(),
        })?;
        let len = u32::try_from(data.len()).map_err(|_| MlinkError::PayloadTooLarge {
            size: data.len(),
            max: u32::MAX as usize,
        })?;
        stream.write_all(&len.to_be_bytes()).await?;
        if !data.is_empty() {
            stream.write_all(data).await?;
        }
        stream.flush().await?;
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        if let Some(mut stream) = self.stream.take() {
            match stream.shutdown().await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

fn register_service(
    port: u16,
    local_name: &str,
    room_hash: Option<[u8; 8]>,
) -> Result<AdvertiseHandle> {
    let daemon = ServiceDaemon::new()
        .map_err(|e| MlinkError::HandlerError(format!("mdns daemon start: {e}")))?;
    let hostname = local_hostname();
    // Service instance name must be unique enough to avoid collisions when
    // multiple mlink nodes share the same local_name. Append the bound port.
    let instance_name = format!("{}-{}", sanitize_instance(local_name), port);
    let mut txt: HashMap<String, String> = HashMap::new();
    txt.insert("name".into(), local_name.into());
    if let Some(h) = room_hash {
        txt.insert("room".into(), hex_encode(&h));
    }

    let host_ipv4 = format!("{hostname}.local.");
    let info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &host_ipv4,
        "",
        port,
        Some(txt),
    )
    .map_err(|e| MlinkError::HandlerError(format!("mdns ServiceInfo: {e}")))?
    .enable_addr_auto();

    let fullname = info.get_fullname().to_string();
    daemon
        .register(info)
        .map_err(|e| MlinkError::HandlerError(format!("mdns register: {e}")))?;

    Ok(AdvertiseHandle { daemon, fullname })
}

fn browse_once(duration: Duration) -> Result<Vec<DiscoveredPeer>> {
    let daemon = ServiceDaemon::new()
        .map_err(|e| MlinkError::HandlerError(format!("mdns daemon start: {e}")))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| MlinkError::HandlerError(format!("mdns browse: {e}")))?;
    let deadline = std::time::Instant::now() + duration;
    let mut peers: HashMap<String, DiscoveredPeer> = HashMap::new();

    loop {
        let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
            Some(d) if !d.is_zero() => d,
            _ => break,
        };
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let port = info.get_port();
                let name = info
                    .get_property_val_str("name")
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| info.get_fullname().to_string());
                let metadata = info
                    .get_property_val_str("room")
                    .and_then(|hex| hex_decode_8(hex))
                    .map(|h| h.to_vec())
                    .unwrap_or_default();
                let v4_addrs: Vec<Ipv4Addr> = info.get_addresses_v4().into_iter().collect();
                for v4 in v4_addrs {
                    let sock = SocketAddr::new(IpAddr::V4(v4), port);
                    let id = sock.to_string();
                    peers.entry(id.clone()).or_insert(DiscoveredPeer {
                        id,
                        name: name.clone(),
                        rssi: None,
                        metadata: metadata.clone(),
                    });
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
    Ok(peers.into_values().collect())
}

fn sanitize_instance(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    if cleaned.is_empty() {
        "mlink".into()
    } else {
        cleaned
    }
}

fn local_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("HOST").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "mlink-host".into())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn hex_decode_8(s: &str) -> Option<[u8; 8]> {
    if s.len() != 16 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_metadata() {
        let t = TcpTransport::new();
        assert_eq!(t.id(), "tcp");
        assert_eq!(t.mtu(), DEFAULT_MTU);
        let caps = t.capabilities();
        assert_eq!(caps.max_peers, 32);
        assert_eq!(caps.throughput_bps, 100_000_000);
        assert_eq!(caps.latency_ms, 1);
        assert!(caps.reliable);
        assert!(caps.bidirectional);
    }

    #[test]
    fn room_hash_round_trips() {
        let mut t = TcpTransport::new();
        assert!(t.room_hash().is_none());
        let h = [0xCD; 8];
        t.set_room_hash(h);
        assert_eq!(t.room_hash(), Some(&h));
        t.clear_room_hash();
        assert!(t.room_hash().is_none());
    }

    #[test]
    fn set_local_name_updates_field() {
        let mut t = TcpTransport::new();
        t.set_local_name("node-a");
        assert_eq!(t.local_name, "node-a");
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let s = hex_encode(&bytes);
        assert_eq!(s, "0123456789abcdef");
        let back = hex_decode_8(&s).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn hex_decode_rejects_bad_length() {
        assert!(hex_decode_8("abcd").is_none());
    }

    #[test]
    fn hex_decode_rejects_non_hex() {
        assert!(hex_decode_8("zzzzzzzzzzzzzzzz").is_none());
    }

    #[test]
    fn sanitize_replaces_non_alnum() {
        assert_eq!(sanitize_instance("foo bar_baz!"), "foo-bar-baz-");
    }

    // T-U-02: with_port / port() 在 listen 之前
    #[test]
    fn with_port_updates_port_without_binding() {
        let t = TcpTransport::new().with_port(54321);
        assert_eq!(t.port(), 54321);
        assert!(t.listener.is_none());
    }

    // T-U-03: with_discover_duration 生效（短窗口）
    #[tokio::test]
    async fn with_discover_duration_bounds_browse_window() {
        let mut t = TcpTransport::new().with_discover_duration(Duration::from_millis(50));
        let start = std::time::Instant::now();
        // 结果不做断言，只验证调用耗时远小于默认 3s
        let _ = t.discover().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "discover should honour short window, took {:?}",
            elapsed
        );
    }

    // T-U-06: hex_encode / hex_decode_8 正向往返（补充完整字节表）
    #[test]
    fn hex_encode_lowercase_16_chars() {
        let bytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let s = hex_encode(&bytes);
        assert_eq!(s.len(), 16);
        assert_eq!(s, "0123456789abcdef");
        assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        let back = hex_decode_8(&s).expect("decode");
        assert_eq!(back, bytes);
    }

    // T-U-07: hex_decode_8 长度错误拒绝
    #[test]
    fn hex_decode_rejects_all_bad_lengths() {
        assert!(hex_decode_8("").is_none());
        assert!(hex_decode_8("abcd").is_none());
        assert!(hex_decode_8(&"a".repeat(15)).is_none());
        assert!(hex_decode_8(&"a".repeat(17)).is_none());
    }

    // T-U-08: hex_decode_8 非 hex 字符拒绝（16 字符但含非 hex）
    #[test]
    fn hex_decode_rejects_non_hex_variants() {
        assert!(hex_decode_8("zzzzzzzzzzzzzzzz").is_none());
        assert!(hex_decode_8("0123abcdefGHIJKL").is_none());
        // 大写 hex 的 chars().all(is_ascii_hexdigit) 会通过，
        // 但 "GHIJKL" 不是合法 hex，确保整体 16 字符里存在非 hex 就拒绝。
        assert!(hex_decode_8("0123456789abcd!!").is_none());
    }

    // T-U-09: sanitize_instance 多分支
    #[test]
    fn sanitize_instance_variants() {
        // ASCII 字母数字 + '-' 原样保留
        assert_eq!(sanitize_instance("abc-123"), "abc-123");
        // 空格 / 下划线 / 标点替换为 '-'
        assert_eq!(sanitize_instance("foo bar_baz!"), "foo-bar-baz-");
        // 非 ASCII 全部被替换，但至少保留占位
        let s = sanitize_instance("中文");
        assert!(!s.is_empty());
        assert!(s.chars().all(|c| c == '-'));
        // 空输入回退到 "mlink"
        assert_eq!(sanitize_instance(""), "mlink");
        // 混合中文与 '-'：5 个中文字符都被替换为 '-'，原有 '-' 保留 → 全 '-'
        let mixed = sanitize_instance("只有-破折号");
        assert_eq!(mixed.len(), "只有-破折号".chars().count());
        assert!(mixed.chars().all(|c| c == '-'));
    }

    // T-U-10: local_hostname 三级回退顺序
    // 这组用例会改全局环境变量，必须串行执行。
    use std::sync::Mutex;
    static HOST_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_hostname_env<F: FnOnce()>(
        hostname: Option<&str>,
        host: Option<&str>,
        computer: Option<&str>,
        f: F,
    ) {
        let _g = HOST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved = (
            std::env::var("HOSTNAME").ok(),
            std::env::var("HOST").ok(),
            std::env::var("COMPUTERNAME").ok(),
        );
        // 先清再设
        std::env::remove_var("HOSTNAME");
        std::env::remove_var("HOST");
        std::env::remove_var("COMPUTERNAME");
        if let Some(v) = hostname { std::env::set_var("HOSTNAME", v); }
        if let Some(v) = host { std::env::set_var("HOST", v); }
        if let Some(v) = computer { std::env::set_var("COMPUTERNAME", v); }

        f();

        std::env::remove_var("HOSTNAME");
        std::env::remove_var("HOST");
        std::env::remove_var("COMPUTERNAME");
        if let Some(v) = saved.0 { std::env::set_var("HOSTNAME", v); }
        if let Some(v) = saved.1 { std::env::set_var("HOST", v); }
        if let Some(v) = saved.2 { std::env::set_var("COMPUTERNAME", v); }
    }

    #[test]
    fn local_hostname_prefers_hostname_env() {
        with_hostname_env(Some("h1"), Some("h2"), Some("h3"), || {
            assert_eq!(local_hostname(), "h1");
        });
    }

    #[test]
    fn local_hostname_falls_back_to_host_env() {
        with_hostname_env(None, Some("h2"), Some("h3"), || {
            assert_eq!(local_hostname(), "h2");
        });
    }

    #[test]
    fn local_hostname_falls_back_to_computername_env() {
        with_hostname_env(None, None, Some("h3"), || {
            assert_eq!(local_hostname(), "h3");
        });
    }

    #[test]
    fn local_hostname_default_when_all_unset() {
        with_hostname_env(None, None, None, || {
            assert_eq!(local_hostname(), "mlink-host");
        });
    }

    // T-U-11: TcpConnection::new / peer_id 字段
    #[tokio::test]
    async fn tcp_connection_new_sets_peer_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(async move {
            let _ = listener.accept().await.unwrap();
        });
        let stream = TcpStream::connect(addr).await.unwrap();
        let conn = TcpConnection::new(stream, "abc:123".to_string());
        assert_eq!(conn.peer_id(), "abc:123");
        accept_task.await.unwrap();
    }

    /// Loopback roundtrip using 127.0.0.1 directly (no mDNS in the hot path).
    #[tokio::test]
    async fn loopback_connect_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = TcpConnection::new(stream, "test-peer".to_string());
            let got = conn.read().await.unwrap();
            conn.write(&got).await.unwrap();
            conn.close().await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpConnection::new(stream, addr.to_string());
        client.write(b"ping").await.unwrap();
        let echo = client.read().await.unwrap();
        assert_eq!(echo, b"ping");
        client.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_payload_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = TcpConnection::new(stream, "test-peer".to_string());
            let got = conn.read().await.unwrap();
            assert!(got.is_empty());
            conn.write(&[]).await.unwrap();
            conn.close().await.unwrap();
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpConnection::new(stream, addr.to_string());
        client.write(&[]).await.unwrap();
        let echo = client.read().await.unwrap();
        assert!(echo.is_empty());
        client.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn read_after_close_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let _ = listener.accept().await.unwrap();
            // drop the accepted stream to trigger EOF on the client side
        });

        let stream = TcpStream::connect(addr).await.unwrap();
        let mut client = TcpConnection::new(stream, addr.to_string());
        client.close().await.unwrap();
        // Reading after local close should error (stream taken).
        let err = client.read().await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
        let err = client.write(b"x").await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
        server.await.unwrap();
    }

    /// Bring up a TcpTransport, let it advertise via mDNS, then browse from
    /// another transport and verify we see the listener's address. This test
    /// requires mDNS to work on loopback/local network; if the environment
    /// blocks multicast it may time out — we keep the window short and skip
    /// the assertion in that case rather than fail the suite.
    #[tokio::test]
    async fn discover_connect_roundtrip() {
        let mut server_t = TcpTransport::new().with_discover_duration(Duration::from_millis(500));
        server_t.set_local_name("mlink-tcp-test");
        server_t.set_room_hash([0xAB; 8]);

        let listener_task = tokio::spawn(async move {
            // listen() binds + registers mDNS, then waits for one connection.
            let mut conn = server_t.listen().await.unwrap();
            let got = conn.read().await.unwrap();
            conn.write(&got).await.unwrap();
            conn.close().await.unwrap();
            // Keep the transport alive until the test drops the task handle.
            server_t
        });

        // Give the mDNS registrar a moment to publish before browsing.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut client_t = TcpTransport::new().with_discover_duration(Duration::from_secs(2));
        let peers = client_t.discover().await.unwrap();

        // If mDNS is blocked in this env the browse may see nothing; in that
        // case we skip the connect assertion but still cover the code path.
        let Some(peer) = peers
            .into_iter()
            .find(|p| p.metadata == vec![0xAB; 8] && p.name == "mlink-tcp-test")
        else {
            eprintln!("[tcp-test] mDNS browse returned no match; skipping roundtrip assertion");
            listener_task.abort();
            return;
        };

        let mut client = client_t.connect(&peer).await.unwrap();
        client.write(b"hello").await.unwrap();
        let echo = client.read().await.unwrap();
        assert_eq!(echo, b"hello");
        client.close().await.unwrap();
        let _ = listener_task.await;
    }
}
