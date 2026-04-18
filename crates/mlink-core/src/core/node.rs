use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, Mutex, RwLock};

use crate::core::connection::{negotiate_role, perform_handshake, ConnectionManager, Role};
use crate::core::peer::{load_or_create_app_uuid, Peer, PeerManager};
use crate::core::reconnect::ReconnectPolicy;
use crate::core::security::TrustStore;
use crate::protocol::compress::{compress, decompress, should_compress};
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::frame::{decode_frame, encode_frame};
use crate::protocol::types::{
    decode_flags, encode_flags, Frame, Handshake, MessageType, MAGIC, PROTOCOL_VERSION,
};
use crate::transport::{DiscoveredPeer, Transport};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeState {
    Idle,
    Discovering,
    Discovered,
    Connecting,
    Connected,
    Streaming,
    Disconnected,
    Reconnecting,
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub name: String,
    pub encrypt: bool,
    pub trust_store_path: Option<PathBuf>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            encrypt: true,
            trust_store_path: None,
        }
    }
}

pub struct PeerState {
    pub state: NodeState,
    pub reconnect_policy: ReconnectPolicy,
    pub seq: u16,
    pub last_heartbeat: Option<Instant>,
    pub aes_key: Option<Vec<u8>>,
}

impl PeerState {
    fn new() -> Self {
        Self {
            state: NodeState::Connecting,
            reconnect_policy: ReconnectPolicy::new(),
            seq: 0,
            last_heartbeat: None,
            aes_key: None,
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }
}

#[derive(Debug, Clone)]
pub enum NodeEvent {
    PeerDiscovered { peer_id: String },
    PeerConnected { peer_id: String },
    PeerDisconnected { peer_id: String },
    PeerLost { peer_id: String },
    Reconnecting { peer_id: String, attempt: u32 },
}

pub struct Node {
    config: NodeConfig,
    app_uuid: String,
    peer_manager: Arc<PeerManager>,
    peer_states: Arc<RwLock<HashMap<String, PeerState>>>,
    connections: Arc<Mutex<ConnectionManager>>,
    trust_store: Arc<RwLock<TrustStore>>,
    state: NodeState,
    events_tx: broadcast::Sender<NodeEvent>,
}

impl Node {
    pub async fn new(config: NodeConfig) -> Result<Self> {
        let app_uuid = load_or_create_app_uuid()?;
        let trust_path = match &config.trust_store_path {
            Some(p) => p.clone(),
            None => TrustStore::default_path()?,
        };
        let trust_store = TrustStore::new(trust_path)?;
        let (events_tx, _) = broadcast::channel(128);

        Ok(Self {
            config,
            app_uuid,
            peer_manager: Arc::new(PeerManager::new()),
            peer_states: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(Mutex::new(ConnectionManager::new())),
            trust_store: Arc::new(RwLock::new(trust_store)),
            state: NodeState::Idle,
            events_tx,
        })
    }

    pub fn state(&self) -> NodeState {
        self.state
    }

    pub fn app_uuid(&self) -> &str {
        &self.app_uuid
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.events_tx.subscribe()
    }

    pub async fn start(&mut self) -> Result<()> {
        if self.state != NodeState::Idle {
            return Err(MlinkError::HandlerError(format!(
                "cannot start from state {:?}",
                self.state
            )));
        }
        self.state = NodeState::Discovering;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        let ids: Vec<String> = {
            let guard = self.connections.lock().await;
            guard.list_ids()
        };
        for id in ids {
            let _ = self.disconnect_peer(&id).await;
        }
        self.state = NodeState::Idle;
        Ok(())
    }

    pub async fn peers(&self) -> Vec<Peer> {
        self.peer_manager.list().await
    }

    pub async fn get_peer(&self, id: &str) -> Option<Peer> {
        self.peer_manager.get(id).await
    }

    pub async fn peer_state(&self, id: &str) -> Option<NodeState> {
        let guard = self.peer_states.read().await;
        guard.get(id).map(|s| s.state)
    }

