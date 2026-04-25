use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, RwLock};

use super::{Node, NodeEvent, NodeState, PeerState, HEARTBEAT_TIMEOUT};
use crate::core::connection::ConnectionManager;
use crate::core::session::SessionManager;
use crate::protocol::compress::{compress, decompress, should_compress};
use crate::protocol::errors::{MlinkError, Result};
use crate::protocol::frame::{decode_frame, encode_frame};
use crate::protocol::types::{decode_flags, encode_flags, Frame, MessageType, MAGIC, PROTOCOL_VERSION};

impl Node {
    pub async fn send_raw(
        &self,
        peer_id: &str,
        msg_type: MessageType,
        payload: &[u8],
    ) -> Result<()> {
        // Dual-link peers must go through Session so ack/failover/dedup all
        // line up across links. Single-link peers stay on the legacy direct
        // write path — it preserves the exact byte layout on the wire that
        // pre-dual-transport tests and clients assume (no u32 seq prefix).
        if self.session_is_multilink(peer_id).await {
            return self.send_via_session(peer_id, msg_type, payload).await;
        }
        self.send_legacy(peer_id, msg_type, payload).await
    }

    async fn session_is_multilink(&self, peer_id: &str) -> bool {
        match self.sessions.get(peer_id).await {
            Some(s) => s.links.read().await.len() >= 2,
            None => false,
        }
    }

