use std::sync::Arc;
use std::time::Instant;

use super::{Node, NodeEvent, NodeState, PeerState};
use crate::core::connection::{negotiate_role, perform_handshake};
use crate::core::link::{Link, TransportKind};
use crate::core::peer::Peer;
use crate::core::session::manager::AttachOutcome;
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::types::{Handshake, PROTOCOL_VERSION};
use crate::transport::{Connection, DiscoveredPeer, Transport};

impl Node {
    pub async fn connect_peer(
        &self,
        transport: &mut dyn Transport,
        discovered: &DiscoveredPeer,
    ) -> Result<String> {
        eprintln!(
            "[mlink:conn] node.connect_peer ENTER peer_id={} name={:?}",
            discovered.id, discovered.name
        );
        self.set_peer_state(&discovered.id, NodeState::Connecting).await;

        let _role = negotiate_role(&self.app_uuid, &discovered.id);
        eprintln!(
            "[mlink:conn] node.connect_peer role (by wire-id, pre-handshake)={:?} my_app_uuid={} peer_wire_id={}",
            _role, self.app_uuid, discovered.id
        );

        let conn = transport.connect(discovered).await.map_err(|e| {
            eprintln!("[mlink:conn] node.connect_peer transport.connect FAILED peer={}: {e}", discovered.id);
            e
        })?;
        eprintln!("[mlink:conn] node.connect_peer transport.connect OK peer={}", discovered.id);

        let wire_room = self.wire_room_hash();
        let local_hs = Handshake {
            app_uuid: self.app_uuid.clone(),
            version: PROTOCOL_VERSION,
            mtu: transport.mtu() as u16,
            compress: true,
            // Advertise the *true* encryption state: we hold no session key at
            // handshake time, so encryption is not yet active. Matching
            // send_raw's actual behavior keeps the peer from trying to
            // decrypt plaintext.
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: wire_room,
            session_id: None,
            session_last_seq: 0,
        };

        eprintln!(
            "[mlink:conn] node.connect_peer -> perform_handshake peer={} wire_room_hash={:?}",
            discovered.id, wire_room
        );
        let peer_hs = match perform_handshake(&*conn, &local_hs).await {
            Ok(hs) => {
                eprintln!(
                    "[mlink:conn] node.connect_peer handshake OK peer_wire={} peer_app={} peer_room={:?}",
                    discovered.id, hs.app_uuid, hs.room_hash
                );
                hs
            }
            Err(e) => {
                eprintln!(
                    "[mlink:conn] node.connect_peer handshake FAILED peer={}: {e}",
                    discovered.id
                );
                let _ = conn.close().await;
                return Err(e);
            }
        };

        self.register_after_handshake(
            peer_hs,
            conn,
            discovered.name.clone(),
            transport.id(),
            transport.capabilities(),
            "connect_peer",
            discovered.id.clone(),
        )
        .await
    }

    /// Accept an incoming connection (peripheral side). Runs a handshake over
    /// the already-open connection, enforces the room-hash check, and — on
    /// success — registers the connection under the peer's app_uuid. On
    /// failure the connection is closed so callers don't need to clean up.
    pub async fn accept_incoming(
        &self,
        conn: Box<dyn Connection>,
        transport_id: &str,
        fallback_name: String,
    ) -> Result<String> {
        let wire_id = conn.peer_id().to_string();
        eprintln!(
            "[mlink:conn] node.accept_incoming ENTER wire_id={wire_id} transport={transport_id}"
        );
        let wire_room = self.wire_room_hash();
        let local_hs = Handshake {
            app_uuid: self.app_uuid.clone(),
            version: PROTOCOL_VERSION,
            mtu: 512,
            compress: true,
            encrypt: false,
            last_seq: 0,
            resume_streams: vec![],
            room_hash: wire_room,
            session_id: None,
            session_last_seq: 0,
        };

        eprintln!(
            "[mlink:conn] node.accept_incoming -> perform_handshake wire={wire_id} wire_room_hash={:?}",
            wire_room
        );
        let peer_hs = match perform_handshake(&*conn, &local_hs).await {
            Ok(hs) => {
                eprintln!(
                    "[mlink:conn] node.accept_incoming handshake OK wire={wire_id} peer_app={} peer_room={:?}",
                    hs.app_uuid, hs.room_hash
                );
                hs
            }
            Err(e) => {
                eprintln!(
                    "[mlink:conn] node.accept_incoming handshake FAILED wire={wire_id}: {e}"
                );
                let _ = conn.close().await;
                return Err(e);
            }
        };

        self.register_after_handshake(
            peer_hs,
            conn,
            fallback_name,
            transport_id,
            default_caps_for_wire(transport_id),
            "accept_incoming",
            wire_id,
        )
        .await
    }

