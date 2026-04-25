use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::core::link::{Link, TransportKind};
use crate::core::session::manager::{AttachOutcome, SessionManager};
use crate::core::session::types::{LinkStatus, SessionEvent, SwitchCause};
use crate::protocol::codec;
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::frame::{decode_frame, encode_frame};
use crate::protocol::types::{
    encode_flags, Frame, Handshake, MessageType, MAGIC, PROTOCOL_VERSION,
};
use crate::transport::{Connection, TransportCapabilities};

/// A connection shared between the `ConnectionManager` map and whatever task
/// is currently driving read or write on it. `Connection` is `&self`-only, so
/// a single `Arc<dyn Connection>` can safely be cloned into a peer-reader
/// task and a sender at the same time — the implementation is responsible
/// for its own internal serialisation. This avoids the old head-of-line
/// block where a parked `read().await` on one shared mutex would also stall
/// `send_raw()` on the same peer.
pub type SharedConnection = Arc<dyn Connection>;

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

/// Compat shell around `SessionManager` (W2-D). Sync API preserves the
/// pre-dual-transport `Arc<Mutex<ConnectionManager>>` contract for legacy
/// call sites / 450+ tests; async API forwards into `SessionManager`. Both
/// stores co-exist during the transition — sync callers mutate only the
/// HashMap, async callers drive the SessionManager.
pub struct ConnectionManager {
    conns: HashMap<String, SharedConnection>,
    sessions: Arc<SessionManager>,
}

impl ConnectionManager {
    pub fn new() -> Self {
        Self {
            conns: HashMap::new(),
            sessions: Arc::new(SessionManager::new()),
        }
    }

    /// Reuse an existing `SessionManager` so Node can share one instance
    /// between the compat shell and its own async paths.
    pub fn with_sessions(sessions: Arc<SessionManager>) -> Self {
        Self {
            conns: HashMap::new(),
            sessions,
        }
    }

    pub fn add(&mut self, id: String, conn: SharedConnection) {
        self.conns.insert(id, conn);
    }

    pub fn remove(&mut self, id: &str) -> Option<SharedConnection> {
        self.conns.remove(id)
    }

    /// Clone the shared handle for `id` if any. Callers must release the
    /// outer Mutex immediately after the clone — the actual read/write
    /// runs against the returned Arc with the map lock dropped.
    pub fn shared(&self, id: &str) -> Option<SharedConnection> {
        self.conns.get(id).cloned()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.conns.contains_key(id)
    }

    pub fn list_ids(&self) -> Vec<String> {
        self.conns.keys().cloned().collect()
    }

    pub fn count(&self) -> usize {
        self.conns.len()
    }

    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.sessions
    }

    /// Async counterpart of `add`: wrap `conn` as a single-link Session so
    /// Session-layer readers/senders can see the peer.
    pub async fn attach_link_async(
        &self,
        id: &str,
        conn: SharedConnection,
        kind: TransportKind,
        caps: TransportCapabilities,
    ) -> AttachOutcome {
        let link_id = format!("{}:{}:0", kind.as_wire(), short_peer(id));
        let link = Link::new(link_id, conn, kind, caps);
        self.sessions.attach_link(id, link, None).await
    }

    /// Attach a second (or Nth) link to an already-connected peer.
    pub async fn attach_secondary_link(
        &self,
        peer_id: &str,
        link: Link,
        session_id_hint: Option<[u8; 16]>,
    ) -> AttachOutcome {
        self.sessions
            .attach_link(peer_id, link, session_id_hint)
            .await
    }

    pub async fn peer_link_status(&self, peer_id: &str) -> Vec<LinkStatus> {
        self.sessions.peer_link_status(peer_id).await
    }

    /// Force `link_id` active on `peer_id`. Emits `Switched(ManualUserRequest)`
    /// when the active link changes; `NoHealthyLink` on unknown peer/link.
    pub async fn switch_active_link(&self, peer_id: &str, link_id: &str) -> Result<()> {
        let session = self.sessions.get(peer_id).await.ok_or_else(|| {
            MlinkError::NoHealthyLink {
                peer_id: peer_id.to_string(),
            }
        })?;
        let present = session
            .links
            .read()
            .await
            .iter()
            .any(|l| l.id() == link_id);
        if !present {
            return Err(MlinkError::NoHealthyLink {
                peer_id: peer_id.to_string(),
            });
        }
        let prev = {
            let mut active = session.active.write().await;
            let prev = active.clone();
            *active = Some(link_id.to_string());
            prev
        };
        if prev.as_deref() != Some(link_id) {
            let _ = session.events.send(SessionEvent::Switched {
                peer_id: peer_id.to_string(),
                from: prev.unwrap_or_default(),
                to: link_id.to_string(),
                cause: SwitchCause::ManualUserRequest,
            });
        }
        Ok(())
    }

    /// Drop a peer from the SessionManager side (closes every link). Sync
    /// HashMap is untouched — call `remove` to evict there too.
    pub async fn drop_peer_async(&self, peer_id: &str) {
        let _ = self.sessions.drop_peer(peer_id).await;
    }
}

