use std::sync::Arc;
use std::time::Instant;

use super::{Node, NodeEvent, NodeState, PeerState};
use crate::core::connection::{negotiate_role, perform_handshake};
use crate::core::peer::Peer;
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
            "accept_incoming",
            wire_id,
        )
        .await
    }

    /// Shared post-handshake bookkeeping: room check → dedup → register peer
    /// + connection + PeerState, flip Node state, emit PeerConnected. On
    /// failure the connection is closed and the caller does not need to
    /// clean up.
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
        site: &'static str,
        mismatch_peer_id: String,
    ) -> Result<String> {
        // Room membership check: if we advertise any rooms, the peer's claimed
        // room_hash must be in our set. Empty set = accept anyone.
        //
        // Snapshot and immediately drop the StdMutex guard so no `!Send`
        // MutexGuard outlives a later `.await` — needed for `tokio::spawn`ing
        // the daemon's connect/accept loop.
        let (rooms_empty, matched, snapshot) = {
            let rooms = self.room_hashes.lock().expect("room_hashes poisoned");
            let empty = rooms.is_empty();
            let matched = match peer_hs.room_hash {
                Some(peer) => rooms.contains(&peer),
                None => false,
            };
            (empty, matched, rooms.clone())
        };
        if !rooms_empty {
            if matched {
                eprintln!(
                    "[mlink:conn] node.{site} room match OK peer={} room={:?}",
                    mismatch_peer_id, peer_hs.room_hash
                );
            } else {
                eprintln!(
                    "[mlink:conn] node.{site} ROOM MISMATCH peer={} local_rooms={:?} peer_claims={:?}",
                    mismatch_peer_id, snapshot, peer_hs.room_hash
                );
                let _ = conn.close().await;
                return Err(MlinkError::RoomMismatch {
                    peer_id: mismatch_peer_id,
                });
            }
        }

        let peer_id = peer_hs.app_uuid.clone();

        // Idempotency: if we already hold a live connection for this app_uuid
        // (peer dialled us while we were dialling out, or the scanner
        // surfaced the same peer twice), drop the new connection and return
        // the existing peer_id. Installing the duplicate would kick the
        // working one out of ConnectionManager and tear down the live link.
        {
            let guard = self.connections.lock().await;
            if guard.contains(&peer_id) {
                drop(guard);
                eprintln!(
                    "[mlink:conn] node.{site} DEDUP peer={} already connected — closing duplicate",
                    peer_id
                );
                let _ = conn.close().await;
                return Ok(peer_id);
            }
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
            let mut guard = self.connections.lock().await;
            guard.add(peer_id.clone(), Arc::from(conn));
        }
        {
            let mut states = self.peer_states.write().await;
            let entry = states.entry(peer_id.clone()).or_insert_with(PeerState::new);
            entry.state = NodeState::Connected;
            entry.last_heartbeat = Some(Instant::now());
        }

        *self.state.lock().expect("state poisoned") = NodeState::Connected;
        let _ = self.events_tx.send(NodeEvent::PeerConnected {
            peer_id: peer_id.clone(),
        });

        Ok(peer_id)
    }
}
