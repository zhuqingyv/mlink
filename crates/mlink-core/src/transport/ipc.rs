use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::protocol::errors::{MlinkError, Result};

use super::transport_trait::{
    Connection, DiscoveredPeer, Transport, TransportCapabilities,
};

const DEFAULT_SOCKET_PATH: &str = "/tmp/mlink.sock";

pub struct IpcTransport {
    socket_path: PathBuf,
    listener: Option<UnixListener>,
}

impl IpcTransport {
    pub fn new(socket_path: impl Into<String>) -> Self {
        Self {
            socket_path: PathBuf::from(socket_path.into()),
            listener: None,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Default for IpcTransport {
    fn default() -> Self {
        Self::new(DEFAULT_SOCKET_PATH)
    }
}

#[async_trait]
impl Transport for IpcTransport {
    fn id(&self) -> &str {
        "ipc"
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
        if tokio::fs::try_exists(&self.socket_path).await? {
            let path_str = self.socket_path.to_string_lossy().into_owned();
            Ok(vec![DiscoveredPeer {
                id: path_str.clone(),
                name: path_str,
                rssi: None,
                metadata: Vec::new(),
            }])
        } else {
            Ok(Vec::new())
        }
    }

    async fn connect(&mut self, peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
        let target: &Path = if peer.id.is_empty() {
            self.socket_path.as_path()
        } else {
            Path::new(peer.id.as_str())
        };
        let stream = UnixStream::connect(target).await?;
        let peer_id = target.to_string_lossy().into_owned();
        Ok(Box::new(IpcConnection::new(stream, peer_id)))
    }

    async fn listen(&mut self) -> Result<Box<dyn Connection>> {
        if self.listener.is_none() {
            if tokio::fs::try_exists(&self.socket_path).await? {
                tokio::fs::remove_file(&self.socket_path).await?;
            }
            let listener = UnixListener::bind(&self.socket_path)?;
            self.listener = Some(listener);
        }
        let listener = self.listener.as_ref().expect("listener bound above");
        let (stream, addr) = listener.accept().await?;
        let peer_id = addr
            .as_pathname()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ipc-peer".to_string());
        Ok(Box::new(IpcConnection::new(stream, peer_id)))
    }

    fn mtu(&self) -> usize {
        usize::MAX
    }
}

pub struct IpcConnection {
    // See TcpConnection: split the stream so reader and writer tasks make
    // progress independently rather than queuing through one lock.
    read_half: Mutex<Option<OwnedReadHalf>>,
    write_half: Mutex<Option<OwnedWriteHalf>>,
    peer_id: String,
}

impl IpcConnection {
    pub fn new(stream: UnixStream, peer_id: impl Into<String>) -> Self {
        let (r, w) = stream.into_split();
        Self {
            read_half: Mutex::new(Some(r)),
            write_half: Mutex::new(Some(w)),
            peer_id: peer_id.into(),
        }
    }
}

#[async_trait]
impl Connection for IpcConnection {
    async fn read(&self) -> Result<Vec<u8>> {
        let mut guard = self.read_half.lock().await;
        let stream = guard.as_mut().ok_or_else(|| MlinkError::PeerGone {
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

    async fn write(&self, data: &[u8]) -> Result<()> {
        let mut guard = self.write_half.lock().await;
        let stream = guard.as_mut().ok_or_else(|| MlinkError::PeerGone {
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

    async fn close(&self) -> Result<()> {
        self.read_half.lock().await.take();
        let taken = self.write_half.lock().await.take();
        if let Some(mut w) = taken {
            match w.shutdown().await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn transport_metadata() {
        let t = IpcTransport::new("/tmp/mlink-test.sock");
        assert_eq!(t.id(), "ipc");
        assert_eq!(t.mtu(), usize::MAX);
        let caps = t.capabilities();
        assert_eq!(caps.max_peers, 100);
        assert_eq!(caps.throughput_bps, u64::MAX);
        assert_eq!(caps.latency_ms, 0);
        assert!(caps.reliable);
        assert!(caps.bidirectional);
    }

    #[test]
    fn default_uses_tmp_path() {
        let t = IpcTransport::default();
        assert_eq!(t.socket_path(), Path::new(DEFAULT_SOCKET_PATH));
    }

    #[tokio::test]
    async fn discover_missing_socket_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        let mut t = IpcTransport::new(path.to_string_lossy().into_owned());
        let peers = t.discover().await.unwrap();
        assert!(peers.is_empty());
    }

    #[tokio::test]
    async fn discover_existing_socket_returns_peer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("present.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let mut t = IpcTransport::new(path.to_string_lossy().into_owned());
        let peers = t.discover().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].id, path.to_string_lossy());
    }

    #[tokio::test]
    async fn listen_and_connect_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("rt.sock");
        let path_str = path.to_string_lossy().into_owned();

        let server_path = path_str.clone();
        let server = tokio::spawn(async move {
            let mut t = IpcTransport::new(server_path);
            let conn = t.listen().await.unwrap();
            let got = conn.read().await.unwrap();
            conn.write(&got).await.unwrap();
            conn.close().await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client_t = IpcTransport::new(path_str.clone());
        let peer = DiscoveredPeer {
            id: path_str.clone(),
            name: path_str,
            rssi: None,
            metadata: Vec::new(),
        };
        let client = client_t.connect(&peer).await.unwrap();
        client.write(b"ping").await.unwrap();
        let echo = client.read().await.unwrap();
        assert_eq!(echo, b"ping");
        client.close().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn write_after_close_fails() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("closed.sock");
        let path_str = path.to_string_lossy().into_owned();

        let server_path = path_str.clone();
        let server = tokio::spawn(async move {
            let mut t = IpcTransport::new(server_path);
            let conn = t.listen().await.unwrap();
            let _ = conn.read().await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client_t = IpcTransport::new(path_str.clone());
        let peer = DiscoveredPeer {
            id: path_str.clone(),
            name: path_str,
            rssi: None,
            metadata: Vec::new(),
        };
        let client = client_t.connect(&peer).await.unwrap();
        client.close().await.unwrap();
        let err = client.write(b"x").await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
        drop(server);
    }

    #[tokio::test]
    async fn empty_payload_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.sock");
        let path_str = path.to_string_lossy().into_owned();

        let server_path = path_str.clone();
        let server = tokio::spawn(async move {
            let mut t = IpcTransport::new(server_path);
            let conn = t.listen().await.unwrap();
            let got = conn.read().await.unwrap();
            assert!(got.is_empty());
            conn.write(&[]).await.unwrap();
            conn.close().await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut client_t = IpcTransport::new(path_str.clone());
        let peer = DiscoveredPeer {
            id: path_str.clone(),
            name: path_str,
            rssi: None,
            metadata: Vec::new(),
        };
        let client = client_t.connect(&peer).await.unwrap();
        client.write(&[]).await.unwrap();
        let echo = client.read().await.unwrap();
        assert!(echo.is_empty());
        client.close().await.unwrap();
        server.await.unwrap();
    }
}
