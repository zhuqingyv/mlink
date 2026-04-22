use async_trait::async_trait;

use crate::protocol::errors::Result;

#[derive(Debug, Clone)]
pub struct TransportCapabilities {
    pub max_peers: usize,
    pub throughput_bps: u64,
    pub latency_ms: u32,
    pub reliable: bool,
    pub bidirectional: bool,
}

#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub id: String,
    pub name: String,
    pub rssi: Option<i16>,
    pub metadata: Vec<u8>,
}

/// A live byte-oriented connection to a peer on a specific `Transport`.
///
/// `read` and `write` take `&self` so a reader task and a writer task can make
/// progress in parallel on the same connection. Implementations are expected
/// to protect their mutable transport state internally (e.g. a `Mutex` around
/// a TcpStream write half, per-rx channel for BLE peripheral). Serialising
/// access through an outer `Mutex<Box<dyn Connection>>` at the Node layer
/// would reintroduce the head-of-line block that parked `send_raw` whenever
/// a peer reader was waiting for bytes.
#[async_trait]
pub trait Connection: Send + Sync {
    async fn read(&self) -> Result<Vec<u8>>;
    async fn write(&self, data: &[u8]) -> Result<()>;
    async fn close(&self) -> Result<()>;
    fn peer_id(&self) -> &str;
}

/// Pluggable link-layer: BLE, IPC, mock, or any bidirectional byte channel.
#[async_trait]
pub trait Transport: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> TransportCapabilities;
    async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>>;
    async fn connect(&mut self, peer: &DiscoveredPeer) -> Result<Box<dyn Connection>>;
    async fn listen(&mut self) -> Result<Box<dyn Connection>>;
    fn mtu(&self) -> usize;
}
