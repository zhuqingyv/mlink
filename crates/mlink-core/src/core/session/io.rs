//! Session send path + inline scheduler (W2-A). 契约：INTERFACE-CONTRACTS.md §7.
//! 锁约束：send/flush 不得跨 await 持 `active`/`links`/`unacked`（clone Arc → drop → await）。

use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::warn;

use crate::core::link::Link;
use crate::core::session::types::{
    LinkRole, LinkStatus, PendingFrame, Session, SessionEvent, SwitchCause, SWITCHBACK_HYSTERESIS,
};
use crate::protocol::ack::AckFrame;
use crate::protocol::compress::{compress as do_compress, should_compress};
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::frame::encode_frame;
use crate::protocol::types::{encode_flags, Frame, MessageType, MAGIC, PROTOCOL_VERSION};

impl Session {
    /// 上层唯一发送入口：分配 u32 seq → 构帧 → 选 active → send → 登记
    /// UnackedRing。失败走内联 failover（promote_on_failure + retry 一次）。
    pub async fn send(
        &self,
        msg_type: MessageType,
        payload: Vec<u8>,
        encrypt_key: Option<&[u8]>,
        compress: bool,
    ) -> Result<u32> {
        let seq = self.send_seq.lock().await.next();
        let mut body = payload;
        let compressed_flag = compress && should_compress(&body) && {
            body = do_compress(&body)?;
            true
        };
        let encrypted_flag = if let Some(k) = encrypt_key {
            // security::encrypt 收 u16 nonce；session u32 截低 16 位，两端对齐。
            body = crate::core::security::encrypt(&body, k, seq as u16)?;
            true
        } else {
            false
        };
        let length = u16::try_from(body.len()).map_err(|_| MlinkError::PayloadTooLarge {
            size: body.len(),
            max: u16::MAX as usize,
        })?;
        let bytes = encode_frame(&Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(compressed_flag, encrypted_flag, msg_type),
            seq: seq as u16,
            length,
            payload: body,
        });

