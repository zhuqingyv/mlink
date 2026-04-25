//! Session-level u32 seq generator + 128-wide dedup window.
//!
//! 与 `Frame.seq: u16` 无关 —— 这里是 dual-transport session 层独立线协议，
//! 配合 `protocol::ack::AckFrame` 使用。

/// u32 递增 seq 生成器。wrap 在 u32::MAX 后自然回到 0，不做复用检测
/// （短周期内 4e9 条帧已远超任何现实场景）。
pub struct SeqGen {
    next: u32,
}

impl SeqGen {
    pub fn new() -> Self {
        Self { next: 0 }
    }

    /// 返回当前 seq，内部 `wrapping_add(1)` 前进。首次调用返回 0。
    pub fn next(&mut self) -> u32 {
        let v = self.next;
        self.next = self.next.wrapping_add(1);
        v
    }
}

impl Default for SeqGen {
    fn default() -> Self {
        Self::new()
    }
}

/// 观察结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// 新 seq，已登记。
    Accept,
    /// 窗口内命中 —— 已经见过。
    Duplicate,
    /// 落在窗口之外（太旧），应丢弃且不 ack。
    Stale,
}

/// 128 宽度的 dedup 窗口。
///
/// `last` 永远是观察到的最大 seq（wrap 语义），`bits` bit `i` 表示
/// `last.wrapping_sub(i)` 是否已见；bit 0 在至少一次 observe 之后恒为 1。
///
/// 空窗口（new 刚创建、尚未 observe）内部用 `seen=false` 区分；对外表现为
/// "任意 seq 都 Accept 一次"。
pub struct DedupWindow {
    last: u32,
    bits: u128,
    seen: bool,
}

impl DedupWindow {
    pub fn new() -> Self {
        Self { last: 0, bits: 0, seen: false }
    }

    /// 观察一个 seq。语义见 [`Observation`]。
    ///
    /// wrap：用 `seq.wrapping_sub(last)` 判方向，差值 < 2^31 视为前进。
    pub fn observe(&mut self, seq: u32) -> Observation {
        if !self.seen {
            self.last = seq;
            self.bits = 1;
            self.seen = true;
            return Observation::Accept;
        }

        let forward = seq.wrapping_sub(self.last);
        // 前进视窗：差值在 [1, 2^31)
        if forward != 0 && forward < 0x8000_0000 {
            if forward >= 128 {
                self.bits = 1;
            } else {
                self.bits = (self.bits << forward) | 1;
            }
            self.last = seq;
            return Observation::Accept;
        }

        if forward == 0 {
            return Observation::Duplicate;
        }

        // 后退：back = last - seq（wrap 感知，1..=2^31）
        let back = self.last.wrapping_sub(seq);
        if back >= 128 {
            return Observation::Stale;
        }
        let mask: u128 = 1u128 << back;
        if self.bits & mask != 0 {
            Observation::Duplicate
        } else {
            self.bits |= mask;
            Observation::Accept
        }
    }

    /// 返回 `(last, bits)` 快照；`bits` bit `i` 对应 `last.wrapping_sub(i)`。
    /// 空窗口返回 `(0, 0)`。
    pub fn snapshot(&self) -> (u32, u128) {
        if !self.seen {
            (0, 0)
        } else {
            (self.last, self.bits)
        }
    }
}

