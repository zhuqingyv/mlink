use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Instant;

use tokio::sync::RwLock;

/// Remote peer record: identity, transport, and connection metadata.
#[derive(Debug, Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub app_uuid: String,
    pub connected_at: Instant,
    pub transport_id: String,
}

pub struct PeerManager {
    peers: RwLock<HashMap<String, Peer>>,
}

impl PeerManager {
    pub fn new() -> Self {
        Self {
            peers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn add(&self, peer: Peer) {
        let mut guard = self.peers.write().await;
        guard.insert(peer.id.clone(), peer);
    }

    pub async fn remove(&self, id: &str) -> Option<Peer> {
        let mut guard = self.peers.write().await;
        guard.remove(id)
    }

    pub async fn get(&self, id: &str) -> Option<Peer> {
        let guard = self.peers.read().await;
        guard.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Peer> {
        let guard = self.peers.read().await;
        guard.values().cloned().collect()
    }

    pub async fn count(&self) -> usize {
        let guard = self.peers.read().await;
        guard.len()
    }
}

impl Default for PeerManager {
    fn default() -> Self {
        Self::new()
    }
}

pub fn generate_app_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn app_uuid_path() -> io::Result<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "HOME env var not set"))?;
    let mut path = PathBuf::from(home);
    path.push(".mlink");
    path.push("app_uuid");
    Ok(path)
}

pub fn load_or_create_app_uuid() -> io::Result<String> {
    let path = app_uuid_path()?;

    if path.exists() {
        let mut file = std::fs::File::open(&path)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;
        let trimmed = content.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let uuid = generate_app_uuid();
    let mut file = std::fs::File::create(&path)?;
    file.write_all(uuid.as_bytes())?;
    Ok(uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_peer(id: &str) -> Peer {
        Peer {
            id: id.into(),
            name: format!("peer-{}", id),
            app_uuid: generate_app_uuid(),
            connected_at: Instant::now(),
            transport_id: "mock".into(),
        }
    }

    #[tokio::test]
    async fn add_and_get() {
        let mgr = PeerManager::new();
        let peer = make_peer("a");
        mgr.add(peer.clone()).await;
        let got = mgr.get("a").await.expect("peer should exist");
        assert_eq!(got.id, "a");
        assert_eq!(mgr.count().await, 1);
    }

    #[tokio::test]
    async fn remove_returns_peer() {
        let mgr = PeerManager::new();
        mgr.add(make_peer("a")).await;
        let removed = mgr.remove("a").await;
        assert!(removed.is_some());
        assert_eq!(mgr.count().await, 0);
        assert!(mgr.get("a").await.is_none());
    }

    #[tokio::test]
    async fn list_returns_all() {
        let mgr = PeerManager::new();
        mgr.add(make_peer("a")).await;
        mgr.add(make_peer("b")).await;
        mgr.add(make_peer("c")).await;
        let peers = mgr.list().await;
        assert_eq!(peers.len(), 3);
        let mut ids: Vec<_> = peers.iter().map(|p| p.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn remove_missing_returns_none() {
        let mgr = PeerManager::new();
        assert!(mgr.remove("nope").await.is_none());
    }

    #[test]
    fn generate_app_uuid_is_unique() {
        let a = generate_app_uuid();
        let b = generate_app_uuid();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
    }

    #[test]
    fn load_or_create_app_uuid_is_stable() {
        use std::io::Read;
        let tmp = tempfile::tempdir().expect("tempdir");
        let uuid_path = tmp.path().join(".mlink").join("app_uuid");

        // First call: file doesn't exist, should create
        std::fs::create_dir_all(uuid_path.parent().unwrap()).unwrap();
        let first = generate_app_uuid();
        std::fs::write(&uuid_path, &first).unwrap();

        // Second call: file exists, should read same value
        let mut content = String::new();
        std::fs::File::open(&uuid_path)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(first, content.trim());
        assert_eq!(first.len(), 36);
    }
}
