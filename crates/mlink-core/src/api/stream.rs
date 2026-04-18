use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use crate::core::connection::Role;
use crate::core::node::Node;
use crate::protocol::codec::{decode, encode};
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::types::{MessageType, StreamInfo};

pub const DEFAULT_CHUNK_SIZE: usize = 256;
pub const DEFAULT_CHECKSUM_ALGO: &str = "xor8";

static CENTRAL_COUNTER: AtomicU16 = AtomicU16::new(0);
static PERIPHERAL_COUNTER: AtomicU16 = AtomicU16::new(1);

pub fn next_stream_id_central() -> u16 {
    CENTRAL_COUNTER.fetch_add(2, Ordering::SeqCst)
}

pub fn next_stream_id_peripheral() -> u16 {
    PERIPHERAL_COUNTER.fetch_add(2, Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn reset_stream_counters_for_test() {
    CENTRAL_COUNTER.store(0, Ordering::SeqCst);
    PERIPHERAL_COUNTER.store(1, Ordering::SeqCst);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    pub received: u32,
    pub total: u32,
    pub percent: f32,
}

impl Progress {
    fn new(received: u32, total: u32) -> Self {
        let percent = if total == 0 {
            100.0
        } else {
            (received as f32 / total as f32) * 100.0
        };
        Self {
            received,
            total,
            percent,
        }
    }
}

fn xor8_checksum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |acc, b| acc ^ *b)
}

pub struct StreamWriter {
    node: Arc<Node>,
    peer_id: String,
    stream_id: u16,
    chunk_size: usize,
    chunk_index: u32,
    total_chunks: u32,
    checksum: u8,
    finished: bool,
}

