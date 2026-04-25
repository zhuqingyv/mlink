//! Dial/accept timeout + random retry delay used by the serve/chat/join
//! connect loops. Lives in its own module so the session/* helpers and the
//! commands/* glue can share the same constants without pulling the rest of
//! the CLI binary surface with them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Outer timeout for connect_peer / accept_incoming. Picked larger than the
/// 10s handshake IO ceiling in `perform_handshake` so an honest-but-slow peer
/// still completes, but small enough that a silent peer can't freeze the
/// main `select!` loop. If this fires, the dial/accept path unwinds via a
/// `HandlerError` and the normal retry-with-backoff path runs.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Random retry delay in the 1000-3000ms window. We derive the jitter from
/// the current wall-clock nanoseconds to avoid pulling in a `rand` dependency
/// for a single dice roll.
pub(crate) fn random_backoff() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // 1000..=3000 ms window → 2001 possible values.
    let ms = 1000u64 + (nanos as u64 % 2001);
    Duration::from_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_backoff_stays_within_window() {
        for _ in 0..50 {
            let d = random_backoff();
            assert!(d >= Duration::from_millis(1000));
            assert!(d <= Duration::from_millis(3000));
        }
    }
}
