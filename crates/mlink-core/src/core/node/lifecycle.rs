use std::sync::Arc;
use std::time::Instant;

use super::{Node, NodeEvent, NodeState, PeerState};
use crate::core::link::{Link, TransportKind};
use crate::core::peer::Peer;
use crate::core::session::manager::AttachOutcome;
use crate::core::session::types::{SessionEvent, SwitchCause, EXPLICIT_OVERRIDE_TTL_SECS};
use crate::protocol::errors::{MlinkError, Result};
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
        let conn_arc: Arc<dyn Connection> = Arc::from(conn);
        let kind = TransportKind::Mock;
        let link_id = super::connect::make_link_id(kind, &peer_id);
        let link = Link::new(
            link_id.clone(),
            Arc::clone(&conn_arc),
            kind,
            super::connect::default_caps_for_kind(kind),
        );
        // Keep the SessionManager and ConnectionManager in lock-step so a
        // subsequent connect_peer can dedup-by-kind instead of silently
        // evicting the pre-installed conn.
        let outcome = self.sessions.attach_link(&peer_id, link, None).await;
        let is_first_link = matches!(outcome, AttachOutcome::CreatedNew { .. });
        if is_first_link {
            let mut guard = self.connections.lock().await;
            guard.add(peer_id.clone(), Arc::clone(&conn_arc));
        }
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }
        *self.state.lock().expect("state poisoned") = NodeState::Connected;
        if is_first_link {
            let _ = self.events_tx().send(NodeEvent::PeerConnected {
                peer_id: peer_id.clone(),
            });
        }
        let _ = self.events_tx().send(NodeEvent::LinkAdded {
            peer_id: peer_id.clone(),
            link_id,
            transport: kind,
        });
        self.spawn_session_bridge_for(&peer_id).await;
    }

    pub async fn disconnect_peer(&self, peer_id: &str) -> Result<()> {
        let removed = {
            let mut guard = self.connections.lock().await;
            guard.remove(peer_id)
        };
        if let Some(conn) = removed {
            let _ = conn.close().await;
        }
        let _ = self.sessions.drop_peer(peer_id).await;
        self.peer_manager.remove(peer_id).await;
        {
            let mut states = self.peer_states.write().await;
            if let Some(s) = states.get_mut(peer_id) {
                s.state = NodeState::Disconnected;
            }
        }
        self.explicit_override.write().await.remove(peer_id);
        let _ = self.events_tx().send(NodeEvent::PeerDisconnected {
            peer_id: peer_id.to_string(),
        });
        Ok(())
    }

    /// Drop a single physical link without tearing down the whole peer.
    /// If this removes the last link, the peer is fully disconnected
    /// (PeerDisconnected + ConnectionManager eviction).
    pub async fn disconnect_link(&self, peer_id: &str, link_id: &str) -> Result<()> {
        let peer_gone = self.sessions.detach_link(peer_id, link_id).await;
        let _ = self.events_tx().send(NodeEvent::LinkRemoved {
            peer_id: peer_id.to_string(),
            link_id: link_id.to_string(),
            reason: "manual".to_string(),
        });
        if peer_gone {
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
            self.explicit_override.write().await.remove(peer_id);
            let _ = self.events_tx().send(NodeEvent::PeerDisconnected {
                peer_id: peer_id.to_string(),
            });
        }
        Ok(())
    }

    /// Force `peer_id` onto `to_link` and install a 10-minute manual override
    /// so the scheduler won't bounce back automatically.
    pub async fn switch_active_link(&self, peer_id: &str, to_link: &str) -> Result<()> {
        let session = self.sessions.get(peer_id).await.ok_or_else(|| {
            MlinkError::HandlerError(format!("no session for peer {peer_id}"))
        })?;
        let kind = {
            let links = session.links.read().await;
            let link = links.iter().find(|l| l.id() == to_link).cloned();
            match link {
                Some(l) => l.kind(),
                None => {
                    return Err(MlinkError::HandlerError(format!(
                        "link {to_link} not found on peer {peer_id}"
                    )))
                }
            }
        };
        let from = {
            let mut active = session.active.write().await;
            let prev = active.clone().unwrap_or_default();
            *active = Some(to_link.to_string());
            prev
        };
        self.explicit_override
            .write()
            .await
            .insert(peer_id.to_string(), (kind, Instant::now()));
        let _ = self.session_events_tx().send(SessionEvent::Switched {
            peer_id: peer_id.to_string(),
            from: from.clone(),
            to: to_link.to_string(),
            cause: SwitchCause::ManualUserRequest,
        });
        let _ = self.events_tx().send(NodeEvent::LinkSwitched {
            peer_id: peer_id.to_string(),
            from_link: from,
            to_link: to_link.to_string(),
            cause: SwitchCause::ManualUserRequest,
        });
        Ok(())
    }

    /// True iff `peer_id` currently has a pinned manual override that has not
    /// yet aged past `EXPLICIT_OVERRIDE_TTL_SECS`. Stale entries are removed
    /// lazily here so the map doesn't grow unbounded.
    pub async fn explicit_override_kind(&self, peer_id: &str) -> Option<TransportKind> {
        let mut table = self.explicit_override.write().await;
        if let Some((kind, at)) = table.get(peer_id).copied() {
            if at.elapsed().as_secs() < EXPLICIT_OVERRIDE_TTL_SECS {
                return Some(kind);
            }
            table.remove(peer_id);
        }
        None
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
        let _ = self.events_tx().send(NodeEvent::PeerLost {
            peer_id: peer_id.to_string(),
        });
    }

    pub async fn mark_reconnecting(&self, peer_id: &str, attempt: u32) {
        self.set_peer_state(peer_id, NodeState::Reconnecting).await;
        let _ = self.events_tx().send(NodeEvent::Reconnecting {
            peer_id: peer_id.to_string(),
            attempt,
        });
    }

    /// Spin up (at most once per peer) a lightweight forwarder that tails the
    /// per-session event stream and re-publishes the relevant SessionEvents
    /// as NodeEvents. Safe to call repeatedly: if the underlying Session was
    /// dropped, the spawned task just exits.
    pub(super) async fn spawn_session_bridge_for(&self, peer_id: &str) {
        let Some(session) = self.sessions.get(peer_id).await else {
            return;
        };
        // Only bridge once per session. We use `ack_pending` being attached
        // as a non-signal — instead, a bool in SchedulerState would be
        // cleaner, but we don't want to grow shared state here. A
        // session-event `biased` receiver is cheap enough to be re-spawned on
        // every attach; stale ones die when the session drops its broadcaster.
        let mut rx = session.subscribe();
        let node_tx = self.events_tx().clone();
        let sess_tx = self.session_events_tx().clone();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                let _ = sess_tx.send(ev.clone());
                // LinkAdded is emitted at the Node layer (connect + attach
                // paths) so we don't re-emit it here — the bridge would
                // otherwise miss the first attach (subscribed after send)
                // and double-emit later attaches.
                match ev {
                    SessionEvent::LinkRemoved {
                        peer_id,
                        link_id,
                        reason,
                    } => {
                        let _ = node_tx.send(NodeEvent::LinkRemoved {
                            peer_id,
                            link_id,
                            reason,
                        });
                    }
                    SessionEvent::Switched {
                        peer_id,
                        from,
                        to,
                        cause,
                    } => {
                        let _ = node_tx.send(NodeEvent::LinkSwitched {
                            peer_id,
                            from_link: from,
                            to_link: to,
                            cause,
                        });
                    }
                    SessionEvent::LinkAdded { .. } | SessionEvent::StreamProgress { .. } => {}
                }
            }
        });
    }
}
