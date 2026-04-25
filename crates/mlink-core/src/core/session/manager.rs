use std::collections::HashMap;
use std::sync::Arc;

use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::Mutex;

use crate::core::link::Link;
use crate::core::session::types::{
    LinkRole, LinkStatus, Session, SessionEvent, MAX_LINKS_PER_PEER,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    CreatedNew { session_id: [u8; 16] },
    AttachedExisting,
    RejectedDuplicateKind,
    RejectedCapacity,
    RejectedSessionMismatch,
}

pub struct SessionManager {
    peers: Mutex<HashMap<String, Arc<Session>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self { peers: Mutex::new(HashMap::new()) }
    }

    pub async fn attach_link(
        &self,
        peer_id: &str,
        link: Link,
        session_id_hint: Option<[u8; 16]>,
    ) -> AttachOutcome {
        let mut peers = self.peers.lock().await;
        if let Some(session) = peers.get(peer_id).cloned() {
            if session_id_hint.is_some_and(|h| h != session.session_id) {
                return AttachOutcome::RejectedSessionMismatch;
            }
            let mut links = session.links.write().await;
            if links.len() >= MAX_LINKS_PER_PEER {
                return AttachOutcome::RejectedCapacity;
            }
            if links.iter().any(|l| l.kind() == link.kind()) {
                return AttachOutcome::RejectedDuplicateKind;
            }
            let (lid, kind) = (link.id().to_string(), link.kind());
            links.push(Arc::new(link));
            drop(links);
            let _ = session
                .events
                .send(SessionEvent::LinkAdded { peer_id: peer_id.to_string(), link_id: lid, kind });
            AttachOutcome::AttachedExisting
        } else {
            // first link on unknown peer cannot claim an existing id
            if session_id_hint.is_some() {
                return AttachOutcome::RejectedSessionMismatch;
            }
            let session_id = fresh_session_id();
            let (lid, kind) = (link.id().to_string(), link.kind());
            let session = new_session(peer_id, session_id, Arc::new(link), lid.clone()).await;
            let tx = session.events.clone();
            peers.insert(peer_id.to_string(), session);
            let _ = tx.send(SessionEvent::LinkAdded {
                peer_id: peer_id.to_string(),
                link_id: lid,
                kind,
            });
            AttachOutcome::CreatedNew { session_id }
        }
    }

    pub async fn get(&self, peer_id: &str) -> Option<Arc<Session>> {
        self.peers.lock().await.get(peer_id).cloned()
    }

    pub async fn contains(&self, peer_id: &str) -> bool {
        self.peers.lock().await.contains_key(peer_id)
    }

    /// Drop a whole peer; close every link; return the detached Session.
    pub async fn drop_peer(&self, peer_id: &str) -> Option<Arc<Session>> {
        let session = self.peers.lock().await.remove(peer_id)?;
        for link in session.links.read().await.iter() {
            link.close().await;
        }
        Some(session)
    }

    /// Detach a single link. Returns `true` if the peer was fully removed.
    pub async fn detach_link(&self, peer_id: &str, link_id: &str) -> bool {
        let Some(session) = self.peers.lock().await.get(peer_id).cloned() else {
            return false;
        };
        let removed = {
            let mut links = session.links.write().await;
            links
                .iter()
                .position(|l| l.id() == link_id)
                .map(|pos| links.remove(pos))
        };
        let Some(link) = removed else { return false };
        link.close().await;
        let _ = session.events.send(SessionEvent::LinkRemoved {
            peer_id: peer_id.to_string(),
            link_id: link_id.to_string(),
            reason: "detach".to_string(),
        });
        {
            let mut active = session.active.write().await;
            if active.as_deref() == Some(link_id) {
                *active = None;
            }
        }
        if session.links.read().await.is_empty() {
            self.peers.lock().await.remove(peer_id);
            return true;
        }
        false
    }

    pub async fn list_status(&self) -> Vec<(String, Vec<LinkStatus>)> {
        let snap: Vec<_> = self.peers.lock().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut out = Vec::with_capacity(snap.len());
        for (pid, s) in snap {
            out.push((pid, status_of(&s).await));
        }
        out
    }

    pub async fn peer_link_status(&self, peer_id: &str) -> Vec<LinkStatus> {
        match self.get(peer_id).await {
            Some(s) => status_of(&s).await,
            None => Vec::new(),
        }
    }

    pub async fn peers(&self) -> Vec<String> {
        self.peers.lock().await.keys().cloned().collect()
    }

    pub async fn count(&self) -> usize {
        self.peers.lock().await.len()
    }
}

impl Default for SessionManager {
    fn default() -> Self { Self::new() }
}

async fn new_session(
    peer_id: &str,
    session_id: [u8; 16],
    first_link: Arc<Link>,
    active_id: String,
) -> Arc<Session> {
    let s = Session::new(peer_id.to_string(), session_id);
    s.links.write().await.push(first_link);
    *s.active.write().await = Some(active_id);
    Arc::new(s)
}

async fn status_of(session: &Arc<Session>) -> Vec<LinkStatus> {
    let links = session.links.read().await.clone();
    let active = session.active.read().await.clone();
    links.iter().map(|l| LinkStatus {
        link_id: l.id().to_string(),
        kind: l.kind(),
        role: if active.as_deref() == Some(l.id()) { LinkRole::Active } else { LinkRole::Standby },
        health: l.health(),
    }).collect()
}

fn fresh_session_id() -> [u8; 16] {
    let rng = SystemRandom::new();
    let mut buf = [0u8; 16];
    rng.fill(&mut buf).expect("SystemRandom fill 16 bytes");
    buf
}

#[cfg(test)]
#[path = "manager_tests.rs"]
mod tests;
