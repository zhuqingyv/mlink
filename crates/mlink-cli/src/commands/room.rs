//! `mlink room <action>` — room management. `New` and `Join` create / adopt
//! a 6-digit code and delegate to `cmd_serve`; `Leave`, `List`, `Peers`
//! operate on a freshly-built `RoomManager` which is stateless across CLI
//! invocations (the printed notes reflect that).

use mlink_core::core::room::{generate_room_code, RoomManager};
use mlink_core::protocol::errors::MlinkError;

use mlink_cli::{RoomAction, TransportKind};

use super::serve::cmd_serve;
use crate::node_build::validate_room_code;

pub(crate) async fn cmd_room(action: RoomAction, kind: TransportKind) -> Result<(), MlinkError> {
    match action {
        RoomAction::New => {
            let code = generate_room_code();
            println!("[mlink] room created: {code}");
            println!("[mlink] share this code with other devices, then run:");
            println!("        mlink room join {code}");
            cmd_serve(Some(code), kind).await
        }
        RoomAction::Join { code } => {
            validate_room_code(&code)?;
            println!("[mlink] joining room {code}");
            cmd_serve(Some(code), kind).await
        }
        RoomAction::Leave { code } => {
            validate_room_code(&code)?;
            // CLI is stateless — a persistent daemon is not yet wired up,
            // so "leave" only makes sense for the in-process RoomManager
            // owned by `serve`. We print a hint and succeed.
            let mut manager = RoomManager::new();
            manager.join(&code);
            manager.leave(&code);
            println!("[mlink] left room {code}");
            println!(
                "note: CLI invocations are stateless — stop `mlink serve` to leave a live room"
            );
            Ok(())
        }
        RoomAction::List => {
            let manager = RoomManager::new();
            let rooms = manager.list();
            if rooms.is_empty() {
                println!("no rooms joined in this CLI invocation");
                println!("note: run `mlink serve` or `mlink room join <code>` to join a room");
                return Ok(());
            }
            println!("{:<10}  {}", "CODE", "PEERS");
            for r in rooms {
                println!("{:<10}  {}", r.code, r.peers.len());
            }
            Ok(())
        }
        RoomAction::Peers { code } => {
            validate_room_code(&code)?;
            let manager = RoomManager::new();
            let peers = manager.peers(&code);
            if peers.is_empty() {
                println!("no peers in room {code}");
                println!(
                    "note: CLI invocations are stateless — run `mlink room join {code}` to see live peers"
                );
                return Ok(());
            }
            println!("peers in room {code} ({}):", peers.len());
            for p in peers {
                println!("  - {p}");
            }
            Ok(())
        }
    }
}
