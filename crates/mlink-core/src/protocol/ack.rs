use crate::protocol::errors::{MlinkError, Result};

use std::collections::VecDeque;

pub const ACK_FRAME_BYTES: usize = 20;
pub const UNACKED_RING_CAP: usize = 256;
pub const SACK_WINDOW: u32 = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AckFrame {
    pub ack: u32,
    pub bitmap: u128,
}

pub fn encode_ack(a: &AckFrame) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ACK_FRAME_BYTES);
    buf.extend_from_slice(&a.ack.to_be_bytes());
    buf.extend_from_slice(&a.bitmap.to_be_bytes());
    buf
}

pub fn decode_ack(b: &[u8]) -> Result<AckFrame> {
    if b.len() < ACK_FRAME_BYTES {
        return Err(MlinkError::CodecError(format!(
            "ack frame too short: {} < {}",
            b.len(),
            ACK_FRAME_BYTES
        )));
    }
    let ack = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let mut bm = [0u8; 16];
    bm.copy_from_slice(&b[4..20]);
    let bitmap = u128::from_be_bytes(bm);
    Ok(AckFrame { ack, bitmap })
}

/// seq_after(a, b) → true iff `a` is logically "after or equal to" `b` under u32 wrap.
#[inline]
fn seq_geq(a: u32, b: u32) -> bool {
    a.wrapping_sub(b) < 0x8000_0000
}

pub struct UnackedRing<T: Clone + Send + Sync + 'static> {
    buf: VecDeque<(u32, T)>,
}

impl<T: Clone + Send + Sync + 'static> Default for UnackedRing<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + Send + Sync + 'static> UnackedRing<T> {
    pub fn new() -> Self {
        Self {
            buf: VecDeque::with_capacity(UNACKED_RING_CAP),
        }
    }

    /// Append a (seq, item). Returns the evicted oldest entry when full;
    /// callers are expected to emit a warn-level log on eviction.
    pub fn push(&mut self, seq: u32, item: T) -> Option<(u32, T)> {
        let evicted = if self.buf.len() >= UNACKED_RING_CAP {
            self.buf.pop_front()
        } else {
            None
        };
        self.buf.push_back((seq, item));
        evicted
    }

    /// Cumulative ack: remove all entries whose seq is `<= ack` under u32-wrap order.
    pub fn ack_cum(&mut self, ack: u32) {
        self.buf.retain(|(s, _)| !seq_geq(ack, *s));
    }

    /// Selective ack: clear entries in (base, base+SACK_WINDOW] whose bit is set.
    /// bit0 corresponds to seq = base+1.
    pub fn sack(&mut self, base: u32, bitmap: u128) {
        if bitmap == 0 {
            return;
        }
        self.buf.retain(|(s, _)| {
            let d = s.wrapping_sub(base);
            if d == 0 || d > SACK_WINDOW {
                return true;
            }
            let bit = d - 1;
            (bitmap >> bit) & 1 == 0
        });
    }

    /// Clone all entries with seq `>= since` under u32-wrap order; ring is unchanged.
    pub fn unacked_since(&self, since: u32) -> Vec<(u32, T)> {
        self.buf
            .iter()
            .filter(|(s, _)| seq_geq(*s, since))
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.buf.len() >= UNACKED_RING_CAP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let a = AckFrame {
            ack: 0xDEAD_BEEF,
            bitmap: (1u128 << 127) | (1u128 << 0) | 0x1234_5678_9ABC_DEF0,
        };
        let buf = encode_ack(&a);
        assert_eq!(buf.len(), ACK_FRAME_BYTES);
        let decoded = decode_ack(&buf).unwrap();
        assert_eq!(decoded, a);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        let short = [0u8; ACK_FRAME_BYTES - 1];
        let err = decode_ack(&short).unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[test]
    fn push_until_full_evicts_oldest() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        for i in 0..UNACKED_RING_CAP as u32 {
            assert!(r.push(i, i).is_none());
        }
        assert!(r.is_full());
        assert_eq!(r.len(), UNACKED_RING_CAP);

        let evicted = r.push(UNACKED_RING_CAP as u32, UNACKED_RING_CAP as u32);
        assert_eq!(evicted, Some((0, 0)));
        assert_eq!(r.len(), UNACKED_RING_CAP);

        // Oldest still in ring should now be (1, 1)
        let rem = r.unacked_since(0);
        assert_eq!(rem.first(), Some(&(1, 1)));
        assert_eq!(rem.last(), Some(&(UNACKED_RING_CAP as u32, UNACKED_RING_CAP as u32)));
    }