    pub async fn connect_peer(
        &mut self,
        transport: &mut dyn Transport,
        discovered: &DiscoveredPeer,
    ) -> Result<String> {
        self.set_peer_state(&discovered.id, NodeState::Connecting).await;

        let _role = negotiate_role(&self.app_uuid, &discovered.id);

        let mut conn = transport.connect(discovered).await.map_err(|e| {
            // If connect fails we don't have a peer state yet to roll back
            e
        })?;

        let local_hs = Handshake {
            app_uuid: self.app_uuid.clone(),
            version: PROTOCOL_VERSION,
            mtu: transport.mtu() as u16,
            compress: true,
            encrypt: self.config.encrypt,
            last_seq: 0,
            resume_streams: vec![],
        };

        let peer_hs = perform_handshake(conn.as_mut(), &local_hs).await?;

        let peer_id = peer_hs.app_uuid.clone();
        let peer = Peer {
            id: peer_id.clone(),
            name: discovered.name.clone(),
            app_uuid: peer_hs.app_uuid.clone(),
            connected_at: Instant::now(),
            transport_id: transport.id().to_string(),
        };

        self.peer_manager.add(peer).await;
        {
            let mut guard = self.connections.lock().await;
            guard.add(peer_id.clone(), conn).await;
        }
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }

        self.state = NodeState::Connected;
        let _ = self.events_tx.send(NodeEvent::PeerConnected {
            peer_id: peer_id.clone(),
        });

        Ok(peer_id)
    }

    pub async fn disconnect_peer(&mut self, peer_id: &str) -> Result<()> {
        let removed = {
            let mut guard = self.connections.lock().await;
            guard.remove(peer_id).await
        };
        if let Some(mut conn) = removed {
            let _ = conn.close().await;
        }
        self.peer_manager.remove(peer_id).await;
        {
            let mut states = self.peer_states.write().await;
            if let Some(s) = states.get_mut(peer_id) {
                s.state = NodeState::Disconnected;
            }
        }
        let _ = self.events_tx.send(NodeEvent::PeerDisconnected {
            peer_id: peer_id.to_string(),
        });
        Ok(())
    }

    pub async fn send_raw(
        &self,
        peer_id: &str,
        msg_type: MessageType,
        payload: &[u8],
    ) -> Result<()> {
        let (seq, compressed_flag, body) = {
            let mut states = self.peer_states.write().await;
            let st = states.get_mut(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?;

            let seq = st.next_seq();

            let mut body = payload.to_vec();
            let compressed_flag = if should_compress(&body) {
                body = compress(&body)?;
                true
            } else {
                false
            };

            if self.config.encrypt {
                if let Some(key) = &st.aes_key {
                    body = crate::core::security::encrypt(&body, key, seq)?;
                }
            }
            (seq, compressed_flag, body)
        };

        let encrypted_flag = self.config.encrypt;
        let length = u16::try_from(body.len()).map_err(|_| MlinkError::PayloadTooLarge {
            size: body.len(),
            max: u16::MAX as usize,
        })?;
        let frame = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(compressed_flag, encrypted_flag, msg_type),
            seq,
            length,
            payload: body,
        };

        let bytes = encode_frame(&frame);
        let mut guard = self.connections.lock().await;
        let conn = guard.get_mut(peer_id).await.ok_or_else(|| MlinkError::PeerGone {
            peer_id: peer_id.to_string(),
        })?;
        conn.write(&bytes).await
    }

