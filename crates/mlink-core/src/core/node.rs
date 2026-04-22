use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
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
use crate::transport::{Connection, DiscoveredPeer, Transport};

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
            encrypt: false,
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
    /// A `MessageType::Message` frame arrived from `peer_id`. Payload is the
    /// decoded, decrypted, decompressed bytes. Only emitted for peers whose
    /// connection is being drained by the background reader spawned by
    /// `Node::spawn_peer_reader`.
    MessageReceived { peer_id: String, payload: Vec<u8> },
}

/// Top-level mlink endpoint: owns peers, connections, trust store, and events.
pub struct Node {
    config: NodeConfig,
    app_uuid: String,
    peer_manager: Arc<PeerManager>,
    peer_states: Arc<RwLock<HashMap<String, PeerState>>>,
    connections: Arc<Mutex<ConnectionManager>>,
    trust_store: Arc<RwLock<TrustStore>>,
    state: StdMutex<NodeState>,
    events_tx: broadcast::Sender<NodeEvent>,
    room_hashes: StdMutex<HashSet<[u8; 8]>>,
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
            state: StdMutex::new(NodeState::Idle),
            events_tx,
            room_hashes: StdMutex::new(HashSet::new()),
        })
    }

    /// Add a room hash to this node's membership set. The handshake will accept
    /// any peer whose advertised `room_hash` is present in this set. An empty
    /// set imposes no room restriction.
    pub fn add_room_hash(&self, hash: [u8; 8]) {
        self.room_hashes.lock().expect("room_hashes poisoned").insert(hash);
    }

    /// Remove a room hash from this node's membership set.
    pub fn remove_room_hash(&self, hash: &[u8; 8]) {
        self.room_hashes.lock().expect("room_hashes poisoned").remove(hash);
    }

    pub fn room_hashes(&self) -> HashSet<[u8; 8]> {
        self.room_hashes.lock().expect("room_hashes poisoned").clone()
    }

    /// First room hash we'll advertise on the wire. The Handshake protocol
    /// still carries a single `Option<[u8;8]>`, so when we belong to multiple
    /// rooms we just pick one — the peer will verify it against *its* set.
    fn wire_room_hash(&self) -> Option<[u8; 8]> {
        self.room_hashes
            .lock()
            .expect("room_hashes poisoned")
            .iter()
            .next()
            .copied()
    }

    pub fn state(&self) -> NodeState {
        *self.state.lock().expect("state poisoned")
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

    pub async fn start(&self) -> Result<()> {
        let mut st = self.state.lock().expect("state poisoned");
        if *st != NodeState::Idle {
            return Err(MlinkError::HandlerError(format!(
                "cannot start from state {:?}",
                *st
            )));
        }
        *st = NodeState::Discovering;
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        let ids: Vec<String> = self.connections.lock().await.list_ids();
        for id in ids {
            let _ = self.disconnect_peer(&id).await;
        }
        *self.state.lock().expect("state poisoned") = NodeState::Idle;
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

    /// True if we already hold a live connection keyed by `app_uuid`. Used by
    /// the CLI/serve loop (and internally here) to keep a second dial from
    /// racing a connection that has already completed its handshake.
    pub async fn has_peer(&self, app_uuid: &str) -> bool {
        self.connections.lock().await.contains(app_uuid)
    }

    pub async fn connect_peer(
        &self,
        transport: &mut dyn Transport,
        discovered: &DiscoveredPeer,
    ) -> Result<String> {
        eprintln!(
            "[mlink:conn] node.connect_peer ENTER peer_id={} name={:?}",
            discovered.id, discovered.name
        );
        self.set_peer_state(&discovered.id, NodeState::Connecting).await;

        let _role = negotiate_role(&self.app_uuid, &discovered.id);
        eprintln!(
            "[mlink:conn] node.connect_peer role (by wire-id, pre-handshake)={:?} my_app_uuid={} peer_wire_id={}",
            _role, self.app_uuid, discovered.id
        );

        let conn = transport.connect(discovered).await.map_err(|e| {
            eprintln!("[mlink:conn] node.connect_peer transport.connect FAILED peer={}: {e}", discovered.id);
            e
        })?;
        eprintln!("[mlink:conn] node.connect_peer transport.connect OK peer={}", discovered.id);

        let wire_room = self.wire_room_hash();
        let local_hs = Handshake {
            app_uuid: self.app_uuid.clone(),
            version: PROTOCOL_VERSION,
            mtu: transport.mtu() as u16,
            compress: true,
            // Advertise the *true* encryption state: we hold no session key at
            // handshake time, so encryption is not yet active. Matching
            // send_raw's actual behavior keeps the peer from trying to
            // decrypt plaintext.
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: wire_room,
        };

        eprintln!(
            "[mlink:conn] node.connect_peer -> perform_handshake peer={} wire_room_hash={:?}",
            discovered.id, wire_room
        );
        let peer_hs = match perform_handshake(&*conn, &local_hs).await {
            Ok(hs) => {
                eprintln!(
                    "[mlink:conn] node.connect_peer handshake OK peer_wire={} peer_app={} peer_room={:?}",
                    discovered.id, hs.app_uuid, hs.room_hash
                );
                hs
            }
            Err(e) => {
                eprintln!(
                    "[mlink:conn] node.connect_peer handshake FAILED peer={}: {e}",
                    discovered.id
                );
                let _ = conn.close().await;
                return Err(e);
            }
        };

        // Room membership check: if we advertise any rooms, the peer's claimed
        // room_hash must be in our set. Empty set = accept anyone. This lets a
        // single Node belong to several rooms at once — we just need *some*
        // overlap between our membership and the peer's claim.
        {
            let rooms = self.room_hashes.lock().expect("room_hashes poisoned");
            if !rooms.is_empty() {
                match peer_hs.room_hash {
                    Some(peer) if rooms.contains(&peer) => {
                        eprintln!(
                            "[mlink:conn] node.connect_peer room match OK peer={} room={:?}",
                            discovered.id, peer
                        );
                    }
                    other => {
                        eprintln!(
                            "[mlink:conn] node.connect_peer ROOM MISMATCH peer={} local_rooms={:?} peer_claims={:?}",
                            discovered.id, *rooms, other
                        );
                        drop(rooms);
                        let _ = conn.close().await;
                        return Err(MlinkError::RoomMismatch {
                            peer_id: discovered.id.clone(),
                        });
                    }
                }
            }
        }

        let peer_id = peer_hs.app_uuid.clone();

        // Idempotency: if we already hold a live connection for this app_uuid
        // (because the peer dialled us inbound while we were dialling out, or
        // the scanner surfaced the same peer twice), drop the new connection
        // and return the existing peer_id. Installing the duplicate would kick
        // the working one out of ConnectionManager and tear down the live link.
        {
            let guard = self.connections.lock().await;
            if guard.contains(&peer_id) {
                drop(guard);
                eprintln!(
                    "[mlink:conn] node.connect_peer DEDUP peer={} already connected — closing new dial",
                    peer_id
                );
                let _ = conn.close().await;
                return Ok(peer_id);
            }
        }

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
            guard.add(peer_id.clone(), Arc::from(conn));
        }
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }

        *self.state.lock().expect("state poisoned") = NodeState::Connected;
        let _ = self.events_tx.send(NodeEvent::PeerConnected {
            peer_id: peer_id.clone(),
        });

        Ok(peer_id)
    }

    /// Accept an incoming connection (peripheral side). Runs a handshake over
    /// the already-open connection, enforces the room-hash check, and — on
    /// success — registers the connection under the peer's app_uuid. On
    /// failure the connection is closed so callers don't need to clean up.
    pub async fn accept_incoming(
        &self,
        conn: Box<dyn Connection>,
        transport_id: &str,
        fallback_name: String,
    ) -> Result<String> {
        let wire_id = conn.peer_id().to_string();
        eprintln!(
            "[mlink:conn] node.accept_incoming ENTER wire_id={wire_id} transport={transport_id}"
        );
        let wire_room = self.wire_room_hash();
        let local_hs = Handshake {
            app_uuid: self.app_uuid.clone(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: wire_room,
        };

        eprintln!(
            "[mlink:conn] node.accept_incoming -> perform_handshake wire={wire_id} wire_room_hash={:?}",
            wire_room
        );
        let peer_hs = match perform_handshake(&*conn, &local_hs).await {
            Ok(hs) => {
                eprintln!(
                    "[mlink:conn] node.accept_incoming handshake OK wire={wire_id} peer_app={} peer_room={:?}",
                    hs.app_uuid, hs.room_hash
                );
                hs
            }
            Err(e) => {
                eprintln!(
                    "[mlink:conn] node.accept_incoming handshake FAILED wire={wire_id}: {e}"
                );
                let _ = conn.close().await;
                return Err(e);
            }
        };

        {
            let rooms = self.room_hashes.lock().expect("room_hashes poisoned");
            if !rooms.is_empty() {
                match peer_hs.room_hash {
                    Some(peer) if rooms.contains(&peer) => {
                        eprintln!(
                            "[mlink:conn] node.accept_incoming room match OK wire={wire_id} room={:?}",
                            peer
                        );
                    }
                    other => {
                        eprintln!(
                            "[mlink:conn] node.accept_incoming ROOM MISMATCH wire={wire_id} local_rooms={:?} peer_claims={:?}",
                            *rooms, other
                        );
                        drop(rooms);
                        let wire_peer_id = conn.peer_id().to_string();
                        let _ = conn.close().await;
                        return Err(MlinkError::RoomMismatch {
                            peer_id: wire_peer_id,
                        });
                    }
                }
            }
        }

        let peer_id = peer_hs.app_uuid.clone();

        // Idempotency: see `connect_peer`. If the outbound dial already won
        // the race, drop this inbound duplicate instead of evicting the live
        // connection. Returning Ok lets the caller treat it as a no-op.
        {
            let guard = self.connections.lock().await;
            if guard.contains(&peer_id) {
                drop(guard);
                eprintln!(
                    "[mlink:conn] node.accept_incoming DEDUP peer={} already connected — closing duplicate",
                    peer_id
                );
                let _ = conn.close().await;
                return Ok(peer_id);
            }
        }

        let peer = Peer {
            id: peer_id.clone(),
            name: fallback_name,
            app_uuid: peer_hs.app_uuid.clone(),
            connected_at: Instant::now(),
            transport_id: transport_id.to_string(),
        };

        self.peer_manager.add(peer).await;
        {
            let mut guard = self.connections.lock().await;
            guard.add(peer_id.clone(), Arc::from(conn));
        }
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }
        *self.state.lock().expect("state poisoned") = NodeState::Connected;
        let _ = self.events_tx.send(NodeEvent::PeerConnected {
            peer_id: peer_id.clone(),
        });
        Ok(peer_id)
    }

    /// Test hook: install a `Connection` for `peer_id` directly, bypassing
    /// transport discovery and handshake. Used by end-to-end tests to drive
    /// the Node with a pre-wired mock (e.g. `MockTransport::pair`).
    ///
    /// Production code paths should use `connect_peer` instead, which performs
    /// the handshake and wires up the full PeerState.
    pub async fn attach_connection(&self, peer_id: String, conn: Box<dyn Connection>) {
        {
            let mut guard = self.connections.lock().await;
            guard.add(peer_id.clone(), Arc::from(conn));
        }
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }
        *self.state.lock().expect("state poisoned") = NodeState::Connected;
        let _ = self.events_tx.send(NodeEvent::PeerConnected { peer_id });
    }

    pub async fn disconnect_peer(&self, peer_id: &str) -> Result<()> {
        let removed = {
            let mut guard = self.connections.lock().await;
            guard.remove(peer_id)
        };
        if let Some(conn) = removed {
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
        let (seq, compressed_flag, encrypted_flag, body) = {
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

            // Only claim "encrypted" on the wire when encryption is both
            // requested AND a session key exists. Otherwise the flag is a lie
            // and the peer will try to decrypt plaintext.
            let encrypted_flag = if self.config.encrypt {
                if let Some(key) = &st.aes_key {
                    body = crate::core::security::encrypt(&body, key, seq)?;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            (seq, compressed_flag, encrypted_flag, body)
        };

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
        // Clone the Arc<dyn Connection> under a brief lock, then release the
        // map lock before the actual write. Otherwise a parked reader holding
        // the map lock during `read().await` would block every `send_raw` in
        // the process — that was the exact BLE chat deadlock.
        let conn = {
            let guard = self.connections.lock().await;
            guard.shared(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?
        };
        conn.write(&bytes).await
    }

    /// Start a background task that drains messages from `peer_id` and
    /// publishes each `MessageType::Message` frame as a
    /// `NodeEvent::MessageReceived`. The task exits on the first read error
    /// (e.g. peer disconnect) after emitting `NodeEvent::PeerDisconnected` —
    /// it does *not* attempt to reconnect.
    ///
    /// Must be called after a successful `connect_peer` / `accept_incoming`
    /// for that peer; otherwise the reader will immediately see `PeerGone`
    /// and exit.
    pub fn spawn_peer_reader(&self, peer_id: String) -> tokio::task::JoinHandle<()> {
        let connections = Arc::clone(&self.connections);
        let peer_states = Arc::clone(&self.peer_states);
        let events_tx = self.events_tx.clone();
        let encrypt = self.config.encrypt;
        tokio::spawn(async move {
            loop {
                let res = recv_once(&connections, &peer_states, encrypt, &peer_id).await;
                match res {
                    Ok((msg_type, payload)) => {
                        if msg_type == MessageType::Message {
                            let _ = events_tx.send(NodeEvent::MessageReceived {
                                peer_id: peer_id.clone(),
                                payload,
                            });
                        }
                        // Heartbeats and other control frames are consumed
                        // silently — `recv_once` already refreshed the
                        // last-heartbeat timestamp.
                    }
                    Err(_) => {
                        let _ = events_tx.send(NodeEvent::PeerDisconnected {
                            peer_id: peer_id.clone(),
                        });
                        return;
                    }
                }
            }
        })
    }

    pub async fn recv_raw(&self, peer_id: &str) -> Result<(Frame, Vec<u8>)> {
        let conn = {
            let guard = self.connections.lock().await;
            guard.shared(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?
        };
        let bytes = conn.read().await?;

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

/// Read-decode-decrypt-decompress a single frame for the given peer using only
/// the shared state handles (no `&Node` needed). Extracted so
/// `spawn_peer_reader` can run in a background task without borrowing the
/// parent Node. Returns the message type plus the plaintext payload.
async fn recv_once(
    connections: &Arc<Mutex<ConnectionManager>>,
    peer_states: &Arc<RwLock<HashMap<String, PeerState>>>,
    encrypt: bool,
    peer_id: &str,
) -> Result<(MessageType, Vec<u8>)> {
    // Clone the Arc<dyn Connection> under the map lock, then release it
    // before awaiting read. Holding the map lock across read().await is what
    // caused the BLE chat deadlock: a parked reader blocked every concurrent
    // send_raw on the same Node.
    let conn = {
        let guard = connections.lock().await;
        guard.shared(peer_id).ok_or_else(|| MlinkError::PeerGone {
            peer_id: peer_id.to_string(),
        })?
    };
    let bytes = conn.read().await?;

    let frame = decode_frame(&bytes)?;
    let (compressed, encrypted, msg_type) = decode_flags(frame.flags);

    let mut payload = frame.payload.clone();

    if encrypted && encrypt {
        let states = peer_states.read().await;
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

    if msg_type == MessageType::Heartbeat {
        let mut states = peer_states.write().await;
        if let Some(s) = states.get_mut(peer_id) {
            s.last_heartbeat = Some(Instant::now());
        }
    }

    Ok((msg_type, payload))
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
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");
        node.start().await.expect("start");
        assert_eq!(node.state(), NodeState::Discovering);
    }

    #[tokio::test]
    async fn start_rejects_double_start() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
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
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
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
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
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
    fn node_config_default_encrypt_false() {
        // Default must be false: we hold no aes_key at construction time,
        // so claiming encrypt=true on the wire would be a lie.
        let c = NodeConfig::default();
        assert!(!c.encrypt);
        assert!(c.trust_store_path.is_none());
    }

    #[test]
    fn test_config_helper_used() {
        let _c = test_config();
    }

    #[tokio::test]
    async fn accept_incoming_matching_room_registers_peer() {
        let _tmp = set_tmp_home();
        let tmp_a = tempfile::tempdir().expect("tmp-a");
        let tmp_b = tempfile::tempdir().expect("tmp-b");
        let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
            .await
            .expect("new a");
        let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
            .await
            .expect("new b");
        let room = [0x55u8; 8];
        node_a.add_room_hash(room);
        node_b.add_room_hash(room);

        let (ca, cb) = crate::transport::mock::mock_pair();
        // Both sides run the handshake concurrently: one side acts as the
        // "incoming-accept" (node_a), the other as a pre-wired mock client
        // that drives perform_handshake directly.
        let hs_b = Handshake {
            app_uuid: node_b.app_uuid().to_string(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: Some(room),
        };
        let driver = tokio::spawn(async move {
            perform_handshake(&cb, &hs_b).await.expect("peer hs");
        });

        let got = node_a
            .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
            .await
            .expect("accept ok");
        driver.await.expect("driver joined");

        assert_eq!(got, node_b.app_uuid());
        assert_eq!(node_a.connection_count().await, 1);
    }

    #[tokio::test]
    async fn accept_incoming_mismatched_room_rejects() {
        let _tmp = set_tmp_home();
        let tmp_a = tempfile::tempdir().expect("tmp-a");
        let tmp_b = tempfile::tempdir().expect("tmp-b");
        let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
            .await
            .expect("new a");
        let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
            .await
            .expect("new b");
        node_a.add_room_hash([0x11; 8]);
        let bogus_room = Some([0x22u8; 8]);

        let (ca, cb) = crate::transport::mock::mock_pair();
        let hs_b = Handshake {
            app_uuid: node_b.app_uuid().to_string(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: bogus_room,
        };
        let driver = tokio::spawn(async move {
            // The peer will get either a handshake reply or a closed socket;
            // both are acceptable here — we only assert the local rejection.
            let _ = perform_handshake(&cb, &hs_b).await;
        });

        let err = node_a
            .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
            .await
            .unwrap_err();
        let _ = driver.await;
        assert!(matches!(err, MlinkError::RoomMismatch { .. }));
        assert_eq!(node_a.connection_count().await, 0);
    }

    #[tokio::test]
    async fn connect_peer_matching_room_roundtrip_is_accepted() {
        // Round-trip: add / remove a room and observe the membership set.
        let _tmp = set_tmp_home();
        let tmp = tempfile::tempdir().expect("tmp");
        let node = Node::new(tmp_config(tmp.path().join("t.json")))
            .await
            .expect("new");
        assert!(node.room_hashes().is_empty());
        node.add_room_hash([0xFE; 8]);
        assert!(node.room_hashes().contains(&[0xFE; 8]));
        node.remove_room_hash(&[0xFE; 8]);
        assert!(node.room_hashes().is_empty());
    }

    #[tokio::test]
    async fn node_in_multiple_rooms_accepts_peers_from_any_of_them() {
        // A node that belongs to {room_a, room_b} must accept a peer whose
        // handshake advertises room_b — it's in the set. Previously the Node
        // only tracked one room_hash, so this scenario could not be expressed.
        let _tmp = set_tmp_home();
        let tmp_a = tempfile::tempdir().expect("tmp-a");
        let tmp_b = tempfile::tempdir().expect("tmp-b");
        let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
            .await
            .expect("new a");
        let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
            .await
            .expect("new b");
        let room_a = [0xAAu8; 8];
        let room_b = [0xBBu8; 8];
        // node_a is a member of both rooms; node_b is only in room_b.
        node_a.add_room_hash(room_a);
        node_a.add_room_hash(room_b);
        node_b.add_room_hash(room_b);
        assert_eq!(node_a.room_hashes().len(), 2);

        let (ca, cb) = crate::transport::mock::mock_pair();
        let hs_b = Handshake {
            app_uuid: node_b.app_uuid().to_string(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: Some(room_b),
        };
        let driver = tokio::spawn(async move {
            perform_handshake(&cb, &hs_b).await.expect("peer hs");
        });

        let got = node_a
            .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
            .await
            .expect("accept ok — room_b is in node_a's set");
        driver.await.expect("driver joined");
        assert_eq!(got, node_b.app_uuid());
        assert_eq!(node_a.connection_count().await, 1);
    }

    #[tokio::test]
    async fn node_rejects_peer_whose_room_is_not_in_set() {
        // Negative case for multi-room membership: node belongs to {A, B};
        // peer claims room C — rejection must still fire.
        let _tmp = set_tmp_home();
        let tmp_a = tempfile::tempdir().expect("tmp-a");
        let tmp_b = tempfile::tempdir().expect("tmp-b");
        let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
            .await
            .expect("new a");
        let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
            .await
            .expect("new b");
        node_a.add_room_hash([0xAA; 8]);
        node_a.add_room_hash([0xBB; 8]);

        let (ca, cb) = crate::transport::mock::mock_pair();
        let hs_b = Handshake {
            app_uuid: node_b.app_uuid().to_string(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: Some([0xCC; 8]),
        };
        let driver = tokio::spawn(async move {
            let _ = perform_handshake(&cb, &hs_b).await;
        });

        let err = node_a
            .accept_incoming(Box::new(ca), "mock", "mock-peer".into())
            .await
            .unwrap_err();
        let _ = driver.await;
        assert!(matches!(err, MlinkError::RoomMismatch { .. }));
        assert_eq!(node_a.connection_count().await, 0);
    }

    #[tokio::test]
    async fn attach_connection_enables_send_recv_without_handshake() {
        let _tmp = set_tmp_home();
        let tmp2 = tempfile::tempdir().expect("tmp2");
        let node = Node::new(tmp_config(tmp2.path().join("t.json")))
            .await
            .expect("new");

        let (a, b) = crate::transport::mock::mock_pair();
        node.attach_connection("mock-peer-b".into(), Box::new(a)).await;

        assert_eq!(node.connection_count().await, 1);
        assert_eq!(
            node.peer_state("mock-peer-b").await,
            Some(NodeState::Connected)
        );
        assert_eq!(node.state(), NodeState::Connected);

        // End-to-end frame over the attached mock.
        node.send_raw("mock-peer-b", MessageType::Message, b"hello")
            .await
            .expect("send");

        use crate::transport::Connection as _;
        let bytes = b.read().await.expect("peer read");
        let frame = crate::protocol::frame::decode_frame(&bytes).expect("decode");
        let (_, _, mt) = crate::protocol::types::decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Message);
    }

    #[tokio::test]
    async fn accept_incoming_is_idempotent_when_already_connected() {
        // Scenario: outbound dial already finished (attach_connection simulates
        // the post-handshake registration). A duplicate inbound arrives for the
        // same app_uuid. accept_incoming must close the duplicate and return
        // Ok(existing_id) instead of evicting the live connection.
        let _tmp = set_tmp_home();
        let tmp_a = tempfile::tempdir().expect("tmp-a");
        let tmp_b = tempfile::tempdir().expect("tmp-b");
        let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
            .await
            .expect("new a");
        let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
            .await
            .expect("new b");
        let peer_b_id = node_b.app_uuid().to_string();

        // Pretend node_b is already connected via the outbound path.
        let (existing, _drop_peer) = crate::transport::mock::mock_pair();
        node_a
            .attach_connection(peer_b_id.clone(), Box::new(existing))
            .await;
        assert_eq!(node_a.connection_count().await, 1);
        assert!(node_a.has_peer(&peer_b_id).await);

        // Now simulate an inbound duplicate from the same peer: run a
        // handshake on the duplicate pair; accept_incoming should DEDUP.
        let (ca, cb) = crate::transport::mock::mock_pair();
        let hs_b = Handshake {
            app_uuid: peer_b_id.clone(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: None,
        };
        let driver = tokio::spawn(async move {
            let _ = perform_handshake(&cb, &hs_b).await;
        });

        let got = node_a
            .accept_incoming(Box::new(ca), "mock", "dup".into())
            .await
            .expect("dedup accept returns Ok");
        let _ = driver.await;

        assert_eq!(got, peer_b_id);
        // Still exactly one connection — the duplicate was dropped.
        assert_eq!(node_a.connection_count().await, 1);
    }

    #[tokio::test]
    async fn has_peer_tracks_attach_and_disconnect() {
        let _tmp = set_tmp_home();
        let tmp = tempfile::tempdir().expect("tmp");
        let node = Node::new(tmp_config(tmp.path().join("t.json")))
            .await
            .expect("new");
        assert!(!node.has_peer("peer-x").await);
        let (a, _b) = crate::transport::mock::mock_pair();
        node.attach_connection("peer-x".into(), Box::new(a)).await;
        assert!(node.has_peer("peer-x").await);
        node.disconnect_peer("peer-x").await.expect("disc");
        assert!(!node.has_peer("peer-x").await);
    }

    #[tokio::test]
    async fn spawn_peer_reader_emits_message_received_event() {
        // Wire two Nodes via a mock pair, attach the connections on both ends,
        // spawn a reader on one side, send a Message from the other, and
        // assert the event surfaces on the broadcast channel.
        let _tmp = set_tmp_home();
        let tmp_a = tempfile::tempdir().expect("tmp-a");
        let tmp_b = tempfile::tempdir().expect("tmp-b");
        let node_a = Node::new(tmp_config(tmp_a.path().join("a.json")))
            .await
            .expect("new a");
        let node_b = Node::new(tmp_config(tmp_b.path().join("b.json")))
            .await
            .expect("new b");

        let (a_conn, b_conn) = crate::transport::mock::mock_pair();
        node_a.attach_connection("peer-b".into(), Box::new(a_conn)).await;
        node_b.attach_connection("peer-a".into(), Box::new(b_conn)).await;

        // Subscribe BEFORE spawning so we don't miss the event.
        let mut events = node_a.subscribe();
        let handle = node_a.spawn_peer_reader("peer-b".into());

        // Send a Message frame from node_b → node_a.
        node_b
            .send_raw("peer-a", MessageType::Message, b"hello chat")
            .await
            .expect("send");

        // Drain events until we see MessageReceived or time out.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let timeout = deadline.saturating_duration_since(tokio::time::Instant::now());
            if timeout.is_zero() {
                panic!("timed out waiting for MessageReceived event");
            }
            match tokio::time::timeout(timeout, events.recv()).await {
                Ok(Ok(NodeEvent::MessageReceived { peer_id, payload })) => {
                    assert_eq!(peer_id, "peer-b");
                    assert_eq!(payload, b"hello chat".to_vec());
                    break;
                }
                Ok(Ok(_)) => continue, // ignore PeerConnected noise from attach
                Ok(Err(_)) => panic!("event stream closed"),
                Err(_) => panic!("timeout waiting for MessageReceived"),
            }
        }

        handle.abort();
    }

    /// Regression test for the BLE chat deadlock: a reader parked in
    /// `conn.read().await` must not block a concurrent `send_raw` on the same
    /// Node. Before the SharedConnection refactor, `recv_once` held
    /// `connections.lock().await` across the read, and `send_raw` needed the
    /// same mutex — so a peer that never sent a byte silently stalled every
    /// outbound message the node tried to make.
    #[tokio::test]
    async fn send_raw_not_blocked_by_parked_reader() {
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // A connection whose read() hangs forever (like a BLE central that
        // isn't yet writing) but whose write() succeeds and records calls via
        // a shared counter. The counter lives outside the conn so the test can
        // observe writes without holding a second reference to the trait obj.
        struct HangingReadConn {
            id: String,
            writes: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl Connection for HangingReadConn {
            async fn read(&self) -> Result<Vec<u8>> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn write(&self, _data: &[u8]) -> Result<()> {
                self.writes.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn close(&self) -> Result<()> {
                Ok(())
            }
            fn peer_id(&self) -> &str {
                &self.id
            }
        }

        let _tmp = set_tmp_home();
        let trust_tmp = tempfile::tempdir().expect("tmp");
        let node = Node::new(tmp_config(trust_tmp.path().join("t.json")))
            .await
            .expect("new");

        let writes = Arc::new(AtomicUsize::new(0));
        let conn = HangingReadConn {
            id: "stuck-peer".into(),
            writes: Arc::clone(&writes),
        };
        node.attach_connection("stuck-peer".into(), Box::new(conn)).await;

        // Park a reader. It will hang forever inside conn.read() — exactly the
        // shape of the BLE chat host waiting on the client's first byte.
        let reader_handle = node.spawn_peer_reader("stuck-peer".into());

        // Give the reader a beat to reach its pending read.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Sending must complete on its own without the reader making progress.
        // If the old per-map-mutex deadlock is back, this future will hang and
        // the tokio::time::timeout below will trip.
        let send_fut = node.send_raw("stuck-peer", MessageType::Message, b"hi");
        tokio::time::timeout(Duration::from_secs(2), send_fut)
            .await
            .expect("send_raw must not be blocked by parked reader")
            .expect("send ok");

        assert_eq!(writes.load(Ordering::SeqCst), 1);
        reader_handle.abort();
    }
}