    #[test]
    fn ack_cum_releases_prefix() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        for i in 1..=10u32 {
            r.push(i, i);
        }
        r.ack_cum(5);
        assert_eq!(r.len(), 5);
        let rem = r.unacked_since(0);
        assert_eq!(rem.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![6, 7, 8, 9, 10]);
    }

    #[test]
    fn ack_cum_on_empty_noop() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        r.ack_cum(100);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn sack_selective_clear() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        for i in 1..=8u32 {
            r.push(i, i);
        }
        // base=0, bits: ack seq 2 (bit 1), 4 (bit 3), 7 (bit 6)
        let bitmap = (1u128 << 1) | (1u128 << 3) | (1u128 << 6);
        r.sack(0, bitmap);
        let rem: Vec<u32> = r.unacked_since(0).into_iter().map(|(s, _)| s).collect();
        assert_eq!(rem, vec![1, 3, 5, 6, 8]);
    }

    #[test]
    fn sack_zero_bitmap_noop() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        for i in 1..=4u32 {
            r.push(i, i);
        }
        r.sack(0, 0);
        assert_eq!(r.len(), 4);
    }

    #[test]
    fn sack_out_of_window_ignored() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        r.push(5, 5);
        r.push(200, 200);
        // base=10, bitmap bit 0 → seq 11 (no such seq → noop)
        r.sack(10, 1);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn unacked_since_filters_by_seq() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        for i in 1..=5u32 {
            r.push(i, i);
        }
        let since3: Vec<u32> = r.unacked_since(3).into_iter().map(|(s, _)| s).collect();
        assert_eq!(since3, vec![3, 4, 5]);

        // ring unchanged
        assert_eq!(r.len(), 5);
    }

    #[test]
    fn wrap_around_seq_space() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        let base = u32::MAX - 2;
        // Push seqs: MAX-2, MAX-1, MAX, 0, 1, 2 — crossing the u32 wrap.
        for s in [base, base + 1, base + 2, 0, 1, 2] {
            r.push(s, s);
        }
        assert_eq!(r.len(), 6);

        // Cumulative ack at MAX should clear the three pre-wrap entries.
        r.ack_cum(u32::MAX);
        let rem: Vec<u32> = r.unacked_since(0).into_iter().map(|(s, _)| s).collect();
        assert_eq!(rem, vec![0, 1, 2]);

        // SACK across wrap: base=MAX, bitmap bit0 = seq 0, bit2 = seq 2
        let mut r2: UnackedRing<u32> = UnackedRing::new();
        for s in [0u32, 1, 2] {
            r2.push(s, s);
        }
        let bm = (1u128 << 0) | (1u128 << 2);
        r2.sack(u32::MAX, bm);
        let rem2: Vec<u32> = r2.unacked_since(0).into_iter().map(|(s, _)| s).collect();
        assert_eq!(rem2, vec![1]);
    }

    #[test]
    fn empty_ring_operations() {
        let mut r: UnackedRing<u32> = UnackedRing::new();
        assert!(r.is_empty());
        assert!(!r.is_full());
        assert_eq!(r.len(), 0);
        assert_eq!(r.unacked_since(0).len(), 0);
        r.ack_cum(42);
        r.sack(0, u128::MAX);
        assert!(r.is_empty());
    }
}
