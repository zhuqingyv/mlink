//! `async fn run(cli)` — the command dispatcher invoked from `main`. Pure
//! match on `Commands::*` with a fallback "no subcommand" path that
//! generates (or adopts) a 6-digit room code and boots `cmd_serve`.
//!
//! `Commands::Daemon` and `Commands::Dev` are handled before we get here
//! (see `main::main`) because they own their own ctrl-c + cleanup path; the
//! dispatch entries below exist only to satisfy the exhaustive match.

use mlink_cli::{Cli, Commands, TransportKind};
use mlink_core::core::room::generate_room_code;
use mlink_core::protocol::errors::MlinkError;

use crate::commands;
use crate::node_build::validate_room_code;

pub(crate) async fn run(cli: Cli) -> Result<(), MlinkError> {
    let kind = TransportKind::parse(&cli.transport).map_err(MlinkError::HandlerError)?;
    match cli.command {
        Some(Commands::Serve) => commands::serve::cmd_serve(None, kind).await,
        Some(Commands::Room { action }) => commands::room::cmd_room(action, kind).await,
        Some(Commands::Send { code, file, message }) => {
            commands::send::cmd_send_room(code, file, message, kind).await
        }
        Some(Commands::Listen) => commands::listen::cmd_listen(kind).await,
        Some(Commands::Chat { code }) => commands::chat::cmd_chat(code, kind).await,
        Some(Commands::Join { code, chat }) => commands::join::cmd_join(code, chat, kind).await,

        Some(Commands::Scan) => commands::scan::cmd_scan(kind).await,
        Some(Commands::Connect { peer_id }) => commands::connect::cmd_connect(peer_id, kind).await,
        Some(Commands::Ping { peer_id }) => commands::ping::cmd_ping(peer_id, kind).await,
        Some(Commands::Status) => commands::status::cmd_status().await,
        Some(Commands::Trust { action }) => commands::trust::cmd_trust(action).await,
        Some(Commands::Doctor) => commands::doctor::cmd_doctor(kind).await,
        Some(Commands::Daemon) => commands::daemon::cmd_daemon().await,
        // Unreachable in practice — `mlink dev` is dispatched before the
        // outer ctrl-c guard in `main()` so it owns its own teardown path.
        // Kept here only to satisfy the exhaustive match.
        Some(Commands::Dev) => commands::dev::cmd_dev()
            .await
            .map_err(|e| MlinkError::HandlerError(format!("dev: {e}"))),

        // No subcommand → one-shot "join or create a room" mode.
        None => {
            let code = match cli.code {
                Some(c) => {
                    validate_room_code(&c)?;
                    println!("[mlink] joining room {c}");
                    c
                }
                None => {
                    let c = generate_room_code();
                    println!("[mlink] room created: {c}");
                    println!("[mlink] share this code with other devices, then run:");
                    println!("        mlink {c}");
                    c
                }
            };
            commands::serve::cmd_serve(Some(code), kind).await
        }
    }
}
