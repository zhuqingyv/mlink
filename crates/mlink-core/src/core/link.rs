use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::protocol::errors::Result;
use crate::transport::{Connection, TransportCapabilities};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    Ble,
    Tcp,
    Ipc,
    Mock,
}

impl TransportKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            TransportKind::Ble => "ble",
            TransportKind::Tcp => "tcp",
            TransportKind::Ipc => "ipc",
            TransportKind::Mock => "mock",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "ble" => Some(TransportKind::Ble),
            "tcp" => Some(TransportKind::Tcp),
            "ipc" => Some(TransportKind::Ipc),
            "mock" => Some(TransportKind::Mock),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LinkHealth {
    pub rtt_ema_us: u32,
    pub err_rate_milli: u16,
    pub last_ok_at_ms: u64,
    pub inflight: u16,
}

/// EMA shift (alpha = 1/8).
const EMA_SHIFT: u32 = 3;
pub const LINK_STALE_MS: u64 = 5_000;
pub const LINK_UNHEALTHY_ERR_MILLI: u16 = 500;

pub struct Link {
    id: String,
    kind: TransportKind,
    caps: TransportCapabilities,
    conn: Arc<dyn Connection>,
    health: Mutex<LinkHealth>,
    created_at: Instant,
}

impl Link {
    pub fn new(
        id: String,
        conn: Arc<dyn Connection>,
        kind: TransportKind,
        caps: TransportCapabilities,
    ) -> Self {
        Self {
            id,
            kind,
            caps,
            conn,
            health: Mutex::new(LinkHealth::default()),
            created_at: Instant::now(),
        }
    }

    pub fn id(&self) -> &str { &self.id }
    pub fn kind(&self) -> TransportKind { self.kind }
    pub fn caps(&self) -> &TransportCapabilities { &self.caps }
    pub fn conn(&self) -> &Arc<dyn Connection> { &self.conn }

    /// 拷贝快照，短持锁。不得跨 await 持有返回值。
    pub fn health(&self) -> LinkHealth {
        *self.health.lock().expect("link health poisoned")
    }

    fn now_ms(&self) -> u64 { self.created_at.elapsed().as_millis() as u64 }

    pub fn record_rtt(&self, rtt_us: u32) {
        let now_ms = self.now_ms();
        let mut h = self.health.lock().expect("link health poisoned");
        h.rtt_ema_us = ema_u32(h.rtt_ema_us, rtt_us);
        h.err_rate_milli = ema_err(h.err_rate_milli, 0);
        h.last_ok_at_ms = now_ms;
    }

    pub fn record_success(&self) {
        let now_ms = self.now_ms();
        let mut h = self.health.lock().expect("link health poisoned");
        h.err_rate_milli = ema_err(h.err_rate_milli, 0);
        h.last_ok_at_ms = now_ms;
    }

    pub fn record_error(&self) {
        let mut h = self.health.lock().expect("link health poisoned");
        h.err_rate_milli = ema_err(h.err_rate_milli, 1000);
    }

    pub fn inc_inflight(&self) {
        let mut h = self.health.lock().expect("link health poisoned");
        h.inflight = h.inflight.saturating_add(1);
    }

    pub fn dec_inflight(&self) {
        let mut h = self.health.lock().expect("link health poisoned");
        h.inflight = h.inflight.saturating_sub(1);
    }

    pub fn score(&self) -> i64 {
        score_from(&self.caps, &self.health())
    }

    pub fn is_healthy(&self) -> bool {
        let h = self.health();
        if h.err_rate_milli >= LINK_UNHEALTHY_ERR_MILLI {
            return false;
        }
        // stale 判定只在至少有过 rtt 样本后生效，避免刚建 link 就被判死。
        if h.rtt_ema_us > 0 && self.now_ms().saturating_sub(h.last_ok_at_ms) >= LINK_STALE_MS {
            return false;
        }
        true
    }

    // 先 clone Arc 再 await，遵循 project_connection_ownership_pattern。
    pub async fn send(&self, bytes: &[u8]) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        match conn.write(bytes).await {
            Ok(()) => { self.record_success(); Ok(()) }
            Err(e) => { self.record_error(); Err(e) }
        }
    }

    pub async fn recv(&self) -> Result<Vec<u8>> {
        let conn = Arc::clone(&self.conn);
        match conn.read().await {
            Ok(v) => { self.record_success(); Ok(v) }
            Err(e) => { self.record_error(); Err(e) }
        }
    }

    pub async fn close(&self) {
        let conn = Arc::clone(&self.conn);
        let _ = conn.close().await;
    }
}

fn ema_u32(prev: u32, sample: u32) -> u32 {
    if prev == 0 { return sample; }
    let diff = sample as i64 - prev as i64;
    (prev as i64 + (diff >> EMA_SHIFT)).clamp(0, u32::MAX as i64) as u32
}

fn ema_err(prev: u16, sample: u16) -> u16 {
    let diff = sample as i32 - prev as i32;
    (prev as i32 + (diff >> EMA_SHIFT)).clamp(0, 1000) as u16
}

pub(crate) fn score_from(caps: &TransportCapabilities, h: &LinkHealth) -> i64 {
    let tp = caps.throughput_bps.min(i64::MAX as u64) as i64;
    let success_factor = 1000 - h.err_rate_milli as i64;
    let base = tp.saturating_mul(success_factor) / 1000;
    base.saturating_sub(h.rtt_ema_us as i64 / 10)
}
