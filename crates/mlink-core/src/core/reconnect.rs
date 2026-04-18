use std::collections::HashMap;
use std::time::{Duration, Instant};

const FOREGROUND_MAX_DELAY: Duration = Duration::from_secs(60);
const FOREGROUND_DURATION: Duration = Duration::from_secs(5 * 60);
const BACKGROUND_INTERVAL: Duration = Duration::from_secs(120);
const BACKGROUND_DURATION: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectMode {
    Foreground,
    Background,
}

#[derive(Debug, Clone)]
pub struct ReconnectState {
    pub mode: ReconnectMode,
    pub attempt: u32,
    pub started_at: Instant,
    pub last_attempt: Option<Instant>,
}

impl ReconnectState {
    fn new() -> Self {
        Self {
            mode: ReconnectMode::Foreground,
            attempt: 0,
            started_at: Instant::now(),
            last_attempt: None,
        }
    }
}

pub struct ReconnectPolicy {
    state: ReconnectState,
    background_started_at: Option<Instant>,
}

impl ReconnectPolicy {
    pub fn new() -> Self {
        Self {
            state: ReconnectState::new(),
            background_started_at: None,
        }
    }

    pub fn next_delay(&mut self) -> Option<Duration> {
        let now = Instant::now();

        if self.state.mode == ReconnectMode::Foreground
            && now.duration_since(self.state.started_at) >= FOREGROUND_DURATION
        {
            self.state.mode = ReconnectMode::Background;
            self.state.attempt = 0;
            self.background_started_at = Some(now);
        }

        let delay = match self.state.mode {
            ReconnectMode::Foreground => {
                let d = if self.state.attempt == 0 {
                    Duration::ZERO
                } else {
                    let secs = 1u64.checked_shl(self.state.attempt - 1).unwrap_or(u64::MAX);
                    Duration::from_secs(secs).min(FOREGROUND_MAX_DELAY)
                };
                Some(d)
            }
            ReconnectMode::Background => {
                let bg_start = self.background_started_at.unwrap_or(now);
                if now.duration_since(bg_start) >= BACKGROUND_DURATION {
                    None
                } else {
                    Some(BACKGROUND_INTERVAL)
                }
            }
        };

        if delay.is_some() {
            self.state.attempt = self.state.attempt.saturating_add(1);
            self.state.last_attempt = Some(now);
        }

        delay
    }

    pub fn reset(&mut self) {
        self.state = ReconnectState::new();
        self.background_started_at = None;
    }

    pub fn mode(&self) -> ReconnectMode {
        self.state.mode
    }

    pub fn attempt(&self) -> u32 {
        self.state.attempt
    }
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct StreamProgress {
    pub stream_id: u16,
    pub received_bitmap: Vec<u8>,
    pub total_chunks: u32,
}

pub struct ReconnectManager {
    pending_streams: HashMap<u16, StreamProgress>,
}

impl ReconnectManager {
    pub fn new() -> Self {
        Self {
            pending_streams: HashMap::new(),
        }
    }

    pub fn record_stream_progress(&mut self, progress: StreamProgress) {
        self.pending_streams.insert(progress.stream_id, progress);
    }

    pub fn take_resume_streams(&mut self) -> Vec<StreamProgress> {
        self.pending_streams.drain().map(|(_, v)| v).collect()
    }

    pub fn clear(&mut self) {
        self.pending_streams.clear();
    }
}

impl Default for ReconnectManager {
    fn default() -> Self {
        Self::new()
    }
}
