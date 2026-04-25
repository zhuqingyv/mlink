use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::sync::{broadcast, Mutex, RwLock};

use crate::core::connection::{negotiate_role, ConnectionManager, Role};
use crate::core::peer::{load_or_create_app_uuid, PeerManager};
use crate::core::security::TrustStore;
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
}

