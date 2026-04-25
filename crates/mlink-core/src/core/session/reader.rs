//! Session 多 link 合并读取 + 去重 + 延迟 ACK (W2-B)。
//!
//! Wire 约定（与 W2-A 发送端对齐）：
//! - `MessageType::Message` 的 `frame.payload` 前 4 字节是 big-endian u32 session_seq，
//!   其后是应用层原始 payload。接收侧按 seq 去重。
//! - `MessageType::Ack` 的 `frame.payload` 是 20 字节 [`AckFrame`]（见 protocol::ack）。
//!   收到后清 `UnackedRing`；**不**作为应用消息返回给 `recv` 调用方。
//! - 其他 MessageType（Heartbeat / Handshake / Ctrl / Stream*）走非 dual-transport 路径，
//!   由 Node 层处理；本 reader 视为"非 session-seq 帧"直接跳过返回空 seq，继续 select。
//!
//! ACK 合并策略（契约 §8 不变量）：累积到 [`ACK_MAX_PENDING`] 条或距首条 Accept
//! 超过 [`ACK_MAX_DELAY_MS`] 即刷。每次 `recv` 被调用时在主循环里一并推进。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::sleep_until;

use crate::core::link::Link;
use crate::core::session::types::{
    AckPending, Session, SessionEvent, ACK_MAX_DELAY_MS, ACK_MAX_PENDING,
};
use crate::protocol::ack::{decode_ack, encode_ack, AckFrame};
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::frame::{decode_frame, encode_frame};
use crate::protocol::seq::Observation;
use crate::protocol::types::{
    decode_flags, encode_flags, Frame, MessageType, MAGIC, PROTOCOL_VERSION,
};

/// Session seq 前缀宽度（big-endian u32）。
pub(crate) const SESSION_SEQ_HEADER: usize = 4;

impl Session {
    /// 从任意活跃 link 读取下一条 **应用消息**。
    ///
    /// 语义：
    /// - 谁先有数据就用谁；其他 link 的 recv 被 `tokio::select!` 取消。
    /// - Duplicate / Stale 静默丢弃，loop 继续。
    /// - 收到对端 Ack 帧 → `on_ack`，loop 继续。
    /// - 读失败 → `Link::record_error` + emit `LinkRemoved` 事件 + loop 继续；
    ///   若所有 link 都丢了，返回 `NoHealthyLink`。
    /// - 每一轮 loop 都会 flush 到期的 ack pending。
    ///
    /// 返回：`(session_seq, app_payload)`。
    pub async fn recv(self: &Arc<Self>) -> Result<(u32, Vec<u8>)> {
        loop {
            let links = self.snapshot_links().await;
            if links.is_empty() {
                return Err(MlinkError::NoHealthyLink {
                    peer_id: self.peer_id.clone(),
                });
            }
            // 先把到期的 ack 发了。
            self.maybe_flush_ack_locked().await;

            // 计算下一次 ack 到期时刻，作为 select 的 timeout 分支。
            let ack_deadline = self.ack_deadline().await;

            match self.read_any(&links, ack_deadline).await {
                ReadOutcome::Msg(seq, payload) => return Ok((seq, payload)),
                ReadOutcome::Continue => continue,
                ReadOutcome::AllLinksGone => {
                    return Err(MlinkError::NoHealthyLink {
                        peer_id: self.peer_id.clone(),
                    });
                }
            }
        }
    }

    /// 立刻把累积的 ack pending 打包一条 AckFrame 经 active link 发出。
    /// 若 `pending.count == 0` 直接返回。
    pub(crate) async fn flush_ack_now(&self) -> Result<()> {
        let should_send = {
            let mut p = self.ack_pending.lock().await;
            if p.count == 0 {
                false
            } else {
                *p = AckPending::default();
                true
            }
        };
        if !should_send {
            return Ok(());
        }
        let (last, bits) = self.dedup.lock().await.snapshot();
        let af = AckFrame {
            ack: last,
            bitmap: bits,
        };
        let bytes = encode_ack_frame(&af);
        if let Some(link) = self.active_link().await {
            if let Err(e) = link.send(&bytes).await {
                // active link 发失败：record_error 已在 Link::send 里做过；
                // 这里只回退状态让下轮 reader 再试。
                let mut p = self.ack_pending.lock().await;
                p.count = p.count.saturating_add(1);
                if p.first_pending_at.is_none() {
                    p.first_pending_at = Some(Instant::now());
                }
                return Err(e);
            }
        }
        Ok(())
    }