fn short_peer(id: &str) -> &str {
    let cut = id.char_indices().nth(4).map(|(i, _)| i).unwrap_or(id.len());
    &id[..cut]
}

impl Default for ConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn perform_handshake(
    conn: &dyn Connection,
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
            async fn read(&self) -> Result<Vec<u8>> {
                std::future::pending::<()>().await;
                unreachable!()
            }
            async fn write(&self, _data: &[u8]) -> Result<()> {
                Ok(())
            }
            async fn close(&self) -> Result<()> {
                Ok(())
            }
            fn peer_id(&self) -> &str {
                "silent"
            }
        }

        let conn = SilentConn;
        // Pause tokio's clock so the 10s constant doesn't actually wait; we
        // advance virtual time instead and still exercise the real timeout
        // logic in perform_handshake.
        tokio::time::pause();
        let handle = tokio::spawn(async move {
            perform_handshake(&conn, &sample_handshake("me")).await
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
        async fn read(&self) -> Result<Vec<u8>> {
            let mut guard = self.reads.lock().await;
            if guard.is_empty() {
                return Err(MlinkError::PeerGone {
                    peer_id: self.id.clone(),
                });
            }
            Ok(guard.remove(0))
        }
        async fn write(&self, data: &[u8]) -> Result<()> {
            self.writes.lock().await.push(data.to_vec());
            Ok(())
        }
        async fn close(&self) -> Result<()> {
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
        mgr.add("a".into(), Arc::new(make_mock("a")));
        mgr.add("b".into(), Arc::new(make_mock("b")));
        assert_eq!(mgr.count(), 2);
        let mut ids = mgr.list_ids();
        ids.sort();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn manager_remove_returns_conn() {
        let mut mgr = ConnectionManager::new();
        mgr.add("a".into(), Arc::new(make_mock("a")));
        let removed = mgr.remove("a");
        assert!(removed.is_some());
        assert_eq!(mgr.count(), 0);
        assert!(mgr.remove("a").is_none());
    }

    #[tokio::test]
    async fn manager_shared_returns_clone() {
        let mut mgr = ConnectionManager::new();
        mgr.add("a".into(), Arc::new(make_mock("a")));
        let got = mgr.shared("a").expect("present");
        assert_eq!(got.peer_id(), "a");
        assert!(mgr.shared("missing").is_none());
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
            session_id: None,
            session_last_seq: 0,
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
        let conn = MockConn {
            id: "peer".into(),
            reads: reads.clone(),
            writes: writes.clone(),
        };

        let local_hs = sample_handshake("local-uuid");
        let got = perform_handshake(&conn, &local_hs).await.expect("hs");

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
        let conn = MockConn {
            id: "peer".into(),
            reads,
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let err = perform_handshake(&conn, &sample_handshake("me"))
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
        let conn = MockConn {
            id: "p".into(),
            reads,
            writes: Arc::new(Mutex::new(Vec::new())),
        };
        let got = perform_handshake(&conn, &sample_handshake("me"))
            .await
            .expect("hs");
        assert_eq!(got, peer_hs);
    }

    // ---- W2-D compat-shell: async bridge into SessionManager ----

    fn default_caps() -> crate::transport::TransportCapabilities {
        crate::transport::TransportCapabilities {
            max_peers: 8,
            throughput_bps: 1_000_000,
            latency_ms: 10,
            reliable: true,
            bidirectional: true,
        }
    }

    #[tokio::test]
    async fn attach_link_async_creates_session_and_exposes_via_session_manager() {
        let mgr = ConnectionManager::new();
        let conn: SharedConnection = Arc::new(make_mock("peer-a"));
        let outcome = mgr
            .attach_link_async("peer-a", conn, crate::core::link::TransportKind::Tcp, default_caps())
            .await;
        assert!(matches!(
            outcome,
            crate::core::session::manager::AttachOutcome::CreatedNew { .. }
        ));
        assert!(mgr.session_manager().contains("peer-a").await);
        let status = mgr.peer_link_status("peer-a").await;
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].kind, crate::core::link::TransportKind::Tcp);
    }

    #[tokio::test]
    async fn attach_secondary_link_merges_when_session_id_matches() {
        let mgr = ConnectionManager::new();
        let primary: SharedConnection = Arc::new(make_mock("peer-b"));
        let outcome = mgr
            .attach_link_async("peer-b", primary, crate::core::link::TransportKind::Ble, default_caps())
            .await;
        let sid = match outcome {
            crate::core::session::manager::AttachOutcome::CreatedNew { session_id } => session_id,
            _ => panic!("expected CreatedNew"),
        };
        let second_conn: SharedConnection = Arc::new(make_mock("peer-b"));
        let second = crate::core::link::Link::new(
            "tcp:peer:1".into(),
            second_conn,
            crate::core::link::TransportKind::Tcp,
            default_caps(),
        );
        let outcome = mgr.attach_secondary_link("peer-b", second, Some(sid)).await;
        assert_eq!(
            outcome,
            crate::core::session::manager::AttachOutcome::AttachedExisting
        );
        let status = mgr.peer_link_status("peer-b").await;
        assert_eq!(status.len(), 2);
    }

    #[tokio::test]
    async fn switch_active_link_emits_manual_switched_event() {
        let mgr = ConnectionManager::new();
        let primary: SharedConnection = Arc::new(make_mock("peer-c"));
        mgr.attach_link_async(
            "peer-c",
            primary,
            crate::core::link::TransportKind::Ble,
            default_caps(),
        )
        .await;
        // Attach a second link via the secondary path so we have a target to flip to.
        let session = mgr.session_manager().get("peer-c").await.expect("session");
        let sid = session.session_id;
        let second_conn: SharedConnection = Arc::new(make_mock("peer-c"));
        let second_link_id = "tcp:peer:1".to_string();
        let second = crate::core::link::Link::new(
            second_link_id.clone(),
            second_conn,
            crate::core::link::TransportKind::Tcp,
            default_caps(),
        );
        mgr.attach_secondary_link("peer-c", second, Some(sid)).await;

        let mut rx = session.events.subscribe();
        mgr.switch_active_link("peer-c", &second_link_id)
            .await
            .expect("switch ok");
        // Give the broadcast a moment
        let ev = tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("event in time")
            .expect("event recv");
        match ev {
            crate::core::session::types::SessionEvent::Switched { to, cause, .. } => {
                assert_eq!(to, second_link_id);
                assert_eq!(
                    cause,
                    crate::core::session::types::SwitchCause::ManualUserRequest
                );
            }
            other => panic!("expected Switched, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn switch_active_link_rejects_unknown_peer_or_link() {
        let mgr = ConnectionManager::new();
        let err = mgr
            .switch_active_link("nope", "tcp:xxx:0")
            .await
            .unwrap_err();
        assert!(matches!(err, MlinkError::NoHealthyLink { .. }));

        let conn: SharedConnection = Arc::new(make_mock("peer-d"));
        mgr.attach_link_async(
            "peer-d",
            conn,
            crate::core::link::TransportKind::Tcp,
            default_caps(),
        )
        .await;
        let err = mgr
            .switch_active_link("peer-d", "wrong-link")
            .await
            .unwrap_err();
        assert!(matches!(err, MlinkError::NoHealthyLink { .. }));
    }

    #[tokio::test]
    async fn sync_and_async_paths_are_independent() {
        // The sync HashMap and the SessionManager are separate stores; each
        // path should only mutate its own side. This guards callers that
        // only go through the sync shell from accidental SessionManager
        // side-effects, and vice-versa.
        let mut mgr = ConnectionManager::new();
        mgr.add("sync-peer".into(), Arc::new(make_mock("sync-peer")));
        assert!(mgr.contains("sync-peer"));
        assert_eq!(mgr.count(), 1);
        assert!(!mgr.session_manager().contains("sync-peer").await);

        let async_conn: SharedConnection = Arc::new(make_mock("async-peer"));
        mgr.attach_link_async(
            "async-peer",
            async_conn,
            crate::core::link::TransportKind::Tcp,
            default_caps(),
        )
        .await;
        assert!(!mgr.contains("async-peer"));
        assert!(mgr.session_manager().contains("async-peer").await);
    }
}