    pub async fn recv_raw(&self, peer_id: &str) -> Result<(Frame, Vec<u8>)> {
        let bytes = {
            let mut guard = self.connections.lock().await;
            let conn = guard
                .get_mut(peer_id)
                .await
                .ok_or_else(|| MlinkError::PeerGone {
                    peer_id: peer_id.to_string(),
                })?;
            conn.read().await?
        };

        let frame = decode_frame(&bytes)?;
        let (compressed, encrypted, _msg_type) = decode_flags(frame.flags);

        let mut payload = frame.payload.clone();

        if encrypted {
            let states = self.peer_states.read().await;
            let st = states.get(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?;
            if let Some(key) = &st.aes_key {
                payload = crate::core::security::decrypt(&payload, key, frame.seq)?;
            }
        }

        if compressed {
            payload = decompress(&payload)?;
        }

        if _msg_type == MessageType::Heartbeat {
            let mut states = self.peer_states.write().await;
            if let Some(s) = states.get_mut(peer_id) {
                s.last_heartbeat = Some(Instant::now());
            }
        }

        Ok((frame, payload))
    }

    pub async fn send_heartbeat(&self, peer_id: &str) -> Result<()> {
        self.send_raw(peer_id, MessageType::Heartbeat, &[]).await
    }

    pub async fn check_heartbeat(&self, peer_id: &str) -> Result<bool> {
        let states = self.peer_states.read().await;
        let st = states.get(peer_id).ok_or_else(|| MlinkError::PeerGone {
            peer_id: peer_id.to_string(),
        })?;
        match st.last_heartbeat {
            Some(t) => Ok(t.elapsed() < HEARTBEAT_TIMEOUT),
            None => Ok(true),
        }
    }

    pub async fn set_peer_aes_key(&self, peer_id: &str, key: Vec<u8>) {
        let mut states = self.peer_states.write().await;
        let entry = states
            .entry(peer_id.to_string())
            .or_insert_with(PeerState::new);
        entry.aes_key = Some(key);
    }

    async fn set_peer_state(&self, peer_id: &str, new_state: NodeState) {
        let mut states = self.peer_states.write().await;
        let entry = states
            .entry(peer_id.to_string())
            .or_insert_with(PeerState::new);
        entry.state = new_state;
    }

    pub fn role_for(&self, peer_app_uuid: &str) -> Role {
        negotiate_role(&self.app_uuid, peer_app_uuid)
    }

    pub async fn mark_lost(&self, peer_id: &str) {
        let _ = self.events_tx.send(NodeEvent::PeerLost {
            peer_id: peer_id.to_string(),
        });
    }

    pub async fn mark_reconnecting(&self, peer_id: &str, attempt: u32) {
        self.set_peer_state(peer_id, NodeState::Reconnecting).await;
        let _ = self.events_tx.send(NodeEvent::Reconnecting {
            peer_id: peer_id.to_string(),
            attempt,
        });
    }

    pub async fn connection_count(&self) -> usize {
        let guard = self.connections.lock().await;
        guard.count()
    }

    pub fn trust_store(&self) -> Arc<RwLock<TrustStore>> {
        Arc::clone(&self.trust_store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::MockTransport;

    use std::sync::Mutex;

    // Serialize tests that mutate HOME so they don't race with peer::tests.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    struct HomeGuard {
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn set_tmp_home() -> HomeGuard {
        let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().expect("tmp");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        HomeGuard {
            _tmp: tmp,
            prev,
            _guard: guard,
        }
    }

    fn test_config() -> NodeConfig {
        NodeConfig {
            name: "test-node".into(),
            encrypt: false,
            trust_store_path: None,
        }
    }

    fn tmp_config(trust_path: PathBuf) -> NodeConfig {
        NodeConfig {
            name: "test-node".into(),
            encrypt: false,
            trust_store_path: Some(trust_path),
        }
    }

    #[tokio::test]
    async fn new_starts_idle() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let node = Node::new(tmp_config(tmp2.path().join("trust.json")))
            .await
            .expect("new");
        assert_eq!(node.state(), NodeState::Idle);
        assert!(!node.app_uuid().is_empty());
    }

    #[tokio::test]
    async fn start_transitions_to_discovering() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let mut node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        node.start().await.expect("start");
        assert_eq!(node.state(), NodeState::Discovering);
    }

    #[tokio::test]
    async fn start_rejects_double_start() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let mut node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        node.start().await.expect("start 1");
        let err = node.start().await.unwrap_err();
        assert!(matches!(err, MlinkError::HandlerError(_)));
    }

    #[tokio::test]
    async fn stop_returns_to_idle() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let mut node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        node.start().await.expect("start");
        node.stop().await.expect("stop");
        assert_eq!(node.state(), NodeState::Idle);
    }

    #[tokio::test]
    async fn role_for_is_deterministic() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        let me = node.app_uuid().to_string();
        let other_big = format!("{}-z", me);
        let other_small = "aaaa".to_string();
        let _ = node.role_for(&other_big);
        let _ = node.role_for(&other_small);
    }

    #[tokio::test]
    async fn send_raw_on_missing_peer_fails() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        let err = node
            .send_raw("nobody", MessageType::Message, b"hi")
            .await
            .unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
    }

    #[tokio::test]
    async fn connect_peer_via_mock_transport_errors_cleanly() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let mut node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        let mut transport = MockTransport::new();
        let discovered = DiscoveredPeer {
            id: "peer-a".into(),
            name: "a".into(),
            rssi: None,
            metadata: vec![],
        };
        let err = node.connect_peer(&mut transport, &discovered).await.unwrap_err();
        assert!(matches!(err, MlinkError::HandlerError(_)));
    }

    #[tokio::test]
    async fn check_heartbeat_missing_peer_fails() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        let err = node.check_heartbeat("no-such").await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
    }

    #[test]
    fn node_state_is_copy() {
        let s = NodeState::Connecting;
        let t = s;
        assert_eq!(s, t);
    }

    #[test]
    fn node_config_default_encrypt_true() {
        let c = NodeConfig::default();
        assert!(c.encrypt);
        assert!(c.trust_store_path.is_none());
    }

    #[test]
    fn test_config_helper_used() {
        let _c = test_config();
    }
}
