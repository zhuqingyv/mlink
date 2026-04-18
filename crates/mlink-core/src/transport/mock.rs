use async_trait::async_trait;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::protocol::errors::{MlinkError, Result};

use super::transport_trait::{
    Connection, DiscoveredPeer, Transport, TransportCapabilities,
};

pub struct MockConnection {
    peer_id: String,
    tx: Option<Sender<Vec<u8>>>,
    rx: Option<Receiver<Vec<u8>>>,
}

impl MockConnection {
    pub fn new(peer_id: impl Into<String>, tx: Sender<Vec<u8>>, rx: Receiver<Vec<u8>>) -> Self {
        Self {
            peer_id: peer_id.into(),
            tx: Some(tx),
            rx: Some(rx),
        }
    }
}

#[async_trait]
impl Connection for MockConnection {
    async fn read(&mut self) -> Result<Vec<u8>> {
        let rx = self
            .rx
            .as_mut()
            .ok_or_else(|| MlinkError::PeerGone {
                peer_id: self.peer_id.clone(),
            })?;
        rx.recv().await.ok_or_else(|| MlinkError::PeerGone {
            peer_id: self.peer_id.clone(),
        })
    }

    async fn write(&mut self, data: &[u8]) -> Result<()> {
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| MlinkError::PeerGone {
                peer_id: self.peer_id.clone(),
            })?;
        tx.send(data.to_vec())
            .await
            .map_err(|_| MlinkError::PeerGone {
                peer_id: self.peer_id.clone(),
            })
    }

    async fn close(&mut self) -> Result<()> {
        self.tx.take();
        self.rx.take();
        Ok(())
    }

    fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

pub fn mock_pair() -> (MockConnection, MockConnection) {
    let (tx_a, rx_b) = mpsc::channel::<Vec<u8>>(64);
    let (tx_b, rx_a) = mpsc::channel::<Vec<u8>>(64);
    let a = MockConnection::new("mock-peer-a", tx_a, rx_a);
    let b = MockConnection::new("mock-peer-b", tx_b, rx_b);
    (a, b)
}

pub struct MockTransport;

impl MockTransport {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Transport for MockTransport {
    fn id(&self) -> &str {
        "mock"
    }

    fn capabilities(&self) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 100,
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
        Err(MlinkError::HandlerError(
            "MockTransport::connect is unimplemented; use mock_pair() in tests".into(),
        ))
    }

    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        Err(MlinkError::HandlerError(
            "MockTransport::listen is unimplemented; use mock_pair() in tests".into(),
        ))
    }

    fn mtu(&self) -> usize {
        512
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_pair_roundtrip() {
        let (mut a, mut b) = mock_pair();
        a.write(b"hello").await.unwrap();
        let got = b.read().await.unwrap();
        assert_eq!(got, b"hello");

        b.write(b"world").await.unwrap();
        let got = a.read().await.unwrap();
        assert_eq!(got, b"world");
    }

    #[tokio::test]
    async fn close_ends_peer_read() {
        let (mut a, mut b) = mock_pair();
        a.close().await.unwrap();
        let err = b.read().await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
    }

    #[tokio::test]
    async fn write_after_close_fails() {
        let (mut a, _b) = mock_pair();
        a.close().await.unwrap();
        let err = a.write(b"x").await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
    }

    #[test]
    fn transport_metadata() {
        let t = MockTransport::new();
        assert_eq!(t.id(), "mock");
        assert_eq!(t.mtu(), 512);
        let caps = t.capabilities();
        assert_eq!(caps.max_peers, 100);
        assert_eq!(caps.throughput_bps, u64::MAX);
        assert_eq!(caps.latency_ms, 0);
        assert!(caps.reliable);
        assert!(caps.bidirectional);
    }

    #[tokio::test]
    async fn discover_returns_empty() {
        let mut t = MockTransport::new();
        let peers = t.discover().await.unwrap();
        assert!(peers.is_empty());
    }

    #[test]
    fn peer_ids_distinct() {
        let (a, b) = mock_pair();
        assert_ne!(a.peer_id(), b.peer_id());
    }
}