    async fn snapshot_links(&self) -> Vec<Arc<Link>> {
        self.links.read().await.iter().cloned().collect()
    }

    async fn active_link(&self) -> Option<Arc<Link>> {
        let active_id = self.active.read().await.clone()?;
        let links = self.links.read().await;
        links.iter().find(|l| l.id() == active_id).cloned()
    }

    async fn maybe_flush_ack_locked(&self) {
        let should = {
            let p = self.ack_pending.lock().await;
            ack_should_flush(&p)
        };
        if should {
            let _ = self.flush_ack_now().await;
        }
    }

    async fn ack_deadline(&self) -> Instant {
        let p = self.ack_pending.lock().await;
        match p.first_pending_at {
            Some(t) => t + Duration::from_millis(ACK_MAX_DELAY_MS),
            None => Instant::now() + Duration::from_secs(60),
        }
    }

    async fn read_any(&self, links: &[Arc<Link>], ack_deadline: Instant) -> ReadOutcome {
        // Build per-link futures; race them with the ack timer.
        let mut futures = Vec::with_capacity(links.len());
        for l in links {
            let link = Arc::clone(l);
            futures.push(Box::pin(async move {
                let bytes = link.recv().await;
                (link, bytes)
            }));
        }
        tokio::select! {
            biased;
            _ = sleep_until(ack_deadline.into()) => ReadOutcome::Continue,
            (result, _idx, _rest) = futures::future::select_all(futures) => {
                let (link, res) = result;
                match res {
                    Ok(bytes) => self.handle_bytes(&link, &bytes).await,
                    Err(_e) => {
                        // Link 侧已经 record_error。这里通知 session，降级继续。
                        let _ = self.events.send(SessionEvent::LinkRemoved {
                            peer_id: self.peer_id.clone(),
                            link_id: link.id().to_string(),
                            reason: "recv_error".into(),
                        });
                        // 检查是否还有其它 link
                        if self.links.read().await.iter().any(|l| l.id() != link.id()) {
                            ReadOutcome::Continue
                        } else {
                            ReadOutcome::AllLinksGone
                        }
                    }
                }
            }
        }
    }

    async fn handle_bytes(&self, _link: &Arc<Link>, bytes: &[u8]) -> ReadOutcome {
        let frame = match decode_frame(bytes) {
            Ok(f) => f,
            Err(_) => return ReadOutcome::Continue, // malformed → ignore
        };
        let (_compressed, _encrypted, msg_type) = decode_flags(frame.flags);
        match msg_type {
            MessageType::Ack => {
                if let Ok(ack) = decode_ack(&frame.payload) {
                    self.on_ack(&ack).await;
                }
                ReadOutcome::Continue
            }
            MessageType::Message => match self.consume_message(frame).await {
                Some((seq, payload)) => ReadOutcome::Msg(seq, payload),
                None => ReadOutcome::Continue,
            },
            // Heartbeat / Handshake / Ctrl / Stream*：不是 session-seq 范畴。
            // 上层 Node 的老路径（recv_raw / spawn_peer_reader）负责处理；
            // 这里直接忽略继续等下一帧。
            _ => ReadOutcome::Continue,
        }
    }

    async fn consume_message(&self, frame: Frame) -> Option<(u32, Vec<u8>)> {
        if frame.payload.len() < SESSION_SEQ_HEADER {
            return None; // 非 session-seq 帧，静默丢
        }
        let seq = u32::from_be_bytes([
            frame.payload[0],
            frame.payload[1],
            frame.payload[2],
            frame.payload[3],
        ]);
        let body = frame.payload[SESSION_SEQ_HEADER..].to_vec();

        let obs = {
            let mut w = self.dedup.lock().await;
            w.observe(seq)
        };
        match obs {
            Observation::Duplicate | Observation::Stale => None,
            Observation::Accept => {
                let reached_threshold = {
                    let mut p = self.ack_pending.lock().await;
                    p.count = p.count.saturating_add(1);
                    if p.first_pending_at.is_none() {
                        p.first_pending_at = Some(Instant::now());
                    }
                    p.count >= ACK_MAX_PENDING as u32
                };
                if reached_threshold {
                    // 不要等到下一轮 recv；8 条阈值必须立刻刷。
                    let _ = self.flush_ack_now().await;
                }
                Some((seq, body))
            }
        }
    }
}

