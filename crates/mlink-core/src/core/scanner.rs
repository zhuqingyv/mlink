use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{interval, MissedTickBehavior};

use crate::protocol::errors::Result;
use crate::transport::{DiscoveredPeer, Transport};

pub const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(3);

pub struct Scanner {
    transport: Box<dyn Transport>,
    app_uuid: String,
    seen: HashSet<String>,
    scan_interval: Duration,
    room_hashes: Vec<[u8; 8]>,
    unsee_rx: Option<Receiver<String>>,
}

impl Scanner {
    pub fn new(transport: Box<dyn Transport>, app_uuid: impl Into<String>) -> Self {
        Self {
            transport,
            app_uuid: app_uuid.into(),
            seen: HashSet::new(),
            scan_interval: DEFAULT_SCAN_INTERVAL,
            room_hashes: Vec::new(),
            unsee_rx: None,
        }
    }

    /// Attach a channel on which callers can request that `id` be removed from
    /// the "seen" set so the next scan round surfaces it again. Used by the
    /// serve loop to retry peers whose connect attempt died.
    pub fn set_unsee_channel(&mut self, rx: Receiver<String>) {
        self.unsee_rx = Some(rx);
    }

    pub fn unsee(&mut self, id: &str) {
        self.seen.remove(id);
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

    pub fn set_room_hashes(&mut self, hashes: Vec<[u8; 8]>) {
        self.room_hashes = hashes;
    }

    pub fn room_hashes(&self) -> &[[u8; 8]] {
        &self.room_hashes
    }

    /// Returns `true` if the peer should be surfaced under the current room
    /// filter. The filter is soft: when `metadata` is empty we pass the peer
    /// through because some transports (notably macOS BLE) can't read the
    /// advertised room hash from a peripheral before connecting. Room
    /// membership is then verified post-handshake at the Node layer; here we
    /// only avoid surfacing peers that *positively advertise a different*
    /// room when `hashes` is set.
    fn metadata_matches_any_room(metadata: &[u8], hashes: &[[u8; 8]]) -> bool {
        if hashes.is_empty() {
            return true;
        }
        if metadata.is_empty() {
            return true;
        }
        if metadata.len() < 8 {
            return false;
        }
        hashes.iter().any(|h| {
            metadata
                .windows(8)
                .any(|w| w == h.as_slice())
        })
    }

    pub async fn discover_once(&mut self) -> Result<Vec<DiscoveredPeer>> {
        let all = self.transport.discover().await?;
        let mut fresh = Vec::new();
        for peer in all {
            if peer.id == self.app_uuid {
                continue;
            }
            if !Self::metadata_matches_any_room(&peer.metadata, &self.room_hashes) {
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
            // Drain any pending unsee requests before the next scan so retried
            // peers have a chance to re-surface on the upcoming round.
            if let Some(rx) = self.unsee_rx.as_mut() {
                while let Ok(id) = rx.try_recv() {
                    self.seen.remove(&id);
                }
            }
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

    fn peer_with_meta(id: &str, metadata: Vec<u8>) -> DiscoveredPeer {
        DiscoveredPeer {
            id: id.into(),
            name: format!("name-{id}"),
            rssi: Some(-50),
            metadata,
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

    #[tokio::test]
    async fn no_room_hashes_means_no_filter() {
        let t = ScriptedTransport::new(vec![vec![
            peer_with_meta("a", vec![1, 2, 3]),
            peer_with_meta("b", Vec::new()),
        ]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        let got = s.discover_once().await.unwrap();
        assert_eq!(got.len(), 2);
    }

    #[tokio::test]
    async fn room_hash_filter_keeps_matching_peers() {
        let hash: [u8; 8] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];
        let mut meta_with = vec![0x01, 0x02];
        meta_with.extend_from_slice(&hash);
        meta_with.push(0x99);
        // Peer "a" carries a matching hash in metadata → keep.
        // Peer "b" advertises a *different* 8-byte chunk → drop (wrong room).
        // Peer "c" has no metadata → soft-pass (room verified post-handshake).
        let t = ScriptedTransport::new(vec![vec![
            peer_with_meta("a", meta_with),
            peer_with_meta("b", vec![0, 0, 0, 0, 0, 0, 0, 0]),
            peer_with_meta("c", Vec::new()),
        ]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        s.set_room_hashes(vec![hash]);
        let mut got = s.discover_once().await.unwrap();
        got.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "a");
        assert_eq!(got[1].id, "c");
    }

    #[tokio::test]
    async fn empty_metadata_passes_under_room_filter() {
        // Explicitly document the soft-pass: when metadata is empty we must
        // surface the peer even though a room filter is set, because
        // macOS BLE can't read the advertised room hash pre-connect.
        let hash: [u8; 8] = [9; 8];
        let t = ScriptedTransport::new(vec![vec![peer_with_meta("x", Vec::new())]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        s.set_room_hashes(vec![hash]);
        let got = s.discover_once().await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "x");
    }

    #[tokio::test]
    async fn room_hash_filter_matches_any_of_multiple() {
        let h1: [u8; 8] = [1, 1, 1, 1, 1, 1, 1, 1];
        let h2: [u8; 8] = [2, 2, 2, 2, 2, 2, 2, 2];
        let t = ScriptedTransport::new(vec![vec![
            peer_with_meta("a", h1.to_vec()),
            peer_with_meta("b", h2.to_vec()),
            peer_with_meta("c", vec![9, 9, 9, 9, 9, 9, 9, 9]),
        ]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        s.set_room_hashes(vec![h1, h2]);
        let mut got = s.discover_once().await.unwrap();
        got.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "a");
        assert_eq!(got[1].id, "b");
    }

    #[tokio::test]
    async fn unsee_allows_peer_to_resurface() {
        let t = ScriptedTransport::new(vec![vec![peer("a")], vec![peer("a")]]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        let first = s.discover_once().await.unwrap();
        assert_eq!(first.len(), 1);
        // Without unsee, the second round returns nothing.
        // With unsee("a"), "a" should re-surface as fresh.
        s.unsee("a");
        let second = s.discover_once().await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, "a");
    }

    #[tokio::test]
    async fn discover_loop_consumes_unsee_channel() {
        let t = ScriptedTransport::new(vec![
            vec![peer("a")],
            vec![peer("a")],
            vec![peer("a")],
        ]);
        let mut s = Scanner::new(Box::new(t), "self-uuid")
            .with_interval(Duration::from_millis(10));
        let (unsee_tx, unsee_rx) = mpsc::channel::<String>(4);
        s.set_unsee_channel(unsee_rx);
        let (tx, mut rx) = mpsc::channel(16);
        let handle = tokio::spawn(async move { s.discover_loop(tx).await });

        // First round yields "a".
        let p = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.id, "a");

        // Ask scanner to forget "a" so it resurfaces on the next tick.
        unsee_tx.send("a".into()).await.unwrap();
        let p = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.id, "a");

        drop(rx);
        let _ = timeout(Duration::from_secs(1), handle).await.unwrap();
    }

    #[tokio::test]
    async fn set_room_hashes_stores_hashes() {
        let t = ScriptedTransport::new(vec![]);
        let mut s = Scanner::new(Box::new(t), "self-uuid");
        let h: [u8; 8] = [1; 8];
        s.set_room_hashes(vec![h]);
        assert_eq!(s.room_hashes(), &[h]);
    }
}
