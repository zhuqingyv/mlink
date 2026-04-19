use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "mlink", about = "Multi-to-multi local device connection layer")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run mlink as a long-lived node (BLE advertise + scan, prints received messages)
    Serve,
    /// Manage rooms (6-digit code)
    Room {
        #[command(subcommand)]
        action: RoomAction,
    },
    /// Send a message or a file to everyone in a room
    Send {
        /// 6-digit room code
        code: String,
        /// Send a file; if set, the positional message is ignored
        #[arg(long)]
        file: Option<PathBuf>,
        /// Text message (required unless --file is set)
        message: Option<String>,
    },
    /// Listen on all joined rooms and print messages as they arrive
    Listen,

    // ---- legacy peer-id commands (still supported) --------------------------
    /// Scan for nearby mlink devices
    Scan,
    /// Connect to a peer by id
    Connect { peer_id: String },
    /// Send a heartbeat and measure round-trip time
    Ping { peer_id: String },
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
pub enum RoomAction {
    /// Create a new room, print its 6-digit code, and serve the room
    New,
    /// Join an existing room by 6-digit code
    Join { code: String },
    /// Leave a joined room
    Leave { code: String },
    /// List all joined rooms and peer counts
    List,
    /// Show peers currently in the given room
    Peers { code: String },
}

#[derive(Subcommand, Debug)]
pub enum TrustAction {
    /// List trusted peers
    List,
    /// Remove a trusted peer by app_uuid
    Remove { peer_id: String },
}