    /// Shared post-handshake bookkeeping: room check → register (or attach as
    /// secondary link) → register peer + PeerState → flip Node state → emit
    /// PeerConnected / LinkAdded. On failure the connection is closed and the
    /// caller does not need to clean up.
    ///
    /// `site` is only used to disambiguate log lines between the inbound and
    /// outbound paths. `mismatch_peer_id` is reported in `RoomMismatch`
    /// errors — it is the wire-level id the caller started with, which may
    /// differ from the app_uuid carried in `peer_hs` (the peer has not been
    /// trusted until the room check passes).
    async fn register_after_handshake(
        &self,
        peer_hs: Handshake,
        conn: Box<dyn Connection>,
        name: String,
        transport_id: &str,
        transport_caps: crate::transport::TransportCapabilities,
        site: &'static str,
        mismatch_peer_id: String,
    ) -> Result<String> {
        if let Err(e) = self.check_room(&peer_hs, site, &mismatch_peer_id).await {
            let _ = conn.close().await;
            return Err(e);
        }

        let peer_id = peer_hs.app_uuid.clone();
        let kind = TransportKind::from_wire(transport_id).unwrap_or(TransportKind::Mock);
        let conn_arc: Arc<dyn Connection> = Arc::from(conn);
        let is_first_link = !self.sessions.contains(&peer_id).await;

        // Attach to SessionManager. Duplicate kind → dedup (legacy behavior).
        // Capacity overflow → reject. Different kind → attach as secondary.
        let link_id = make_link_id(kind, &peer_id);
        let link = Link::new(
            link_id.clone(),
            Arc::clone(&conn_arc),
            kind,
            transport_caps,
        );
        match self
            .sessions
            .attach_link(&peer_id, link, peer_hs.session_id)
            .await
        {
            AttachOutcome::CreatedNew { .. } | AttachOutcome::AttachedExisting => {}
            AttachOutcome::RejectedDuplicateKind
            | AttachOutcome::RejectedCapacity
            | AttachOutcome::RejectedSessionMismatch => {
                eprintln!(
                    "[mlink:conn] node.{site} attach_link rejected peer={peer_id} kind={:?} — closing duplicate",
                    kind
                );
                let _ = conn_arc.close().await;
                return Ok(peer_id);
            }
        }

        // Always upsert the legacy ConnectionManager entry: the single-link
        // send path (used whenever link count < 2) must be able to reach the
        // peer over the newest link, not just the first one attached.
        {
            let mut guard = self.connections.lock().await;
            guard.add(peer_id.clone(), Arc::clone(&conn_arc));
        }

        let peer = Peer {
            id: peer_id.clone(),
            name,
            app_uuid: peer_hs.app_uuid.clone(),
            connected_at: Instant::now(),
            transport_id: transport_id.to_string(),
        };
        self.peer_manager.add(peer).await;
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }

        *self.state.lock().expect("state poisoned") = NodeState::Connected;

        let _ = self.events_tx().send(NodeEvent::LinkAdded {
            peer_id: peer_id.clone(),
            link_id,
            transport: kind,
        });
        if is_first_link {
            let _ = self.events_tx().send(NodeEvent::PeerConnected {
                peer_id: peer_id.clone(),
            });
        }
        self.spawn_session_bridge_for(&peer_id).await;

