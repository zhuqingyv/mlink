use crate::core::link::{Link, LinkHealth, TransportKind};
use crate::protocol::ack::UnackedRing;
use crate::protocol::seq::{DedupWindow, SeqGen};
use crate::protocol::types::MessageType;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{broadcast, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkRole {
    Active,
    Standby,
}

#[derive(Debug, Clone)]
pub struct LinkStatus {
    pub link_id: String,
    pub kind: TransportKind,
    pub role: LinkRole,
    pub health: LinkHealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchCause {
    ActiveFailed,
    BetterCandidate,
    ManualUserRequest,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    LinkAdded {
        peer_id: String,
        link_id: String,
        kind: TransportKind,
    },
    LinkRemoved {
        peer_id: String,
        link_id: String,
        reason: String,
    },
    Switched {
        peer_id: String,
        from: String,
        to: String,
        cause: SwitchCause,
    },
    StreamProgress {
        peer_id: String,
        stream_id: u16,
        pct: u8,
    },
}

#[derive(Debug, Clone)]
pub struct PendingFrame {
    pub bytes: Vec<u8>,
    pub msg_type: MessageType,
}

#[derive(Debug, Default)]
pub struct SchedulerState {
    pub better_streak: HashMap<String, u8>,
    pub tick: u32,
}

/// 延迟 ACK 累积状态（W2-B reader 维护）。
///
/// 触发发送的阈值：连续 Accept 到 `ACK_MAX_PENDING` 条，或自 `first_pending_at`
/// 起过了 `ACK_MAX_DELAY_MS` 毫秒。任何一条满足即刷。
#[derive(Debug, Default)]
pub struct AckPending {
    /// 自上次发送 Ack 以来累计的 Accept 条数。
    pub count: u32,
    /// 第一条未 ack 帧的到达时刻；count=0 时为 None。
    pub first_pending_at: Option<Instant>,
}

#[allow(dead_code)]
pub struct Session {
    pub peer_id: String,
    pub session_id: [u8; 16],
    pub(crate) links: RwLock<Vec<Arc<Link>>>,
    pub(crate) active: RwLock<Option<String>>,
    pub(crate) send_seq: Mutex<SeqGen>,
    pub(crate) dedup: Mutex<DedupWindow>,
    pub(crate) unacked: Mutex<UnackedRing<PendingFrame>>,
    pub(crate) events: broadcast::Sender<SessionEvent>,
    pub(crate) sched_state: Mutex<SchedulerState>,
    /// 延迟 ACK 累积（W2-B reader 使用）。
    pub(crate) ack_pending: Mutex<AckPending>,
}

impl Session {
    /// 构造一个空的 Session —— 暂不挂任何 link。后续由 SessionManager.attach_link
    /// 填入；或测试里直接 push 进 `links`。
    pub fn new(peer_id: String, session_id: [u8; 16]) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            peer_id,
            session_id,
            links: RwLock::new(Vec::new()),
            active: RwLock::new(None),
            send_seq: Mutex::new(SeqGen::new()),
            dedup: Mutex::new(DedupWindow::new()),
            unacked: Mutex::new(UnackedRing::new()),
            events,
            sched_state: Mutex::new(SchedulerState::default()),
            ack_pending: Mutex::new(AckPending::default()),
        }
    }
}

pub const MAX_LINKS_PER_PEER: usize = 4;
pub const SWITCHBACK_HYSTERESIS: u8 = 3;
pub const ACK_MAX_DELAY_MS: u64 = 50;
pub const ACK_MAX_PENDING: usize = 8;
pub const EXPLICIT_OVERRIDE_TTL_SECS: u64 = 600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_contract() {
        assert_eq!(MAX_LINKS_PER_PEER, 4);
        assert_eq!(SWITCHBACK_HYSTERESIS, 3);
        assert_eq!(ACK_MAX_DELAY_MS, 50);
        assert_eq!(ACK_MAX_PENDING, 8);
        assert_eq!(EXPLICIT_OVERRIDE_TTL_SECS, 600);
    }

    #[test]
    fn scheduler_state_default_is_empty() {
        let s = SchedulerState::default();
        assert!(s.better_streak.is_empty());
        assert_eq!(s.tick, 0);
    }

    #[test]
    fn session_event_is_clone() {
        let ev = SessionEvent::LinkAdded {
            peer_id: "p".into(),
            link_id: "l".into(),
            kind: TransportKind::Tcp,
        };
        let ev2 = ev.clone();
        match ev2 {
            SessionEvent::LinkAdded { peer_id, .. } => assert_eq!(peer_id, "p"),
            _ => panic!("clone variant mismatch"),
        }
    }
}