    async fn send_via_session(
        &self,
        peer_id: &str,
        msg_type: MessageType,
        payload: &[u8],
    ) -> Result<()> {
        let session = self
            .sessions
            .get(peer_id)
            .await
            .ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?;
        let (encrypt_key, compress_on) = {
            let states = self.peer_states.read().await;
            let st = states.get(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?;
            let key = if self.config.encrypt {
                st.aes_key.clone()
            } else {
                None
            };
            (key, true)
        };
        session
            .send(msg_type, payload.to_vec(), encrypt_key.as_deref(), compress_on)
            .await?;
        Ok(())
    }

    async fn send_legacy(
        &self,
        peer_id: &str,
        msg_type: MessageType,
        payload: &[u8],
    ) -> Result<()> {
        let (seq, compressed_flag, encrypted_flag, body) = {
            let mut states = self.peer_states.write().await;
            let st = states.get_mut(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?;

            let seq = st.next_seq();

            let mut body = payload.to_vec();
            let compressed_flag = if should_compress(&body) {
                body = compress(&body)?;
                true
            } else {
                false
            };

            let encrypted_flag = if self.config.encrypt {
                if let Some(key) = &st.aes_key {
                    body = crate::core::security::encrypt(&body, key, seq)?;
                    true
                } else {
                    false
                }
            } else {
                false
            };
            (seq, compressed_flag, encrypted_flag, body)
        };

        let length = u16::try_from(body.len()).map_err(|_| MlinkError::PayloadTooLarge {
            size: body.len(),
            max: u16::MAX as usize,
        })?;
        let frame = Frame {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            flags: encode_flags(compressed_flag, encrypted_flag, msg_type),
            seq,
            length,
            payload: body,
        };

        let bytes = encode_frame(&frame);
        // Clone the Arc<dyn Connection> under a brief lock, then release the
        // map lock before the actual write. Otherwise a parked reader holding
        // the map lock during `read().await` would block every `send_raw` in
        // the process — that was the exact BLE chat deadlock.
        let conn = {
            let guard = self.connections.lock().await;
            guard.shared(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?
        };
        conn.write(&bytes).await
    }

    /// Start a background task that drains messages from `peer_id` and
    /// publishes each `MessageType::Message` frame as a
    /// `NodeEvent::MessageReceived`. On the first read error (e.g. peer EOF)
    /// the task evicts the dead connection from `ConnectionManager`, drops
    /// the peer from `PeerManager`, flips `PeerState` to `Disconnected`, and
    /// emits `NodeEvent::PeerDisconnected` before returning. It does *not*
    /// attempt to reconnect — discovery owns that.
    ///
    /// When the peer has a multi-link session attached, the task routes reads
    /// through `Session::recv` instead so the u32 session_seq / dedup / ack
    /// machinery runs. Both modes converge on the same
    /// `NodeEvent::MessageReceived` payload shape.
    pub fn spawn_peer_reader(&self, peer_id: String) -> tokio::task::JoinHandle<()> {
        let connections = Arc::clone(&self.connections);
        let sessions = Arc::clone(&self.sessions);
        let peer_states = Arc::clone(&self.peer_states);
        let peer_manager = Arc::clone(&self.peer_manager);
        let events_tx = self.events_tx().clone();
        let encrypt = self.config.encrypt;
        tokio::spawn(async move {
            loop {
                let use_session = match sessions.get(&peer_id).await {
                    Some(s) => s.links.read().await.len() >= 2,
                    None => false,
                };
                let res = if use_session {
                    recv_once_session(&sessions, &peer_id).await
                } else {
                    recv_once(&connections, &peer_states, encrypt, &peer_id).await
                };
                match res {
                    Ok((msg_type, payload)) => {
                        if msg_type == MessageType::Message {
                            let _ = events_tx.send(NodeEvent::MessageReceived {
                                peer_id: peer_id.clone(),
                                payload,
                            });
                        }
                        // Heartbeats and other control frames are consumed
                        // silently — recv_once already refreshed the
                        // last-heartbeat timestamp.
                    }
                    Err(_) => {
                        // Mirror disconnect_peer: evict conn, drop peer,
                        // mark state Disconnected, fire the event. Done
                        // inline because this task only holds Arc handles,
                        // not &Node.
                        let removed = {
                            let mut guard = connections.lock().await;
                            guard.remove(&peer_id)
                        };
                        if let Some(conn) = removed {
                            let _ = conn.close().await;
                        }
                        let _ = sessions.drop_peer(&peer_id).await;
                        peer_manager.remove(&peer_id).await;
                        {
                            let mut states = peer_states.write().await;
                            if let Some(s) = states.get_mut(&peer_id) {
                                s.state = NodeState::Disconnected;
                            }
                        }
                        let _ = events_tx.send(NodeEvent::PeerDisconnected {
                            peer_id: peer_id.clone(),
                        });
                        return;
                    }
                }
            }
        })
    }

    pub async fn recv_raw(&self, peer_id: &str) -> Result<(Frame, Vec<u8>)> {
        let conn = {
            let guard = self.connections.lock().await;
            guard.shared(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?
        };
        let bytes = conn.read().await?;

        let frame = decode_frame(&bytes)?;
        let (compressed, encrypted, _msg_type) = decode_flags(frame.flags);

        let mut payload = frame.payload.clone();

        if encrypted {
            let states = self.peer_states.read().await;
            let st = states.get(peer_id).ok_or_else(|| MlinkError::PeerGone {
                peer_id: peer_id.to_string(),
            })?;
            if let Some(key) = &st.aes_key {
                payload = crate::core::security::decrypt(&payload, key, frame.seq)?;
            }
        }

        if compressed {
            payload = decompress(&payload)?;
        }

        if _msg_type == MessageType::Heartbeat {
            let mut states = self.peer_states.write().await;
            if let Some(s) = states.get_mut(peer_id) {
                s.last_heartbeat = Some(Instant::now());
            }
        }

        Ok((frame, payload))
    }

    pub async fn send_heartbeat(&self, peer_id: &str) -> Result<()> {
        self.send_raw(peer_id, MessageType::Heartbeat, &[]).await
    }

    pub async fn check_heartbeat(&self, peer_id: &str) -> Result<bool> {
        let states = self.peer_states.read().await;
        let st = states.get(peer_id).ok_or_else(|| MlinkError::PeerGone {
            peer_id: peer_id.to_string(),
        })?;
        match st.last_heartbeat {
            Some(t) => Ok(t.elapsed() < HEARTBEAT_TIMEOUT),
            None => Ok(true),
        }
    }
}

/// Read-decode-decrypt-decompress a single frame for the given peer using only
/// the shared state handles (no `&Node` needed). Extracted so
/// `spawn_peer_reader` can run in a background task without borrowing the
/// parent Node. Returns the message type plus the plaintext payload.
async fn recv_once(
    connections: &Arc<Mutex<ConnectionManager>>,
    peer_states: &Arc<RwLock<HashMap<String, PeerState>>>,
    encrypt: bool,
    peer_id: &str,
) -> Result<(MessageType, Vec<u8>)> {
    // Clone the Arc<dyn Connection> under the map lock, then release it
    // before awaiting read. Holding the map lock across read().await is what
    // caused the BLE chat deadlock: a parked reader blocked every concurrent
    // send_raw on the same Node.
    let conn = {
        let guard = connections.lock().await;
        guard.shared(peer_id).ok_or_else(|| MlinkError::PeerGone {
            peer_id: peer_id.to_string(),
        })?
    };
    let bytes = conn.read().await?;

    let frame = decode_frame(&bytes)?;
    let (compressed, encrypted, msg_type) = decode_flags(frame.flags);

    let mut payload = frame.payload.clone();

    if encrypted && encrypt {
        let states = peer_states.read().await;
        let st = states.get(peer_id).ok_or_else(|| MlinkError::PeerGone {
            peer_id: peer_id.to_string(),
        })?;
        if let Some(key) = &st.aes_key {
            payload = crate::core::security::decrypt(&payload, key, frame.seq)?;
        }
    }

    if compressed {
        payload = decompress(&payload)?;
    }

    if msg_type == MessageType::Heartbeat {
        let mut states = peer_states.write().await;
        if let Some(s) = states.get_mut(peer_id) {
            s.last_heartbeat = Some(Instant::now());
        }
    }

    Ok((msg_type, payload))
}

/// Session-backed recv: pulls the next application Message off whichever link
/// has data, honoring dedup + ack coalescing. Always returns `MessageType::Message`
/// on success — control frames (ack, heartbeat, etc.) are consumed inside
/// `Session::recv` and not surfaced.
async fn recv_once_session(
    sessions: &Arc<SessionManager>,
    peer_id: &str,
) -> Result<(MessageType, Vec<u8>)> {
    let session = sessions.get(peer_id).await.ok_or_else(|| MlinkError::PeerGone {
        peer_id: peer_id.to_string(),
    })?;
    let (_seq, payload) = session.recv().await?;
    Ok((MessageType::Message, payload))
}
