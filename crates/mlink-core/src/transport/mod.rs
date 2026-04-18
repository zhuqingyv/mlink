#[path = "trait.rs"]
mod transport_trait;

pub use transport_trait::{Connection, DiscoveredPeer, Transport, TransportCapabilities};
