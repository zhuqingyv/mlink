pub mod connection;
pub mod link;
pub mod node;
pub mod peer;
pub mod reconnect;
pub mod room;
pub mod scanner;
pub mod security;
pub mod session;

pub use connection::{negotiate_role, perform_handshake, ConnectionManager, Role};
pub use link::{Link, LinkHealth, TransportKind};
pub use node::{Node, NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT};
pub use peer::{generate_app_uuid, load_or_create_app_uuid, Peer, PeerManager};
pub use scanner::{Scanner, DEFAULT_SCAN_INTERVAL};
pub use security::{
    derive_aes_key, derive_shared_secret, derive_verification_code, encrypt, decrypt,
    generate_keypair, KeyPair, TrustStore, TrustedPeer,
};