impl Default for DedupWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqgen_monotonic() {
        let mut g = SeqGen::new();
        assert_eq!(g.next(), 0);
        assert_eq!(g.next(), 1);
        assert_eq!(g.next(), 2);
    }

    #[test]
    fn seqgen_wrap() {
        let mut g = SeqGen::new();
        // 直接跳到接近 wrap 的位置
        g.next = u32::MAX;
        assert_eq!(g.next(), u32::MAX);
        assert_eq!(g.next(), 0);
        assert_eq!(g.next(), 1);
    }

    #[test]
    fn dedup_first_observation_accept() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(42), Observation::Accept);
    }

    #[test]
    fn dedup_forward_seqs_accept() {
        let mut w = DedupWindow::new();
        for s in 0..10 {
            assert_eq!(w.observe(s), Observation::Accept, "seq {}", s);
        }
    }

    #[test]
    fn dedup_duplicate_detection() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(5), Observation::Accept);
        assert_eq!(w.observe(5), Observation::Duplicate);
        assert_eq!(w.observe(6), Observation::Accept);
        assert_eq!(w.observe(5), Observation::Duplicate);
        assert_eq!(w.observe(6), Observation::Duplicate);
    }

    #[test]
    fn dedup_out_of_order_within_window_accept_once() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(10), Observation::Accept);
        // 10 之前乱序到达
        assert_eq!(w.observe(7), Observation::Accept);
        assert_eq!(w.observe(7), Observation::Duplicate);
        assert_eq!(w.observe(9), Observation::Accept);
        assert_eq!(w.observe(8), Observation::Accept);
        // last 仍为 10
        let (last, _) = w.snapshot();
        assert_eq!(last, 10);
    }

    #[test]
    fn dedup_window_slide_evicts_old() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(0), Observation::Accept);
        // 推进超过 128
        assert_eq!(w.observe(200), Observation::Accept);
        // 旧 seq 应为 Stale（last=200，seq=0，距离 200 > 127）
        assert_eq!(w.observe(0), Observation::Stale);
        // 仍在窗内（200-127 = 73）应可处理
        assert_eq!(w.observe(73), Observation::Accept);
        assert_eq!(w.observe(72), Observation::Stale);
    }

    #[test]
    fn dedup_jump_exactly_128_forward() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(0), Observation::Accept);
        // 跳 128：旧的 0 应当被淘汰（0 距 last=128 正好 128，算窗外）
        assert_eq!(w.observe(128), Observation::Accept);
        assert_eq!(w.observe(0), Observation::Stale);
        // 1..127 在 shift 后的 bits 里为 0（未见），仍落在窗内 [1,128]，
        // 首次到达应 Accept（out-of-order 合法），第二次才 Duplicate。
        assert_eq!(w.observe(1), Observation::Accept);
        assert_eq!(w.observe(1), Observation::Duplicate);
        // 窗外更旧的 seq 仍是 Stale（u32::MAX 距 last=128 为 129）。
        assert_eq!(w.observe(u32::MAX), Observation::Stale);
    }

    #[test]
    fn dedup_jump_127_forward_keeps_last() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(0), Observation::Accept);
        assert_eq!(w.observe(127), Observation::Accept);
        // 0 仍在窗内（距 127 恰好 127），应为 Duplicate
        assert_eq!(w.observe(0), Observation::Duplicate);
    }

    #[test]
    fn dedup_boundary_seq_zero() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(0), Observation::Accept);
        assert_eq!(w.observe(0), Observation::Duplicate);
    }

    #[test]
    fn dedup_boundary_seq_u32_max() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(u32::MAX), Observation::Accept);
        assert_eq!(w.observe(u32::MAX), Observation::Duplicate);
        // wrap 前进：u32::MAX → 0
        assert_eq!(w.observe(0), Observation::Accept);
        // u32::MAX 仍在窗内（0 - 1 = u32::MAX，back=1）
        assert_eq!(w.observe(u32::MAX), Observation::Duplicate);
    }

    #[test]
    fn dedup_wrap_around_forward() {
        let mut w = DedupWindow::new();
        assert_eq!(w.observe(u32::MAX - 2), Observation::Accept);
        assert_eq!(w.observe(u32::MAX - 1), Observation::Accept);
        assert_eq!(w.observe(u32::MAX), Observation::Accept);
        assert_eq!(w.observe(0), Observation::Accept); // wrap
        assert_eq!(w.observe(1), Observation::Accept);
        // 旧的仍在窗内应 Duplicate
        assert_eq!(w.observe(u32::MAX), Observation::Duplicate);
        assert_eq!(w.observe(u32::MAX - 2), Observation::Duplicate);
    }

    #[test]
    fn snapshot_empty_window() {
        let w = DedupWindow::new();
        assert_eq!(w.snapshot(), (0, 0));
    }

    #[test]
    fn snapshot_reflects_last_and_bits() {
        let mut w = DedupWindow::new();
        w.observe(5);
        w.observe(7);
        w.observe(8);
        let (last, bits) = w.snapshot();
        assert_eq!(last, 8);
        // bit0 = last=8, bit1 = 7, bit3 = 5
        assert_eq!(bits & 1, 1, "bit0 (seq 8)");
        assert_eq!((bits >> 1) & 1, 1, "bit1 (seq 7)");
        assert_eq!((bits >> 2) & 1, 0, "bit2 (seq 6, not seen)");
        assert_eq!((bits >> 3) & 1, 1, "bit3 (seq 5)");
    }

    #[test]
    fn dedup_very_far_back_is_stale_not_duplicate() {
        let mut w = DedupWindow::new();
        w.observe(1000);
        // 远远在窗口外
        assert_eq!(w.observe(10), Observation::Stale);
        // 确认不会污染 bits
        assert_eq!(w.observe(1000), Observation::Duplicate);
    }
}
