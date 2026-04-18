use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::node::Node;
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::types::MessageType;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PubSubMessage {
    pub topic: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct PubSubManager {
    subscriptions: HashMap<String, Vec<String>>,
}

impl PubSubManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&mut self, topic: &str, peer_id: &str) {
        let list = self
            .subscriptions
            .entry(topic.to_string())
            .or_insert_with(Vec::new);
        if !list.iter().any(|p| p == peer_id) {
            list.push(peer_id.to_string());
        }
    }

    pub fn unsubscribe(&mut self, topic: &str, peer_id: &str) {
        if let Some(list) = self.subscriptions.get_mut(topic) {
            list.retain(|p| p != peer_id);
            if list.is_empty() {
                self.subscriptions.remove(topic);
            }
        }
    }

    pub fn subscribers(&self, topic: &str) -> Vec<String> {
        self.subscriptions
            .get(topic)
            .cloned()
            .unwrap_or_default()
    }

    pub fn topics(&self) -> Vec<String> {
        self.subscriptions.keys().cloned().collect()
    }

    pub fn topics_for_peer(&self, peer_id: &str) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter_map(|(topic, peers)| {
                if peers.iter().any(|p| p == peer_id) {
                    Some(topic.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn remove_peer(&mut self, peer_id: &str) {
        let empty_topics: Vec<String> = self
            .subscriptions
            .iter_mut()
            .filter_map(|(topic, peers)| {
                peers.retain(|p| p != peer_id);
                if peers.is_empty() {
                    Some(topic.clone())
                } else {
                    None
                }
            })
            .collect();
        for t in empty_topics {
            self.subscriptions.remove(&t);
        }
    }

    pub fn is_subscribed(&self, topic: &str, peer_id: &str) -> bool {
        self.subscriptions
            .get(topic)
            .map(|peers| peers.iter().any(|p| p == peer_id))
            .unwrap_or(false)
    }

    pub fn topic_count(&self) -> usize {
        self.subscriptions.len()
    }
}

pub fn encode_publish(topic: &str, data: &[u8]) -> Result<Vec<u8>> {
    let msg = PubSubMessage {
        topic: topic.to_string(),
        data: data.to_vec(),
    };
    rmp_serde::to_vec(&msg)
        .map_err(|e| MlinkError::CodecError(format!("encode PubSubMessage: {e}")))
}

pub fn decode_publish(bytes: &[u8]) -> Result<PubSubMessage> {
    rmp_serde::from_slice(bytes)
        .map_err(|e| MlinkError::CodecError(format!("decode PubSubMessage: {e}")))
}

pub async fn publish(
    node: &Node,
    pubsub: &PubSubManager,
    topic: &str,
    data: &[u8],
) -> Result<()> {
    let subs = pubsub.subscribers(topic);
    if subs.is_empty() {
        return Ok(());
    }
    let payload = encode_publish(topic, data)?;
    for peer_id in subs {
        node.send_raw(&peer_id, MessageType::Publish, &payload).await?;
    }
    Ok(())
}

pub async fn send_subscribe(node: &Node, peer_id: &str, topic: &str) -> Result<()> {
    node.send_raw(peer_id, MessageType::Subscribe, topic.as_bytes())
        .await
}

pub async fn send_unsubscribe(node: &Node, peer_id: &str, topic: &str) -> Result<()> {
    node.send_raw(peer_id, MessageType::Unsubscribe, topic.as_bytes())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribe_adds_peer_once() {
        let mut m = PubSubManager::new();
        m.subscribe("news", "peer-a");
        m.subscribe("news", "peer-a");
        assert_eq!(m.subscribers("news"), vec!["peer-a".to_string()]);
    }

    #[test]
    fn subscribe_tracks_multiple_peers() {
        let mut m = PubSubManager::new();
        m.subscribe("news", "peer-a");
        m.subscribe("news", "peer-b");
        let subs = m.subscribers("news");
        assert_eq!(subs.len(), 2);
        assert!(subs.contains(&"peer-a".to_string()));
        assert!(subs.contains(&"peer-b".to_string()));
    }

    #[test]
    fn unsubscribe_removes_peer() {
        let mut m = PubSubManager::new();
        m.subscribe("news", "peer-a");
        m.subscribe("news", "peer-b");
        m.unsubscribe("news", "peer-a");
        assert_eq!(m.subscribers("news"), vec!["peer-b".to_string()]);
    }

    #[test]
    fn unsubscribe_last_peer_drops_topic() {
        let mut m = PubSubManager::new();
        m.subscribe("news", "peer-a");
        m.unsubscribe("news", "peer-a");
        assert_eq!(m.topic_count(), 0);
        assert!(m.subscribers("news").is_empty());
    }

    #[test]
    fn unsubscribe_missing_is_noop() {
        let mut m = PubSubManager::new();
        m.unsubscribe("news", "nobody");
        m.subscribe("news", "a");
        m.unsubscribe("other", "a");
        assert_eq!(m.subscribers("news"), vec!["a".to_string()]);
    }

    #[test]
    fn topics_for_peer_lists_all() {
        let mut m = PubSubManager::new();
        m.subscribe("t1", "peer-a");
        m.subscribe("t2", "peer-a");
        m.subscribe("t3", "peer-b");
        let mut got = m.topics_for_peer("peer-a");
        got.sort();
        assert_eq!(got, vec!["t1".to_string(), "t2".to_string()]);
    }

    #[test]
    fn remove_peer_cleans_all_subscriptions() {
        let mut m = PubSubManager::new();
        m.subscribe("t1", "peer-a");
        m.subscribe("t2", "peer-a");
        m.subscribe("t1", "peer-b");
        m.remove_peer("peer-a");
        assert_eq!(m.subscribers("t1"), vec!["peer-b".to_string()]);
        assert!(m.subscribers("t2").is_empty());
        assert_eq!(m.topic_count(), 1);
    }

    #[test]
    fn is_subscribed_reflects_state() {
        let mut m = PubSubManager::new();
        m.subscribe("t1", "peer-a");
        assert!(m.is_subscribed("t1", "peer-a"));
        assert!(!m.is_subscribed("t1", "peer-b"));
        assert!(!m.is_subscribed("missing", "peer-a"));
    }

    #[test]
    fn encode_decode_publish_round_trip() {
        let bytes = encode_publish("chat", b"hello").unwrap();
        let msg = decode_publish(&bytes).unwrap();
        assert_eq!(msg.topic, "chat");
        assert_eq!(msg.data, b"hello");
    }

    #[test]
    fn decode_publish_rejects_garbage() {
        let err = decode_publish(&[0xFF, 0xFE]).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_ok() {
        use crate::core::node::{Node, NodeConfig};
        let tmp = tempfile::tempdir().unwrap();
        let cfg = NodeConfig {
            name: "t".into(),
            encrypt: false,
            trust_store_path: Some(tmp.path().join("t.json")),
        };
        let node = Node::new(cfg).await.unwrap();
        let pubsub = PubSubManager::new();
        publish(&node, &pubsub, "no-subs", b"hi").await.unwrap();
    }
}