fn ack_should_flush(p: &AckPending) -> bool {
    if p.count >= ACK_MAX_PENDING as u32 {
        return true;
    }
    if let Some(t) = p.first_pending_at {
        if t.elapsed() >= Duration::from_millis(ACK_MAX_DELAY_MS) {
            return true;
        }
    }
    false
}

/// 把 [`AckFrame`] 包进一个 `MessageType::Ack` 帧二进制；frame.seq=0（对 session 层无意义）。
fn encode_ack_frame(ack: &AckFrame) -> Vec<u8> {
    let body = encode_ack(ack);
    let frame = Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, MessageType::Ack),
        seq: 0,
        length: body.len() as u16,
        payload: body,
    };
    encode_frame(&frame)
}

enum ReadOutcome {
    Msg(u32, Vec<u8>),
    Continue,
    AllLinksGone,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::link::TransportKind;
    use crate::transport::{
        mock::mock_pair, Connection, TransportCapabilities,
    };
    use std::sync::Arc;

    fn caps(throughput: u64) -> TransportCapabilities {
        TransportCapabilities {
            max_peers: 4,
            throughput_bps: throughput,
            latency_ms: 1,
            reliable: true,
            bidirectional: true,
        }
    }

    /// Build a Session with one Mock link attached; returns the session plus
    /// the far-end Connection used to inject bytes from "the peer".
    async fn session_with_one_link() -> (Arc<Session>, Box<dyn Connection>) {
        let (a, b) = mock_pair();
        let sess = Arc::new(Session::new("peer-x".into(), [0u8; 16]));
        let local_conn: Arc<dyn Connection> = Arc::new(a);
        let link = Arc::new(Link::new(
            "mock:x:1".into(),
            local_conn,
            TransportKind::Mock,
            caps(1_000_000),
        ));
        sess.links.write().await.push(link.clone());
        *sess.active.write().await = Some(link.id().to_string());
        (sess, Box::new(b))
    }

