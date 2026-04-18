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
#[async_trait]
pub trait Connection: Send + Sync {
    async fn read(&mut self) -> Result<Vec<u8>>;
    async fn write(&mut self, data: &[u8]) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
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
