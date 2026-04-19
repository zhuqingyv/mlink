#[path = "trait.rs"]
mod transport_trait;

pub mod ble;
#[cfg(unix)]
pub mod ipc;
pub mod mock;
pub mod tcp;

#[cfg(target_os = "macos")]
pub mod peripheral;

pub use transport_trait::{Connection, DiscoveredPeer, Transport, TransportCapabilities};
