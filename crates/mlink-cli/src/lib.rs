use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "mlink", about = "Multi-to-multi local device connection layer")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Scan for nearby mlink devices
    Scan,
    /// Connect to a peer by id
    Connect { peer_id: String },
    /// Send a heartbeat and measure round-trip time
    Ping { peer_id: String },
    /// Send a MESSAGE frame to a peer
    Send { peer_id: String, message: String },
    /// Show node state and connected peers
    Status,
    /// Manage trusted peers
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Diagnose BLE adapter and environment
    Doctor,
}

#[derive(Subcommand, Debug)]
pub enum TrustAction {
    /// List trusted peers
    List,
    /// Remove a trusted peer by app_uuid
    Remove { peer_id: String },
}
