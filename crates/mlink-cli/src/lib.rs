use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Link-layer selector for the `--transport` flag. Kept in the library crate
/// so both the binary and integration tests can exercise the string→enum
/// parse logic without shelling out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Ble,
    Tcp,
}

impl TransportKind {
    /// Parse the raw flag value. Returns an error string (not MlinkError, to
    /// avoid pulling the core crate into the public signature) ready to be
    /// printed via `eprintln!("error: {msg}")`.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "ble" => Ok(TransportKind::Ble),
            "tcp" => Ok(TransportKind::Tcp),
            other => Err(format!(
                "unknown --transport {other:?}: expected \"ble\" or \"tcp\""
            )),
        }
    }
}

/// Validate a 6-digit room code. Shared between the binary entrypoint and
/// the integration tests so we exercise the real rule, not a copy.
pub fn validate_room_code(code: &str) -> Result<(), String> {
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "invalid room code '{code}': expected 6 digits"
        ));
    }
    Ok(())
}

#[derive(Parser, Debug)]
#[command(
    name = "mlink",
    about = "Multi-to-multi local device connection layer",
    after_help = "Quick start:\n  \
      mlink            # generate a new 6-digit room and start serving\n  \
      mlink 482193     # join an existing room by its 6-digit code\n  \
      mlink send 482193 hello"
)]
pub struct Cli {
    /// 6-digit room code. Omit to generate a new one. Ignored if a subcommand is given.
    pub code: Option<String>,

    /// Link-layer transport: "ble" (default) or "tcp".
    #[arg(long, default_value = "ble", global = true)]
    pub transport: String,

    #[command(subcommand)]
    pub command: Option<Commands>,
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
    /// Interactive chat: join a room, print incoming messages, broadcast each stdin line
    Chat {
        /// 6-digit room code
        code: String,
    },

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
