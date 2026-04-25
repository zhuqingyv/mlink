//! `mlink status` — one-shot dump of the local node's state, identity, and
//! the peers currently held by the in-process Node. Because the CLI is
//! stateless this only reflects the freshly-built Node; the user needs a
//! long-lived `mlink serve` (or the daemon) to see live peers.

use mlink_core::protocol::errors::MlinkError;

use crate::node_build::build_node;

pub(crate) async fn cmd_status() -> Result<(), MlinkError> {
    let node = build_node().await?;
    println!("state: {:?}", node.state());
    println!("app_uuid: {}", node.app_uuid());
    let peers = node.peers().await;
    println!("connected peers: {}", peers.len());
    for p in peers {
        println!("  - {} ({}) transport={}", p.id, p.name, p.transport_id);
    }
    println!("note: CLI invocations are stateless — run `mlink serve` in a long-lived process to see peers here");
    Ok(())
}
