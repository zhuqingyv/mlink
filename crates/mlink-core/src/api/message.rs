use crate::core::node::Node;
use crate::protocol::errors::Result;
use crate::protocol::types::MessageType;

pub async fn send_message(node: &Node, peer_id: &str, data: &[u8]) -> Result<()> {
    node.send_raw(peer_id, MessageType::Message, data).await
}

pub async fn broadcast_message(node: &Node, data: &[u8]) -> Result<()> {
    for peer in node.peers().await {
        node.send_raw(&peer.id, MessageType::Message, data).await?;
    }
    Ok(())
}

pub async fn recv_message(node: &Node, peer_id: &str) -> Result<Vec<u8>> {
    loop {
        let (frame, payload) = node.recv_raw(peer_id).await?;
        let (_c, _e, mt) = crate::protocol::types::decode_flags(frame.flags);
        if mt == MessageType::Message {
            return Ok(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::node::{Node, NodeConfig};
    use crate::protocol::errors::MlinkError;

    async fn make_node() -> (Node, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tmp");
        let cfg = NodeConfig {
            name: "msg-test".into(),
            encrypt: false,
            trust_store_path: Some(tmp.path().join("t.json")),
        };
        let node = Node::new(cfg).await.expect("node");
        (node, tmp)
    }

    #[tokio::test]
    async fn send_message_missing_peer_errors() {
        let (node, _tmp) = make_node().await;
        let err = send_message(&node, "ghost", b"hi").await.unwrap_err();
        assert!(matches!(err, MlinkError::PeerGone { .. }));
    }

    #[tokio::test]
    async fn broadcast_empty_peers_is_ok() {
        let (node, _tmp) = make_node().await;
        broadcast_message(&node, b"data").await.expect("ok");
    }
}