        Ok(peer_id)
    }

    /// Public entry point to attach a second physical link after the session
    /// already exists (e.g. `dual_probe` completed a BLE pair then dialed a
    /// TCP pair). Fails if no session is present for this peer.
    pub async fn attach_secondary_link(
        &self,
        peer_id: &str,
        conn: Box<dyn Connection>,
        transport_kind: TransportKind,
    ) -> Result<String> {
        if !self.sessions.contains(peer_id).await {
            return Err(MlinkError::HandlerError(format!(
                "no session for peer {peer_id}; cannot attach secondary"
            )));
        }
        let link_id = make_link_id(transport_kind, peer_id);
        let conn_arc: Arc<dyn Connection> = Arc::from(conn);
        let link = Link::new(
            link_id.clone(),
            Arc::clone(&conn_arc),
            transport_kind,
            default_caps_for_kind(transport_kind),
        );
        match self.sessions.attach_link(peer_id, link, None).await {
            AttachOutcome::AttachedExisting => {
                let _ = self.events_tx().send(NodeEvent::LinkAdded {
                    peer_id: peer_id.to_string(),
                    link_id: link_id.clone(),
                    transport: transport_kind,
                });
                Ok(link_id)
            }
            AttachOutcome::RejectedDuplicateKind => Err(MlinkError::HandlerError(format!(
                "peer {peer_id} already has a {:?} link",
                transport_kind
            ))),
            AttachOutcome::RejectedCapacity => Err(MlinkError::HandlerError(format!(
                "peer {peer_id} reached MAX_LINKS_PER_PEER"
            ))),
            AttachOutcome::CreatedNew { .. } | AttachOutcome::RejectedSessionMismatch => {
                Err(MlinkError::HandlerError(format!(
                    "unexpected attach outcome for peer {peer_id}"
                )))
            }
        }
    }

    async fn check_room(
        &self,
        peer_hs: &Handshake,
        site: &'static str,
        mismatch_peer_id: &str,
    ) -> Result<()> {
        // Snapshot + immediately drop the StdMutex guard so no `!Send`
        // MutexGuard outlives a later `.await`.
        let (rooms_empty, matched, snapshot) = {
            let rooms = self.room_hashes.lock().expect("room_hashes poisoned");
            let empty = rooms.is_empty();
            let matched = match peer_hs.room_hash {
                Some(peer) => rooms.contains(&peer),
                None => false,
            };
            (empty, matched, rooms.clone())
        };
        if rooms_empty {
            return Ok(());
        }
        if matched {
            eprintln!(
                "[mlink:conn] node.{site} room match OK peer={mismatch_peer_id} room={:?}",
                peer_hs.room_hash
            );
            Ok(())
        } else {
            eprintln!(
                "[mlink:conn] node.{site} ROOM MISMATCH peer={mismatch_peer_id} local_rooms={:?} peer_claims={:?}",
                snapshot, peer_hs.room_hash
            );
            Err(MlinkError::RoomMismatch {
                peer_id: mismatch_peer_id.to_string(),
            })
        }
    }
}

/// Link id = "{kind}:{peer_short}:{nonce_u32}". peer_short is the first 8
/// chars of the peer id for readability; nonce is a random u32 so concurrent
/// attaches within the same millisecond don't collide.
pub(super) fn make_link_id(kind: TransportKind, peer_id: &str) -> String {
    use ring::rand::{SecureRandom, SystemRandom};
    let short = peer_id
        .chars()
        .take(8)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string();
    let mut n = [0u8; 4];
    SystemRandom::new()
        .fill(&mut n)
        .expect("SystemRandom fill 4 bytes");
    let nonce = u32::from_le_bytes(n);
    format!("{}:{}:{:08x}", kind.as_wire(), short, nonce)
}

pub(super) fn default_caps_for_kind(kind: TransportKind) -> crate::transport::TransportCapabilities {
    match kind {
        TransportKind::Ble => crate::transport::TransportCapabilities {
            max_peers: 4,
            throughput_bps: 200_000,
            latency_ms: 20,
            reliable: true,
            bidirectional: true,
        },
        TransportKind::Tcp => crate::transport::TransportCapabilities {
            max_peers: 64,
            throughput_bps: 10_000_000,
            latency_ms: 5,
            reliable: true,
            bidirectional: true,
        },
        TransportKind::Ipc => crate::transport::TransportCapabilities {
            max_peers: 16,
            throughput_bps: 100_000_000,
            latency_ms: 1,
            reliable: true,
            bidirectional: true,
        },
        TransportKind::Mock => crate::transport::TransportCapabilities {
            max_peers: 1,
            throughput_bps: 1_000_000,
            latency_ms: 1,
            reliable: true,
            bidirectional: true,
        },
    }
}

fn default_caps_for_wire(wire: &str) -> crate::transport::TransportCapabilities {
    default_caps_for_kind(TransportKind::from_wire(wire).unwrap_or(TransportKind::Mock))
}
