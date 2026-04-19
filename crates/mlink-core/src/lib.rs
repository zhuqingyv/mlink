//! mlink-core: cross-device BLE + IPC mesh link library.
//!
//! Top-level re-exports expose the commonly used types so that
//! downstream users can write `use mlink_core::{Node, NodeConfig, Peer, MlinkError, ...};`.
//! Deeper or less-common items remain reachable through their modules
//! (`mlink_core::core::*`, `mlink_core::protocol::*`, etc.).

pub mod api;
pub mod core;
pub mod protocol;
pub mod transport;

// ---- core ------------------------------------------------------------------
pub use crate::core::connection::{negotiate_role, perform_handshake, ConnectionManager, Role};
pub use crate::core::node::{
    Node, NodeConfig, NodeEvent, NodeState, PeerState, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT,
};
pub use crate::core::peer::{generate_app_uuid, load_or_create_app_uuid, Peer, PeerManager};
pub use crate::core::reconnect::{ReconnectMode, ReconnectPolicy};
pub use crate::core::scanner::{Scanner, DEFAULT_SCAN_INTERVAL};
pub use crate::core::security::{
    derive_aes_key, derive_shared_secret, derive_verification_code, decrypt, encrypt,
    generate_keypair, KeyPair, TrustStore, TrustedPeer,
};

// ---- protocol --------------------------------------------------------------
pub use crate::protocol::codec::{decode, encode};
pub use crate::protocol::errors::{MlinkError, Result};
pub use crate::protocol::frame::{decode_frame, encode_frame};
pub use crate::protocol::types::{
    decode_flags, encode_flags, CtrlCommand, ErrorCode, Frame, Handshake, InvalidMessageType,
    MessageType, Priority, StreamInfo, StreamResumeInfo, HEADER_SIZE, MAGIC, PROTOCOL_VERSION,
};
// Keep compress as a module path to avoid polluting the top-level namespace.
pub use crate::protocol::compress;

// ---- transport -------------------------------------------------------------
pub use crate::transport::{Connection, DiscoveredPeer, Transport, TransportCapabilities};
pub use crate::transport::ble::BleTransport;
#[cfg(unix)]
pub use crate::transport::ipc::IpcTransport;
pub use crate::transport::mock::{mock_pair, MockConnection, MockTransport};

// ---- api -------------------------------------------------------------------
pub use crate::api::message::{broadcast_message, recv_message, send_message};
pub use crate::api::pubsub::{publish, PubSubManager, PubSubMessage};
pub use crate::api::rpc::{
    decode_request, decode_response, dispatch_request, rpc_request, send_request, send_response,
    BoxedHandler, PendingRequests, RpcRegistry, RpcRequest, RpcResponse, RPC_STATUS_ERROR,
    RPC_STATUS_OK,
};
pub use crate::api::stream::{
    create_stream, create_stream_with_chunk_size, next_stream_id_central,
    next_stream_id_peripheral, Progress, StreamReader, StreamWriter,
};