impl StreamWriter {
    pub fn stream_id(&self) -> u16 {
        self.stream_id
    }

    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    pub fn total_chunks(&self) -> u32 {
        self.total_chunks
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        if self.finished {
            return Err(MlinkError::HandlerError(
                "stream already finished".into(),
            ));
        }
        for chunk in data.chunks(self.chunk_size) {
            self.checksum ^= xor8_checksum(chunk);
            let payload = encode(&StreamChunkMsg {
                stream_id: self.stream_id,
                chunk_index: self.chunk_index,
                data: chunk.to_vec(),
            })?;
            self.node
                .send_raw(&self.peer_id, MessageType::StreamChunk, &payload)
                .await?;
            self.chunk_index = self.chunk_index.saturating_add(1);
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let payload = encode(&StreamEndMsg {
            stream_id: self.stream_id,
            checksum: self.checksum,
        })?;
        self.node
            .send_raw(&self.peer_id, MessageType::StreamEnd, &payload)
            .await
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StreamChunkMsg {
    pub stream_id: u16,
    pub chunk_index: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct StreamEndMsg {
    pub stream_id: u16,
    pub checksum: u8,
}

pub struct StreamReader {
    node: Arc<Node>,
    peer_id: String,
    stream_id: u16,
    total_chunks: u32,
    received: u32,
    done: bool,
}

impl StreamReader {
    pub fn new(node: Arc<Node>, peer_id: String, info: StreamInfo) -> Self {
        Self {
            node,
            peer_id,
            stream_id: info.stream_id,
            total_chunks: info.total_chunks,
            received: 0,
            done: false,
        }
    }

    pub fn stream_id(&self) -> u16 {
        self.stream_id
    }

    pub fn total_chunks(&self) -> u32 {
        self.total_chunks
    }

    pub async fn next(&mut self) -> Option<(Vec<u8>, Progress)> {
        if self.done {
            return None;
        }
        loop {
            let (frame, payload) = match self.node.recv_raw(&self.peer_id).await {
                Ok(v) => v,
                Err(_) => {
                    self.done = true;
                    return None;
                }
            };
            let (_c, _e, mt) = crate::protocol::types::decode_flags(frame.flags);
            match mt {
                MessageType::StreamChunk => {
                    let msg: StreamChunkMsg = match decode(&payload) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    if msg.stream_id != self.stream_id {
                        continue;
                    }
                    self.received = self.received.saturating_add(1);
                    let progress = Progress::new(self.received, self.total_chunks);
                    return Some((msg.data, progress));
                }
                MessageType::StreamEnd => {
                    let msg: std::result::Result<StreamEndMsg, _> = decode(&payload);
                    if let Ok(m) = msg {
                        if m.stream_id == self.stream_id {
                            self.done = true;
                            return None;
                        }
                    }
                }
                _ => continue,
            }
        }
    }
}

pub async fn create_stream(
    node: Arc<Node>,
    peer_id: &str,
    data: &[u8],
) -> Result<StreamWriter> {
    create_stream_with_chunk_size(node, peer_id, data, DEFAULT_CHUNK_SIZE).await
}

pub async fn create_stream_with_chunk_size(
    node: Arc<Node>,
    peer_id: &str,
    data: &[u8],
    chunk_size: usize,
) -> Result<StreamWriter> {
    if chunk_size == 0 {
        return Err(MlinkError::HandlerError("chunk_size must be > 0".into()));
    }

    let role = node.role_for(peer_id);
    let stream_id = match role {
        Role::Central => next_stream_id_central(),
        Role::Peripheral => next_stream_id_peripheral(),
    };

    let total_size = data.len() as u64;
    let total_chunks = if data.is_empty() {
        0
    } else {
        ((data.len() + chunk_size - 1) / chunk_size) as u32
    };

    let info = StreamInfo {
        stream_id,
        total_chunks,
        total_size,
        checksum_algo: DEFAULT_CHECKSUM_ALGO.to_string(),
    };
    let payload = encode(&info)?;
    node.send_raw(peer_id, MessageType::StreamStart, &payload)
        .await?;

    Ok(StreamWriter {
        node,
        peer_id: peer_id.to_string(),
        stream_id,
        chunk_size,
        chunk_index: 0,
        total_chunks,
        checksum: 0,
        finished: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_percent_basic() {
        let p = Progress::new(5, 10);
        assert_eq!(p.received, 5);
        assert_eq!(p.total, 10);
        assert!((p.percent - 50.0).abs() < 0.01);
    }

    #[test]
    fn progress_zero_total_is_full() {
        let p = Progress::new(0, 0);
        assert!((p.percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn progress_full() {
        let p = Progress::new(10, 10);
        assert!((p.percent - 100.0).abs() < 0.01);
    }

    #[test]
    fn xor8_checksum_basic() {
        assert_eq!(xor8_checksum(&[]), 0);
        assert_eq!(xor8_checksum(&[0xAA]), 0xAA);
        assert_eq!(xor8_checksum(&[0xFF, 0x0F]), 0xF0);
    }

    #[test]
    fn central_counter_steps_by_two_even() {
        reset_stream_counters_for_test();
        let a = next_stream_id_central();
        let b = next_stream_id_central();
        let c = next_stream_id_central();
        assert_eq!(a % 2, 0);
        assert_eq!(b, a + 2);
        assert_eq!(c, b + 2);
    }

    #[test]
    fn peripheral_counter_steps_by_two_odd() {
        reset_stream_counters_for_test();
        let a = next_stream_id_peripheral();
        let b = next_stream_id_peripheral();
        assert_eq!(a % 2, 1);
        assert_eq!(b, a + 2);
    }

    #[test]
    fn stream_chunk_msg_serde() {
        let m = StreamChunkMsg {
            stream_id: 7,
            chunk_index: 3,
            data: vec![1, 2, 3],
        };
        let bytes = encode(&m).unwrap();
        let back: StreamChunkMsg = decode(&bytes).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn stream_end_msg_serde() {
        let m = StreamEndMsg { stream_id: 7, checksum: 0xAB };
        let bytes = encode(&m).unwrap();
        let back: StreamEndMsg = decode(&bytes).unwrap();
        assert_eq!(m, back);
    }
}
