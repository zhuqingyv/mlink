use std::sync::Arc;
use std::time::Instant;

use super::{Node, NodeEvent, NodeState, PeerState};
use crate::core::peer::Peer;
use crate::protocol::errors::Result;
use crate::transport::Connection;

impl Node {
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

    pub async fn set_peer_aes_key(&self, peer_id: &str, key: Vec<u8>) {
        let mut states = self.peer_states.write().await;
        let entry = states
            .entry(peer_id.to_string())
            .or_insert_with(PeerState::new);
        entry.aes_key = Some(key);
    }

    pub(super) async fn set_peer_state(&self, peer_id: &str, new_state: NodeState) {
        let mut states = self.peer_states.write().await;
        let entry = states
            .entry(peer_id.to_string())
            .or_insert_with(PeerState::new);
        entry.state = new_state;
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
}