        let no_healthy = || MlinkError::NoHealthyLink { peer_id: self.peer_id.clone() };
        let primary = self.current_or_pick().await.ok_or_else(no_healthy)?;
        if primary.send(&bytes).await.is_ok() {
            self.record_unacked(seq, &bytes, msg_type).await;
            return Ok(seq);
        }
        warn!(peer_id = %self.peer_id, link_id = %primary.id(), "active send failed, failover");
        let failing_id = primary.id().to_string();
        drop(primary);
        let (new_id, _) = self.promote_on_failure(&failing_id).await.ok_or_else(no_healthy)?;
        let new_link = self.link_by_id(&new_id).await.ok_or_else(no_healthy)?;
        new_link.send(&bytes).await.map_err(|_| no_healthy())?;
        self.record_unacked(seq, &bytes, msg_type).await;
        Ok(seq)
    }

    /// 对端 AckFrame 到达：UnackedRing ack_cum + sack。
    pub(crate) async fn on_ack(&self, ack: &AckFrame) {
        let mut ring = self.unacked.lock().await;
        ring.ack_cum(ack.ack);
        ring.sack(ack.ack, ack.bitmap);
    }

    /// 把所有仍在 ring 里的 unacked 帧经 `link_id` 重发。seq 保持原值。
    pub(crate) async fn flush_unacked_over(&self, link_id: &str) -> Result<()> {
        let link = self.link_by_id(link_id).await.ok_or_else(|| {
            MlinkError::NoHealthyLink {
                peer_id: self.peer_id.clone(),
            }
        })?;
        let items = { self.unacked.lock().await.unacked_since(0) };
        for (_seq, pf) in items {
            link.send(&pf.bytes).await?;
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub async fn status(&self) -> Vec<LinkStatus> {
        let links = self.links.read().await.clone();
        let active = self.active.read().await.clone();
        links
            .into_iter()
            .map(|l| LinkStatus {
                link_id: l.id().to_string(),
                kind: l.kind(),
                role: if active.as_deref() == Some(l.id()) {
                    LinkRole::Active
                } else {
                    LinkRole::Standby
                },
                health: l.health(),
            })
            .collect()
    }

    // ---- 内联 scheduler ----

    /// 仅供展示/Web debug 使用；实际调度走 Link::score（带 caps）。
    pub(crate) fn score(status: &LinkStatus) -> i64 {
        let h = &status.health;
        let success = 1000 - h.err_rate_milli as i64;
        success.saturating_mul(1_000_000) / 1000 - h.rtt_ema_us as i64 / 10
    }

    pub(crate) async fn pick_best(&self) -> Option<String> {
        self.best_excluding("").await
    }

    pub(crate) async fn promote_on_failure(
        &self,
        failing_id: &str,
    ) -> Option<(String, SwitchCause)> {
        let next_id = self.best_excluding(failing_id).await?;
        let from = {
            let mut active = self.active.write().await;
            let old = active.clone().unwrap_or_default();
            *active = Some(next_id.clone());
            old
        };
        let _ = self.events.send(SessionEvent::Switched {
            peer_id: self.peer_id.clone(),
            from,
            to: next_id.clone(),
            cause: SwitchCause::ActiveFailed,
        });
        Some((next_id, SwitchCause::ActiveFailed))
    }

    pub(crate) async fn should_switchback(&self, current: &str, best: &str) -> bool {
        let mut st = self.sched_state.lock().await;
        if current == best {
            st.better_streak.remove(best);
            return false;
        }
        let n = st.better_streak.entry(best.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        *n >= SWITCHBACK_HYSTERESIS
    }

    // ---- 内部小工具 ----

    async fn best_excluding(&self, exclude_id: &str) -> Option<String> {
        let links = self.links.read().await;
        links
            .iter()
            .filter(|l| l.id() != exclude_id && l.is_healthy())
            .max_by_key(|l| l.score())
            .map(|l| l.id().to_string())
    }

    async fn current_or_pick(&self) -> Option<Arc<Link>> {
        if let Some(id) = self.active.read().await.clone() {
            if let Some(l) = self.link_by_id(&id).await {
                if l.is_healthy() {
                    return Some(l);
                }
            }
        }
        let id = self.pick_best().await?;
        {
            let mut a = self.active.write().await;
            if a.is_none() {
                *a = Some(id.clone());
            }
        }
        self.link_by_id(&id).await
    }

    async fn link_by_id(&self, id: &str) -> Option<Arc<Link>> {
        let links = self.links.read().await;
        links.iter().find(|l| l.id() == id).cloned()
    }

    async fn record_unacked(&self, seq: u32, bytes: &[u8], msg_type: MessageType) {
        let mut ring = self.unacked.lock().await;
        if let Some((evicted_seq, _)) = ring.push(
            seq,
            PendingFrame {
                bytes: bytes.to_vec(),
                msg_type,
            },
        ) {
            warn!(peer_id = %self.peer_id, evicted_seq, new_seq = seq, "UnackedRing full, evicted oldest");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::link::{Link, TransportKind};
    use crate::core::session::types::Session;
    use crate::transport::mock::{mock_pair, MockConnection};
    use crate::transport::{Connection, TransportCapabilities};
    use std::sync::Arc;

    fn mk_session(peer_id: &str) -> Arc<Session> {
        Arc::new(Session::new(peer_id.into(), [0u8; 16]))
    }

    fn caps(throughput: u64) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 1,
            throughput_bps: throughput,
            latency_ms: 1,
            reliable: true,
            bidirectional: true,
        }
    }

    /// Attaches a fresh mock-backed link to the session and hands back the
    /// peer (B) side so the test keeps it alive — otherwise its receiver drop
    /// would make every write on the A side fail with PeerGone.
    async fn attach(
        session: &Session,
        id: &str,
        kind: TransportKind,
        caps: TransportCapabilities,
    ) -> (Arc<Link>, MockConnection) {
        let (a, b) = mock_pair();
        let conn: Arc<dyn Connection> = Arc::new(a);
        let link = Arc::new(Link::new(id.into(), conn, kind, caps));
        session.links.write().await.push(Arc::clone(&link));
        (link, b)
    }

    #[tokio::test]
    async fn send_single_link_records_unacked() {
        let s = mk_session("peer1");
        let (_l, _peer) = attach(&s, "link-a", TransportKind::Tcp, caps(1_000_000)).await;
        let seq = s
            .send(MessageType::Message, b"hi".to_vec(), None, false)
            .await
            .expect("send ok");
        assert_eq!(seq, 0);
        assert_eq!(s.unacked.lock().await.len(), 1);
        // active should be pinned after first send
        assert_eq!(s.active.read().await.as_deref(), Some("link-a"));
    }

    #[tokio::test]
    async fn send_no_links_returns_no_healthy() {
        let s = mk_session("peer-dead");
        let err = s
            .send(MessageType::Message, b"x".to_vec(), None, false)
            .await
            .expect_err("should fail");
        assert!(matches!(err, MlinkError::NoHealthyLink { .. }));
    }

    #[tokio::test]
    async fn send_failover_to_standby() {
        let s = mk_session("peer2");
        // primary link whose writer side is closed — writes will fail.
        let (primary_conn_a, primary_conn_b) = mock_pair();
        drop(primary_conn_b); // closing peer B → write on A returns PeerGone
        let primary: Arc<dyn Connection> = Arc::new(primary_conn_a);
        let primary_link = Arc::new(Link::new(
            "primary".into(),
            primary,
            TransportKind::Ble,
            caps(100_000),
        ));
        // standby link with live pair
        let (sb_a, _sb_b) = mock_pair();
        let standby: Arc<dyn Connection> = Arc::new(sb_a);
        let standby_link = Arc::new(Link::new(
            "standby".into(),
            standby,
            TransportKind::Tcp,
            caps(10_000_000), // higher throughput → higher score tiebreak
        ));
        s.links.write().await.push(primary_link);
        s.links.write().await.push(standby_link);
        // force primary active to guarantee initial pick
        *s.active.write().await = Some("primary".into());

        let mut events = s.subscribe();

        let seq = s
            .send(MessageType::Message, b"failover-msg".to_vec(), None, false)
            .await
            .expect("send after failover ok");
        assert_eq!(seq, 0);
        assert_eq!(s.active.read().await.as_deref(), Some("standby"));
        // Switched event emitted
        let ev = events.try_recv().expect("expected Switched event");
        assert!(matches!(ev, SessionEvent::Switched { cause: SwitchCause::ActiveFailed, .. }));
    }

    #[tokio::test]
    async fn process_ack_clears_unacked() {
        let s = mk_session("peer3");
        let (_l, _peer) = attach(&s, "l", TransportKind::Tcp, caps(1_000_000)).await;
        for _ in 0..5 {
            s.send(MessageType::Message, b"m".to_vec(), None, false)
                .await
                .unwrap();
        }
        assert_eq!(s.unacked.lock().await.len(), 5);
        // Ack cumulative up to seq=2 → clear seqs 0,1,2 (3 entries)
        s.on_ack(&AckFrame { ack: 2, bitmap: 0 }).await;
        assert_eq!(s.unacked.lock().await.len(), 2);
        // SACK seqs 3 (bit0 relative to base=2) and 4 (bit1)
        s.on_ack(&AckFrame {
            ack: 2,
            bitmap: 0b11,
        })
        .await;
        assert_eq!(s.unacked.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn flush_unacked_replays_over_new_link() {
        let s = mk_session("peer4");
        // primary fills unacked, but when it's time to flush we use the standby path.
        let (_primary, _peer_p) = attach(&s, "p", TransportKind::Ble, caps(200_000)).await;
        s.send(MessageType::Message, b"a".to_vec(), None, false)
            .await
            .unwrap();
        s.send(MessageType::Message, b"b".to_vec(), None, false)
            .await
            .unwrap();
        assert_eq!(s.unacked.lock().await.len(), 2);

        // Second link with a reader side we can pull bytes from.
        let (conn_a, conn_b) = mock_pair();
        let conn_a: Arc<dyn Connection> = Arc::new(conn_a);
        let link_b = Arc::new(Link::new(
            "q".into(),
            conn_a,
            TransportKind::Tcp,
            caps(5_000_000),
        ));
        s.links.write().await.push(link_b);

        s.flush_unacked_over("q").await.expect("flush ok");
        // Two frames should have landed on conn_b.
        let f1 = conn_b.read().await.expect("frame1");
        let f2 = conn_b.read().await.expect("frame2");
        assert!(!f1.is_empty());
        assert!(!f2.is_empty());
    }

    #[tokio::test]
    async fn should_switchback_requires_hysteresis() {
        let s = mk_session("peer5");
        assert!(!s.should_switchback("a", "a").await);
        for _ in 0..(SWITCHBACK_HYSTERESIS as usize - 1) {
            assert!(!s.should_switchback("a", "b").await);
        }
        assert!(s.should_switchback("a", "b").await);
        // streak resets when current catches up with best.
        assert!(!s.should_switchback("b", "b").await);
    }
}
