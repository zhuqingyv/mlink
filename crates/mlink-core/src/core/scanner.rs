use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc::Sender;
use tokio::time::{interval, MissedTickBehavior};

use crate::protocol::errors::Result;
use crate::transport::{DiscoveredPeer, Transport};

pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(3);

pub struct Scanner {
    transport: Box<dyn Transport>,
    app_uuid: String,
    seen: HashSet<String>,
    scan_interval: Duration,
}

impl Scanner {
    pub fn new(transport: Box<dyn Transport>, app_uuid: impl Into<String>) -> Self {
        Self {
            transport,
            app_uuid: app_uuid.into(),
            seen: HashSet::new(),
            scan_interval: DEFAULT_SCAN_INTERVAL,
        }
    }

    pub fn with_interval(mut self, scan_interval: Duration) -> Self {
        self.scan_interval = scan_interval;
        self
    }

    pub fn app_uuid(&self) -> &str {
        &self.app_uuid
    }

    pub fn scan_interval(&self) -> Duration {
        self.scan_interval
    }

    pub fn seen_count(&self) -> usize {
        self.seen.len()
    }

    pub async fn discover_once(&mut self) -> Result<Vec<DiscoveredPeer>> {
        let all = self.transport.discover().await?;
        let mut fresh = Vec::new();
        for peer in all {
            if peer.id == self.app_uuid {
                continue;
            }
            if self.seen.insert(peer.id.clone()) {
                fresh.push(peer);
            }
        }
        Ok(fresh)
    }

    pub async fn discover_loop(&mut self, tx: Sender<DiscoveredPeer>) -> Result<()> {
        let mut ticker = interval(self.scan_interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if tx.is_closed() {
                return Ok(());
            }
            let fresh = self.discover_once().await?;
            for peer in fresh {
                if tx.send(peer).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    pub fn reset(&mut self) {
        self.seen.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::errors::MlinkError;
    use crate::transport::{Connection, TransportCapabilities};
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::sync::mpsc;
    use tokio::time::{timeout, Duration};

    struct ScriptedTransport {
        rounds: Arc<Mutex<Vec<Vec<DiscoveredPeer>>>>,
        calls: Arc<Mutex<usize>>,
    }

    impl ScriptedTransport {
        fn new(rounds: Vec<Vec<DiscoveredPeer>>) -> Self {
            Self {
                rounds: Arc::new(Mutex::new(rounds)),
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn calls(&self) -> Arc<Mutex<usize>> {
            Arc::clone(&self.calls)
        }
    }

    #[async_trait]
    impl Transport for ScriptedTransport {
        fn id(&self) -> &str {
            "scripted"
        }

        fn capabilities(&self) -> TransportCapabilities {
            TransportCapabilities {
                max_peers: 10,
                throughput_bps: 0,
                latency_ms: 0,
                reliable: true,
                bidirectional: true,
            }
        }

        async fn discover(&mut self) -> Result<Vec<DiscoveredPeer>> {
            *self.calls.lock().unwrap() += 1;
            let mut rounds = self.rounds.lock().unwrap();
            if rounds.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(rounds.remove(0))
            }
        }

        async fn connect(&mut self, _peer: &DiscoveredPeer) -> Result<Box<dyn Connection>> {
            Err(MlinkError::HandlerError("not used in scanner tests".into()))
        }

        async fn listen(&mut self) -> Result<Box<dyn Connection>> {
            Err(MlinkError::HandlerError("not used in scanner tests".into()))
        }

        fn mtu(&self) -> usize {
            512
        }
    }

    fn peer(id: &str) -> DiscoveredPeer {
        DiscoveredPeer {
            id: id.into(),
            name: format!("name-{id}"),
            rssi: Some(-50),
            metadata: Vec::new(),
        }
    }

    #[tokio::test]
    async fn discover_once_returns_new_peers() {
        let t = ScriptedTransport::new(vec![vec![peer("a"), peer("b")]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        let got = s.discover_once().await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(s.seen_count(), 2);
    }

    #[tokio::test]
    async fn discover_once_dedupes_across_calls() {
        let t = ScriptedTransport::new(vec![
            vec![peer("a"), peer("b")],
            vec![peer("b"), peer("c")],
        ]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");

        let first = s.discover_once().await.unwrap();
        assert_eq!(first.len(), 2);

        let second = s.discover_once().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, "c");
        assert_eq!(s.seen_count(), 3);
    }

    #[tokio::test]
    async fn discover_once_filters_self() {
        let t = ScriptedTransport::new(vec![vec![peer("self-uuid"), peer("b")]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        let got = s.discover_once().await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "b");
        assert_eq!(s.seen_count(), 1);
    }

    #[tokio::test]
    async fn reset_clears_seen() {
        let t = ScriptedTransport::new(vec![vec![peer("a")], vec![peer("a")]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        s.discover_once().await.unwrap();
        assert_eq!(s.seen_count(), 1);
        s.reset();
        assert_eq!(s.seen_count(), 0);
        let got = s.discover_once().await.unwrap();
        assert_eq!(got.len(), 1);
    }

    #[tokio::test]
    async fn discover_loop_yields_peers_and_respects_channel_close() {
        let t = ScriptedTransport::new(vec![
            vec![peer("a"), peer("b")],
            vec![peer("c")],
            vec![peer("d")],
        ]);
        let calls = t.calls();
        let mut s = Scanner::new(Box::new(t), "self-uuid")
            .with_interval(Duration::from_millis(10));

        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(async move { s.discover_loop(tx).await });

        let mut got_ids = Vec::new();
        for _ in 0..4 {
            let p = timeout(Duration::from_secs(1), rx.recv())
                .await
                .expect("timed out waiting for peer")
                .expect("channel closed before peer arrived");
            got_ids.push(p.id);
        }
        drop(rx);

        let result = timeout(Duration::from_secs(1), handle).await.unwrap();
        assert!(result.unwrap().is_ok());

        got_ids.sort();
        assert_eq!(got_ids, vec!["a", "b", "c", "d"]);
        assert!(*calls.lock().unwrap() >= 3);
    }

    #[tokio::test]
    async fn with_interval_sets_custom_interval() {
        let t = ScriptedTransport::new(vec![]);
        let s = Scanner::new(Box::new(t), "self-uuid")
            .with_interval(Duration::from_millis(42));
        assert_eq!(s.scan_interval(), Duration::from_millis(42));
    }
}
