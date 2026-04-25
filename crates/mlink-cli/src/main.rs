use std::process::ExitCode;

use clap::Parser;
use mlink_cli::{Cli, Commands};
use tokio::signal;

mod commands;
mod node_build;
mod run;
mod session;
mod util;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .init();

    let cli = Cli::parse();

    // `mlink daemon` wants graceful shutdown so it can clean up
    // `~/.mlink/daemon.json` — the outer `_exit(0)` path below would skip that.
    // Dispatch straight to `mlink_daemon::run()`, which owns its own ctrl-c
    // select and runs `remove_daemon_info` before returning.
    if matches!(cli.command, Some(Commands::Daemon)) {
        return match mlink_daemon::run().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: daemon: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // `mlink dev` = daemon + open the debug page in a browser. Same cleanup
    // path as `daemon`, so we also dispatch before the outer `_exit` guard.
    if matches!(cli.command, Some(Commands::Dev)) {
        return match commands::dev::cmd_dev().await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: dev: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Outer Ctrl+C guard. Long-running subcommands (serve/chat/join) each have
    // their own `select!` on `signal::ctrl_c()` for a graceful goodbye, but a
    // handful of paths in the transport layer can wedge cancellation — the BLE
    // FFI drop path has swallowed SIGINT in the wild, and the mDNS
    // `spawn_blocking(browse)` holds an OS thread for up to 3s before the
    // runtime can unwind. To keep `Ctrl+C` feeling instant no matter what, we
    // race the command future against ctrl-c at the top and short-circuit via
    // `libc::_exit` the moment the signal arrives. Any child tasks,
    // spawn_blocking workers, or Obj-C objects get cleaned up by the OS on
    // process teardown — safe for a one-shot CLI.
    tokio::select! {
        // `biased` makes the ctrl-c arm poll first each tick, so a SIGINT
        // arriving while `run` is still inside one of its own `select!`s wins
        // unconditionally — we don't want to race the inner ctrl-c arm into a
        // slow `node.stop()` + runtime drop teardown that the whole point of
        // this outer guard is to bypass.
        biased;
        _ = signal::ctrl_c() => {
            println!("\n[mlink] bye");
            // Flush stdout so the goodbye line actually lands before exit —
            // pipes are block-buffered, and `_exit` skips libc cleanup that
            // would otherwise flush us.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            // `std::process::exit` runs atexit handlers and drops the Tokio
            // runtime, which waits for spawn_blocking workers (the mDNS
            // daemon loop in particular holds a thread for up to 3s). Hop
            // straight to `_exit` to give Ctrl+C an immediate response —
            // OS resource reclamation handles the rest.
            unsafe { libc::_exit(0) };
        }
        result = run::run(cli) => match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}
