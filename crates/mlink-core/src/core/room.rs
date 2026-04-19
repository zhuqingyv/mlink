use std::collections::HashMap;
use std::time::Instant;

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

const ROOM_HASH_INFO: &[u8] = b"mlink-room";
const ROOM_APP_ID_PREFIX: &str = "mlink-room-id-";

pub fn generate_room_code() -> String {
    let rng = SystemRandom::new();
    let mut buf = [0u8; 4];
    rng.fill(&mut buf).expect("SystemRandom fill 4 bytes");
    let n = u32::from_be_bytes(buf) % 1_000_000;
    format!("{:06}", n)
}

pub fn room_hash(code: &str) -> [u8; 8] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, code.as_bytes());
    let tag = hmac::sign(&key, ROOM_HASH_INFO);
    let mut out = [0u8; 8];
    out.copy_from_slice(&tag.as_ref()[..8]);
    out
}

pub fn room_app_id(app_uuid: &str, code: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, app_uuid.as_bytes());
    let data = format!("{}{}", ROOM_APP_ID_PREFIX, code);
    let tag = hmac::sign(&key, data.as_bytes());
    let bytes = &tag.as_ref()[..16];
    let mut hex = String::with_capacity(32);
    for b in bytes {
        hex.push_str(&format!("{:02x}", b));
    }
    hex
}

#[derive(Debug, Clone)]
pub struct RoomState {
    pub code: String,
    pub hash: [u8; 8],
    pub joined_at: Instant,
    pub peers: Vec<String>,
}

#[derive(Debug, Default)]
pub struct RoomManager {
    rooms: HashMap<String, RoomState>,
}

impl RoomManager {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
        }
    }

    pub fn join(&mut self, code: &str) -> RoomState {
        let state = RoomState {
            code: code.to_string(),
            hash: room_hash(code),
            joined_at: Instant::now(),
            peers: Vec::new(),
        };
        self.rooms.insert(code.to_string(), state.clone());
        state
    }

    pub fn leave(&mut self, code: &str) -> Option<RoomState> {
        self.rooms.remove(code)
    }

    pub fn list(&self) -> Vec<&RoomState> {
        self.rooms.values().collect()
    }

    pub fn peers(&self, code: &str) -> Vec<String> {
        self.rooms
            .get(code)
            .map(|r| r.peers.clone())
            .unwrap_or_default()
    }

    pub fn is_joined(&self, code: &str) -> bool {
        self.rooms.contains_key(code)
    }

    pub fn active_hashes(&self) -> Vec<[u8; 8]> {
        self.rooms.values().map(|r| r.hash).collect()
    }

    pub fn add_peer(&mut self, code: &str, peer_id: &str) {
        if let Some(room) = self.rooms.get_mut(code) {
            if !room.peers.iter().any(|p| p == peer_id) {
                room.peers.push(peer_id.to_string());
            }
        }
    }

    pub fn remove_peer(&mut self, code: &str, peer_id: &str) {
        if let Some(room) = self.rooms.get_mut(code) {
            room.peers.retain(|p| p != peer_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_room_code_is_six_digits() {
        for _ in 0..50 {
            let c = generate_room_code();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|ch| ch.is_ascii_digit()));
        }
    }

    #[test]
    fn generate_room_code_preserves_leading_zeros() {
        let c = "000042".to_string();
        assert_eq!(c.len(), 6);
        let formatted = format!("{:06}", 42u32);
        assert_eq!(formatted, "000042");
    }

    #[test]
    fn room_hash_is_deterministic() {
        let a = room_hash("123456");
        let b = room_hash("123456");
        assert_eq!(a, b);
    }

    #[test]
    fn room_hash_differs_per_code() {
        let a = room_hash("111111");
        let b = room_hash("222222");
        assert_ne!(a, b);
    }

    #[test]
    fn room_hash_is_eight_bytes() {
        let h = room_hash("000000");
        assert_eq!(h.len(), 8);
    }

    #[test]
    fn room_app_id_is_32_hex_chars() {
        let id = room_app_id("my-uuid", "123456");
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn room_app_id_is_deterministic() {
        let a = room_app_id("uuid-x", "654321");
        let b = room_app_id("uuid-x", "654321");
        assert_eq!(a, b);
    }

    #[test]
    fn room_app_id_differs_per_uuid() {
        let a = room_app_id("uuid-a", "123456");
        let b = room_app_id("uuid-b", "123456");
        assert_ne!(a, b);
    }

    #[test]
    fn room_app_id_differs_per_code() {
        let a = room_app_id("uuid-x", "111111");
        let b = room_app_id("uuid-x", "222222");
        assert_ne!(a, b);
    }

    #[test]
    fn manager_join_and_is_joined() {
        let mut m = RoomManager::new();
        assert!(!m.is_joined("123456"));
        let state = m.join("123456");
        assert_eq!(state.code, "123456");
        assert_eq!(state.hash, room_hash("123456"));
        assert!(state.peers.is_empty());
        assert!(m.is_joined("123456"));
    }

    #[test]
    fn manager_leave_returns_state() {
        let mut m = RoomManager::new();
        m.join("123456");
        let left = m.leave("123456");
        assert!(left.is_some());
        assert!(!m.is_joined("123456"));
        assert!(m.leave("123456").is_none());
    }

    #[test]
    fn manager_list_returns_all_rooms() {
        let mut m = RoomManager::new();
        m.join("111111");
        m.join("222222");
        assert_eq!(m.list().len(), 2);
    }

    #[test]
    fn manager_active_hashes_matches_rooms() {
        let mut m = RoomManager::new();
        m.join("111111");
        m.join("222222");
        let hashes = m.active_hashes();
        assert_eq!(hashes.len(), 2);
        assert!(hashes.contains(&room_hash("111111")));
        assert!(hashes.contains(&room_hash("222222")));
    }

    #[test]
    fn manager_add_and_remove_peer() {
        let mut m = RoomManager::new();
        m.join("123456");
        m.add_peer("123456", "peer-1");
        m.add_peer("123456", "peer-2");
        m.add_peer("123456", "peer-1");
        let peers = m.peers("123456");
        assert_eq!(peers.len(), 2);

        m.remove_peer("123456", "peer-1");
        let peers = m.peers("123456");
        assert_eq!(peers, vec!["peer-2".to_string()]);
    }

    #[test]
    fn manager_peers_on_missing_room_empty() {
        let m = RoomManager::new();
        assert!(m.peers("nope").is_empty());
    }

    #[test]
    fn manager_add_peer_on_missing_room_is_noop() {
        let mut m = RoomManager::new();
        m.add_peer("nope", "peer-1");
        assert!(m.peers("nope").is_empty());
    }
}
