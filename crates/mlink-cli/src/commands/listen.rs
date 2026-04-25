//! `mlink listen` — "run forever, print messages". Identical to `serve`
//! without a room code, so we delegate straight to `cmd_serve(None, kind)`.

use mlink_core::protocol::errors::MlinkError;

use super::serve::cmd_serve;
use mlink_cli::TransportKind;

pub(crate) async fn cmd_listen(kind: TransportKind) -> Result<(), MlinkError> {
    println!("[mlink] listen: running serve loop, Ctrl+C to quit");
    cmd_serve(None, kind).await
}
