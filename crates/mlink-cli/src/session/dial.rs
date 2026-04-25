//! Dial / accept plumbing shared by serve/chat/join. Each helper wraps the
//! Node-layer call in the shared `CONNECT_TIMEOUT`, converts timeouts to
//! `HandlerError`, and — for failed dials — schedules a backoff before
//! asking the scanner to resurface the wire id.

use mlink_core::core::node::Node;
use mlink_core::protocol::errors::MlinkError;
use mlink_core::transport::{Connection, DiscoveredPeer, Transport};
use tokio::sync::mpsc;

use crate::util::backoff::{random_backoff, CONNECT_TIMEOUT};

pub(crate) async fn dial_with_timeout(
    node: &Node,
    transport: &mut dyn Transport,
    peer: &DiscoveredPeer,
) -> Result<String, MlinkError> {
    let wire_id = peer.id.clone();
    let result = tokio::time::timeout(CONNECT_TIMEOUT, node.connect_peer(transport, peer)).await;
    match result {
        Ok(inner) => inner,
        Err(_) => Err(MlinkError::HandlerError(format!(
            "connect to {} timed out after {:?}",
            wire_id, CONNECT_TIMEOUT
        ))),
    }
}

pub(crate) async fn accept_with_timeout(
    node: &Node,
    conn: Box<dyn Connection>,
    label: &'static str,
    wire_id: String,
) -> Result<String, MlinkError> {
    let result =
        tokio::time::timeout(CONNECT_TIMEOUT, node.accept_incoming(conn, label, wire_id.clone()))
            .await;
    match result {
        Ok(inner) => inner,
        Err(_) => Err(MlinkError::HandlerError(format!(
            "accept from {} timed out after {:?}",
            wire_id, CONNECT_TIMEOUT
        ))),
    }
}

/// Random-backoff "ask the scanner to forget about this wire id so we can
/// retry next round". Spawns a detached task so the main select! loop
/// doesn't block waiting for the delay.
pub(crate) fn schedule_retry_unsee(unsee_tx: mpsc::Sender<String>, wire_id: String) {
    let delay = random_backoff();
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = unsee_tx.send(wire_id).await;
    });
}
