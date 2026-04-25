use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, RwLock};

use super::{Node, NodeEvent, NodeState, PeerState, HEARTBEAT_TIMEOUT};
use crate::core::connection::ConnectionManager;
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

            // Only claim "encrypted" on the wire when encryption is both
            // requested AND a session key exists. Otherwise the flag is a lie
            // and the peer will try to decrypt plaintext.
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
    /// The cleanup matters because a reader that only emits the event (and
    /// leaves the stale `Arc<dyn Connection>` in place) causes the next
    /// `send_raw` to fail with "Broken pipe", and blocks a re-dial to the
    /// same `app_uuid` via the dedup guard in `connect_peer`.
    ///
    /// Must be called after a successful `connect_peer` / `accept_incoming`
    /// for that peer; otherwise the reader will immediately see `PeerGone`
    /// and exit (still running the cleanup so nothing leaks).
    pub fn spawn_peer_reader(&self, peer_id: String) -> tokio::task::JoinHandle<()> {
        let connections = Arc::clone(&self.connections);
        let peer_states = Arc::clone(&self.peer_states);
        let peer_manager = Arc::clone(&self.peer_manager);
        let events_tx = self.events_tx.clone();
        let encrypt = self.config.encrypt;
        tokio::spawn(async move {
            loop {
                let res = recv_once(&connections, &peer_states, encrypt, &peer_id).await;
                match res {
                    Ok((msg_type, payload)) => {
                        if msg_type == MessageType::Message {
                            let _ = events_tx.send(NodeEvent::MessageReceived {
                                peer_id: peer_id.clone(),
                                payload,
                            });
                        }
                        // Heartbeats and other control frames are consumed
                        // silently — `recv_once` already refreshed the
                        // last-heartbeat timestamp.
                    }
                    Err(_) => {
                        // Mirror `disconnect_peer`: evict conn, drop peer,
                        // mark state Disconnected, fire the event. Done
                        // inline because this task only holds Arc handles,
                        // not `&Node`.
                        let removed = {
                            let mut guard = connections.lock().await;
                            guard.remove(&peer_id)
                        };
                        if let Some(conn) = removed {
                            let _ = conn.close().await;
                        }
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
