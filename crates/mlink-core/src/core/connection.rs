use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

use crate::protocol::codec;
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::frame::{decode_frame, encode_frame};
use crate::protocol::types::{
    encode_flags, Frame, Handshake, MessageType, MAGIC, PROTOCOL_VERSION,
};
use crate::transport::Connection;

/// Per-direction ceiling on the handshake. A slow or silent peer must never
/// be allowed to freeze the caller's event loop (e.g. `cmd_serve`'s main
/// select!) — on timeout we bail with a `HandlerError` the caller can map to
/// a retry.
pub const HANDSHAKE_IO_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Central,
    Peripheral,
}

pub fn negotiate_role(my_uuid: &str, peer_uuid: &str) -> Role {
    match my_uuid.cmp(peer_uuid) {
        Ordering::Less => Role::Central,
        _ => Role::Peripheral,
    }
}

pub struct ConnectionManager {
    conns: HashMap<String, Box<dyn Connection>>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            conns: HashMap::new(),
        }
    }

    pub async fn add(&mut self, id: String, conn: Box<dyn Connection>) {
        self.conns.insert(id, conn);
    }

    pub async fn remove(&mut self, id: &str) -> Option<Box<dyn Connection>> {
        self.conns.remove(id)
    }

    pub async fn get(&self, id: &str) -> Option<&Box<dyn Connection>> {
        self.conns.get(id)
    }

    pub async fn get_mut(&mut self, id: &str) -> Option<&mut Box<dyn Connection>> {
        self.conns.get_mut(id)
    }

    pub fn list_ids(&self) -> Vec<String> {
        self.conns.keys().cloned().collect()
    }

    pub fn count(&self) -> usize {
        self.conns.len()
    }
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn perform_handshake(
    conn: &mut dyn Connection,
    local_handshake: &Handshake,
) -> Result<Handshake> {
    let payload = codec::encode(local_handshake)?;
    let length = u16::try_from(payload.len()).map_err(|_| MlinkError::PayloadTooLarge {
        size: payload.len(),
        max: u16::MAX as usize,
    })?;
    let frame = Frame {
        magic: MAGIC,
        version: PROTOCOL_VERSION,
        flags: encode_flags(false, false, MessageType::Handshake),
        seq: 0,
        length,
        payload,
    };
    tokio::time::timeout(HANDSHAKE_IO_TIMEOUT, conn.write(&encode_frame(&frame)))
        .await
        .map_err(|_| {
            MlinkError::HandlerError(format!(
                "handshake write timed out after {:?}",
                HANDSHAKE_IO_TIMEOUT
            ))
        })??;

    let bytes = tokio::time::timeout(HANDSHAKE_IO_TIMEOUT, conn.read())
        .await
        .map_err(|_| {
            MlinkError::HandlerError(format!(
                "handshake read timed out after {:?}",
                HANDSHAKE_IO_TIMEOUT
            ))
        })??;
    let recv_frame = decode_frame(&bytes)?;
    let (_, _, msg_type) = crate::protocol::types::decode_flags(recv_frame.flags);
    if msg_type != MessageType::Handshake {
        return Err(MlinkError::CodecError(format!(
            "expected Handshake, got {:?}",
            msg_type
        )));
    }
    codec::decode(&recv_frame.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::StreamResumeInfo;
    use crate::transport::Connection;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn role_negotiation_smaller_uuid_is_central() {
        assert_eq!(negotiate_role("aaa", "bbb"), Role::Central);
        assert_eq!(negotiate_role("bbb", "aaa"), Role::Peripheral);
    }

    #[test]
    fn role_negotiation_is_deterministic_pairwise() {
        let a = "uuid-a";
        let b = "uuid-z";
        let a_role = negotiate_role(a, b);
        let b_role = negotiate_role(b, a);
        assert_eq!(a_role, Role::Central);
        assert_eq!(b_role, Role::Peripheral);
    }

    #[test]
    fn role_negotiation_handles_equal_uuids() {
        // Same uuid is pathological but must be deterministic.
        let r = negotiate_role("same", "same");
        assert_eq!(r, Role::Peripheral);
    }

    #[tokio::test]
    async fn perform_handshake_times_out_on_silent_peer() {
        // A peer that never sends back a handshake must not freeze the caller;
        // the write succeeds, the read hangs, and the per-direction timeout
        // must convert that into a HandlerError within a bounded window.
        struct SilentConn;
        #[async_trait]
        impl Connection for SilentConn {
            async fn read(&mut self) -> Result<Vec<u8>> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn write(&mut self, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            async fn close(&mut self) -> Result<()> {
                Ok(())
            }
            fn peer_id(&self) -> &str {
                "silent"
            }
        }

        let mut conn = SilentConn;
        // Pause tokio's clock so the 10s constant doesn't actually wait; we
        // advance virtual time instead and still exercise the real timeout
        // logic in perform_handshake.
        tokio::time::pause();
        let handle = tokio::spawn(async move {
            perform_handshake(&mut conn, &sample_handshake("me")).await
        });
        // Step past the read ceiling.
        tokio::time::advance(HANDSHAKE_IO_TIMEOUT + Duration::from_millis(1)).await;
        let err = handle.await.expect("join").unwrap_err();
        assert!(
            matches!(err, MlinkError::HandlerError(ref msg) if msg.contains("timed out")),
            "expected HandlerError(... timed out ...), got {err:?}"
        );
    }

    struct MockConn {
        id: String,
        reads: Arc<Mutex<Vec<Vec<u8>>>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    #[async_trait]
    impl Connection for MockConn {
        async fn read(&mut self) -> Result<Vec<u8>> {
            let mut guard = self.reads.lock().await;
            if guard.is_empty() {
                return Err(MlinkError::PeerGone {
                    peer_id: self.id.clone(),
                });
            }
            Ok(guard.remove(0))
        }
        async fn write(&mut self, data: &[u8]) -> Result<()> {
            self.writes.lock().await.push(data.to_vec());
            Ok(())
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
        fn peer_id(&self) -> &str {
            &self.id
        }
    }

    fn make_mock(id: &str) -> MockConn {
        MockConn {
            id: id.into(),
            reads: Arc::new(Mutex::new(Vec::new())),
            writes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[tokio::test]
    async fn manager_add_and_count() {
        let mut mgr = ConnectionManager::new();
        mgr.add("a".into(), Box::new(make_mock("a"))).await;
        mgr.add("b".into(), Box::new(make_mock("b"))).await;
        assert_eq!(mgr.count(), 2);
        let mut ids = mgr.list_ids();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn manager_remove_returns_conn() {
        let mut mgr = ConnectionManager::new();
        mgr.add("a".into(), Box::new(make_mock("a"))).await;
        let removed = mgr.remove("a").await;
        assert!(removed.is_some());
        assert_eq!(mgr.count(), 0);
        assert!(mgr.remove("a").await.is_none());
    }

    #[tokio::test]
    async fn manager_get_returns_ref() {
        let mut mgr = ConnectionManager::new();
        mgr.add("a".into(), Box::new(make_mock("a"))).await;
        let got = mgr.get("a").await.expect("present");
        assert_eq!(got.peer_id(), "a");
        assert!(mgr.get("missing").await.is_none());
    }

    fn sample_handshake(uuid: &str) -> Handshake {
        Handshake {
            app_uuid: uuid.into(),
            version: 1,
            mtu: 512,
            compress: true,
            encrypt: true,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: None,
        }
    }

    #[tokio::test]
    async fn perform_handshake_round_trip() {
        // Pre-stage a handshake frame "from the peer" in the read queue.
        let peer_hs = sample_handshake("peer-uuid");
        let peer_payload = codec::encode(&peer_hs).expect("encode");
        let peer_frame = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(false, false, MessageType::Handshake),
            seq: 0,
            length: peer_payload.len() as u16,
            payload: peer_payload,
        };
        let peer_bytes = encode_frame(&peer_frame);

        let reads = Arc::new(Mutex::new(vec![peer_bytes]));
        let writes: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut conn = MockConn {
            id: "peer".into(),
            reads: reads.clone(),
            writes: writes.clone(),
        };

        let local_hs = sample_handshake("local-uuid");
        let got = perform_handshake(&mut conn, &local_hs).await.expect("hs");

        assert_eq!(got, peer_hs);

        // Verify we wrote a well-formed handshake frame.
        let written = writes.lock().await;
        assert_eq!(written.len(), 1);
        let decoded = decode_frame(&written[0]).expect("decode");
        let (_, _, mt) = crate::protocol::types::decode_flags(decoded.flags);
        assert_eq!(mt, MessageType::Handshake);
        let sent_hs: Handshake = codec::decode(&decoded.payload).expect("decode hs");
        assert_eq!(sent_hs, local_hs);
    }

    #[tokio::test]
    async fn perform_handshake_rejects_non_handshake_reply() {
        // Peer sends a Message frame instead of a Handshake.
        let bogus_frame = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(false, false, MessageType::Message),
            seq: 0,
            length: 0,
            payload: vec![],
        };
        let reads = Arc::new(Mutex::new(vec![encode_frame(&bogus_frame)]));
        let mut conn = MockConn {
            id: "peer".into(),
            reads,
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let err = perform_handshake(&mut conn, &sample_handshake("me"))
            .await
            .unwrap_err();
        assert!(matches!(err, MlinkError::CodecError(_)));
    }

    #[tokio::test]
    async fn perform_handshake_with_resume_streams() {
        let mut peer_hs = sample_handshake("peer");
        peer_hs.last_seq = 4242;
        peer_hs.resume_streams = vec![StreamResumeInfo {
            stream_id: 3,
            received_bitmap: vec![0xFF, 0x0F],
        }];

        let peer_payload = codec::encode(&peer_hs).expect("encode");
        let peer_frame = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(false, false, MessageType::Handshake),
            seq: 0,
            length: peer_payload.len() as u16,
            payload: peer_payload,
        };

        let reads = Arc::new(Mutex::new(vec![encode_frame(&peer_frame)]));
        let mut conn = MockConn {
            id: "p".into(),
            reads,
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let got = perform_handshake(&mut conn, &sample_handshake("me"))
            .await
            .expect("hs");
        assert_eq!(got, peer_hs);
    }
}
