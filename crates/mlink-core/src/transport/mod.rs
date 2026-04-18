#[path = "trait.rs"]
mod transport_trait;

pub mod ble;
pub mod ipc;
pub mod mock;

pub use transport_trait::{Connection, DiscoveredPeer, Transport, TransportCapabilities};
