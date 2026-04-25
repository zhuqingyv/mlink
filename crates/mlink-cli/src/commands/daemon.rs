//! Defers all the heavy lifting to `mlink_daemon::run()` which owns the
//! Node, single-instance lock, and WS server. We just translate its error
//! into `MlinkError::HandlerError` so the CLI's error printing path is
//! uniform.

use mlink_core::protocol::errors::MlinkError;

pub(crate) async fn cmd_daemon() -> Result<(), MlinkError> {
    mlink_daemon::run()
        .await
        .map_err(|e| MlinkError::HandlerError(format!("daemon: {e}")))
}
