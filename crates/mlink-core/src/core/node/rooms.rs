use super::Node;

impl Node {
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

    pub fn room_hashes(&self) -> std::collections::HashSet<[u8; 8]> {
        self.room_hashes.lock().expect("room_hashes poisoned").clone()
    }

    /// First room hash we'll advertise on the wire. The Handshake protocol
    /// still carries a single `Option<[u8;8]>`, so when we belong to multiple
    /// rooms we just pick one — the peer will verify it against *its* set.
    pub(super) fn wire_room_hash(&self) -> Option<[u8; 8]> {
        self.room_hashes
            .lock()
            .expect("room_hashes poisoned")
            .iter()
            .next()
            .copied()
    }
}
