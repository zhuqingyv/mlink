use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::{broadcast, Mutex, RwLock};

use crate::core::connection::{negotiate_role, ConnectionManager, Role};
use crate::core::link::TransportKind;
use crate::core::peer::{load_or_create_app_uuid, PeerManager};
use crate::core::security::TrustStore;
use crate::core::session::types::{LinkStatus, SessionEvent};
use crate::core::session::SessionManager;
use crate::protocol::errors::{MlinkError, Result};

mod connect;
mod io;
mod lifecycle;
mod rooms;
mod types;

#[cfg(test)]
mod tests;

pub use types::{
    NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT,
};

/// Active-link override installed by `switch_active_link`. TTL is the
/// `EXPLICIT_OVERRIDE_TTL_SECS` constant from `core::session::types`.
pub(super) type ExplicitOverrideMap = HashMap<String, (TransportKind, Instant)>;

/// Top-level mlink endpoint: owns peers, connections, trust store, and events.
pub struct Node {
    config: NodeConfig,
    app_uuid: String,
    peer_manager: Arc<PeerManager>,
    peer_states: Arc<RwLock<HashMap<String, PeerState>>>,
    /// Legacy byte-oriented connection registry. Still the source of truth for
    /// `send_raw` / `spawn_peer_reader` on single-link peers; dual-link peers
    /// go through `sessions` instead.
    pub(super) connections: Arc<Mutex<ConnectionManager>>,
    /// Multi-link Session registry (dual-transport). Populated alongside
    /// `connections` on every successful connect/accept so that attaching a
    /// second link later becomes a simple `SessionManager.attach_link` call.
    pub(super) sessions: Arc<SessionManager>,
    /// Manual active-link overrides — TTL `EXPLICIT_OVERRIDE_TTL_SECS`.
    pub(super) explicit_override: Arc<RwLock<ExplicitOverrideMap>>,
    trust_store: Arc<RwLock<TrustStore>>,
    state: StdMutex<NodeState>,
    events_tx: broadcast::Sender<NodeEvent>,
    session_events_tx: broadcast::Sender<SessionEvent>,
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
        let (session_events_tx, _) = broadcast::channel(128);

        Ok(Self {
            config,
            app_uuid,
            peer_manager: Arc::new(PeerManager::new()),
            peer_states: Arc::new(RwLock::new(HashMap::new())),
            connections: Arc::new(Mutex::new(ConnectionManager::new())),
            sessions: Arc::new(SessionManager::new()),
            explicit_override: Arc::new(RwLock::new(HashMap::new())),
            trust_store: Arc::new(RwLock::new(trust_store)),
            state: StdMutex::new(NodeState::Idle),
            events_tx,
            session_events_tx,
            room_hashes: StdMutex::new(HashSet::new()),
        })
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

    /// Subscribe to raw `SessionEvent`s. Callers that want the NodeEvent view
    /// (LinkAdded / LinkRemoved / LinkSwitched) should use `subscribe()` —
    /// this stream is for daemon-style forwarders that want the full
    /// SessionEvent (e.g. StreamProgress).
    pub fn subscribe_session_events(&self) -> broadcast::Receiver<SessionEvent> {
        self.session_events_tx.subscribe()
    }

    pub(super) fn events_tx(&self) -> &broadcast::Sender<NodeEvent> {
        &self.events_tx
    }

    pub(super) fn session_events_tx(&self) -> &broadcast::Sender<SessionEvent> {
        &self.session_events_tx
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


    pub fn role_for(&self, peer_app_uuid: &str) -> Role {
        negotiate_role(&self.app_uuid, peer_app_uuid)
    }

    pub async fn connection_count(&self) -> usize {
        let guard = self.connections.lock().await;
        guard.count()
    }

    pub fn trust_store(&self) -> Arc<RwLock<TrustStore>> {
        Arc::clone(&self.trust_store)
    }

    /// Status of all links attached to `peer_id`, including active/standby
    /// role. Returns an empty vec if the peer has no session.
    pub async fn peer_link_status(&self, peer_id: &str) -> Vec<LinkStatus> {
        self.sessions.peer_link_status(peer_id).await
    }
}