    fn encode_message(seq: u32, payload: &[u8]) -> Vec<u8> {
        let mut body = Vec::with_capacity(SESSION_SEQ_HEADER + payload.len());
        body.extend_from_slice(&seq.to_be_bytes());
        body.extend_from_slice(payload);
        let frame = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(false, false, MessageType::Message),
            seq: 0,
            length: body.len() as u16,
            payload: body,
        };
        encode_frame(&frame)
    }

    #[tokio::test]
    async fn recv_returns_message_and_decodes_session_seq() {
        let (sess, peer) = session_with_one_link().await;
        peer.write(&encode_message(7, b"hello")).await.unwrap();
        let (seq, body) = sess.recv().await.unwrap();
        assert_eq!(seq, 7);
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn recv_drops_duplicate_seq() {
        let (sess, peer) = session_with_one_link().await;
        peer.write(&encode_message(1, b"a")).await.unwrap();
        peer.write(&encode_message(1, b"a")).await.unwrap();
        peer.write(&encode_message(2, b"b")).await.unwrap();
        let (s1, _) = sess.recv().await.unwrap();
        let (s2, p2) = sess.recv().await.unwrap();
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);
        assert_eq!(p2, b"b");
    }

    #[tokio::test]
    async fn ack_flushes_after_eight_accepts() {
        let (sess, peer) = session_with_one_link().await;
        // 连续 push 8 条，第 8 条后应立即发出一条 AckFrame 回对端。
        for i in 1..=8u32 {
            peer.write(&encode_message(i, &[i as u8])).await.unwrap();
        }
        for _ in 0..8 {
            sess.recv().await.unwrap();
        }
        // 第 9 次 recv 进入前 maybe_flush 应已触发；再驱动一下让 flush 后发出的
        // ack 落到对端。给一点时间异步处理。
        tokio::time::sleep(Duration::from_millis(5)).await;
        let mut received_ack = None;
        // 对端应至少收到一帧；它可能是 AckFrame。
        let got = tokio::time::timeout(Duration::from_millis(50), peer.read())
            .await
            .unwrap()
            .unwrap();
        let frame = decode_frame(&got).unwrap();
        let (_, _, mt) = decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Ack);
        let ack = decode_ack(&frame.payload).unwrap();
        received_ack = Some(ack);
        let ack = received_ack.unwrap();
        assert_eq!(ack.ack, 8, "cumulative ack should be last observed seq");
    }

    #[tokio::test]
    async fn ack_flushes_after_delay_ms() {
        let (sess, peer) = session_with_one_link().await;
        peer.write(&encode_message(1, b"a")).await.unwrap();
        // 先拿到第一条消息，累积 ack count=1。
        let _ = sess.recv().await.unwrap();
        // 主动等 > ACK_MAX_DELAY_MS，再调一次 recv —— 主循环 tick 应触发 flush。
        tokio::time::sleep(Duration::from_millis(ACK_MAX_DELAY_MS + 10)).await;
        // 用一个"永远 pending"的场景：不再 push，recv 会 select 到 ack 截止后 continue。
        // 为避免 recv 无限阻塞，我们 spawn 一下然后 timeout。
        let sess_cl = Arc::clone(&sess);
        let h = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_millis(30), sess_cl.recv()).await
        });
        // 给 driver 一点时间 flush
        tokio::time::sleep(Duration::from_millis(10)).await;
        h.abort();
        let got = tokio::time::timeout(Duration::from_millis(50), peer.read())
            .await
            .expect("ack frame should arrive")
            .unwrap();
        let frame = decode_frame(&got).unwrap();
        let (_, _, mt) = decode_flags(frame.flags);
        assert_eq!(mt, MessageType::Ack);
    }

    #[tokio::test]
    async fn on_ack_clears_unacked_ring() {
        let (sess, _peer) = session_with_one_link().await;
        {
            use crate::core::session::types::PendingFrame;
            let mut ring = sess.unacked.lock().await;
            for s in 1..=5u32 {
                ring.push(
                    s,
                    PendingFrame {
                        bytes: vec![s as u8],
                        msg_type: MessageType::Message,
                    },
                );
            }
        }
        sess.on_ack(&AckFrame {
            ack: 3,
            bitmap: 0,
        })
        .await;
        assert_eq!(sess.unacked.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn no_links_returns_no_healthy_link() {
        let sess = Arc::new(Session::new("peer-y".into(), [0u8; 16]));
        let err = sess.recv().await.unwrap_err();
        assert!(matches!(err, MlinkError::NoHealthyLink { .. }));
    }

    #[tokio::test]
    async fn link_recv_error_triggers_linkremoved_event_and_fails() {
        let (sess, peer) = session_with_one_link().await;
        let mut ev = sess.events.subscribe();
        // 关掉对端 → 本端下次 read 返回 Err。
        peer.close().await.unwrap();
        let err = tokio::time::timeout(Duration::from_millis(500), sess.recv())
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, MlinkError::NoHealthyLink { .. }));
        // 先前应 emit 过 LinkRemoved
        let got = tokio::time::timeout(Duration::from_millis(50), ev.recv())
            .await
            .expect("event")
            .unwrap();
        match got {
            SessionEvent::LinkRemoved { reason, .. } => assert_eq!(reason, "recv_error"),
            other => panic!("unexpected event: {:?}", other),
        }
    }

    #[tokio::test]
    async fn multi_link_reads_from_faster_source() {
        // 两条 link，向其中一条注入一帧 —— recv 应返回之。
        let (a1, b1) = mock_pair();
        let (a2, _b2) = mock_pair();
        let sess = Arc::new(Session::new("peer-m".into(), [0u8; 16]));
        let l1 = Arc::new(Link::new(
            "mock:1".into(),
            Arc::new(a1),
            TransportKind::Mock,
            caps(1_000_000),
        ));
        let l2 = Arc::new(Link::new(
            "mock:2".into(),
            Arc::new(a2),
            TransportKind::Mock,
            caps(500_000),
        ));
        sess.links.write().await.extend([l1.clone(), l2.clone()]);
        *sess.active.write().await = Some(l1.id().to_string());
        b1.write(&encode_message(42, b"via-l1")).await.unwrap();
        let (seq, body) = sess.recv().await.unwrap();
        assert_eq!(seq, 42);
        assert_eq!(body, b"via-l1");
    }

    #[test]
    fn ack_should_flush_honors_count_threshold() {
        let p = AckPending {
            count: ACK_MAX_PENDING as u32,
            first_pending_at: Some(Instant::now()),
        };
        assert!(ack_should_flush(&p));
    }

    #[test]
    fn ack_should_flush_false_when_empty() {
        let p = AckPending::default();
        assert!(!ack_should_flush(&p));
    }
}
