//! Standalone daemon binary. `mlink daemon` (from mlink-cli) execs this via
//! `mlink_daemon::run()`; running this binary directly also works for local
//! development.

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    match mlink_daemon::run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mlink-daemon: {e}");
            ExitCode::FAILURE
        }
    }
}
