use std::time::{Duration, Instant};

use crate::core::reconnect::ReconnectPolicy;

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
    pub trust_store_path: Option<std::path::PathBuf>,
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
    pub(super) fn new() -> Self {
        Self {
            state: NodeState::Connecting,
            reconnect_policy: ReconnectPolicy::new(),
            seq: 0,
            last_heartbeat: None,
            aes_key: None,
        }
    }

    pub(super) fn next_seq(&mut self) -> u16 {
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
