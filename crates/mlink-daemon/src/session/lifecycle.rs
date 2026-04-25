use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::ws::{Message, WebSocket};
use futures::stream::StreamExt;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use super::dispatch::handle_text;
use super::events::forward_peer_event;
use super::outbound::send;
use super::{MAX_FRAME_BYTES, OUTBOUND_CAPACITY};
use crate::protocol::{codes, encode_frame, ErrorPayload};
use crate::{DaemonState, SessionEntry, SessionHandle, DAEMON_VERSION};

pub async fn run(mut socket: WebSocket, state: DaemonState) {
    // ---- send `ready` -----
    let ready = encode_frame(
        "ready",
        None,
        serde_json::json!({
            "app_uuid": state.node.app_uuid(),
            "version": DAEMON_VERSION,
        }),
    );
    if socket.send(Message::Text(ready)).await.is_err() {
        return;
    }

    let (tx, mut rx) = mpsc::channel::<String>(OUTBOUND_CAPACITY);
    let handle: SessionHandle = Arc::new(StdMutex::new(SessionEntry {
        subs: HashSet::new(),
        tx,
    }));
    {
        let mut sessions = state.sessions.lock().expect("sessions poisoned");
        sessions.push(Arc::clone(&handle));
    }
    let mut events = state.node_events.subscribe();

    // `StreamExt::next` on WebSocket is cancel-safe, as is `mpsc::Receiver::recv`
    // and `broadcast::Receiver::recv`. All three drive one session lifetime —
    // dropping the socket breaks the loop and deregisters from the sessions
    // list.
    loop {
        tokio::select! {
            ws_msg = socket.next() => {
                let Some(ws_msg) = ws_msg else { break; };
                let msg = match ws_msg {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::debug!(error = %e, "ws recv error");
                        break;
                    }
                };
                match msg {
                    Message::Text(t) => {
                        if t.len() > MAX_FRAME_BYTES {
                            let err = encode_frame(
                                "error",
                                None,
                                ErrorPayload {
                                    id: None,
                                    code: codes::PAYLOAD_TOO_LARGE,
                                    message: format!(
                                        "frame {} bytes exceeds {MAX_FRAME_BYTES}",
                                        t.len()
                                    ),
                                },
                            );
                            let _ = socket.send(Message::Text(err)).await;
                            continue;
                        }
                        if !handle_text(&mut socket, &state, &handle, &t).await {
                            break;
                        }
                    }
                    Message::Binary(b) => {
                        let err = encode_frame(
                            "error",
                            None,
                            ErrorPayload {
                                id: None,
                                code: codes::BAD_JSON,
                                message: format!("binary frames are not supported ({} bytes)", b.len()),
                            },
                        );
                        if socket.send(Message::Text(err)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            routed = rx.recv() => {
                // Messages delivered by the central fan-out worker. `None`
                // means every sender was dropped, which only happens at
                // shutdown — we'd be about to exit anyway.
                let Some(frame) = routed else { break; };
                if !send(&mut socket, frame).await {
                    break;
                }
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        if !forward_peer_event(&mut socket, &state, &handle, ev).await {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "session event bridge lagged");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    // Deregister this session. The fan-out worker will notice the dropped
    // sender on its next push and tolerate the failure, but removing the
    // entry here keeps the list bounded.
    {
        let mut sessions = state.sessions.lock().expect("sessions poisoned");
        sessions.retain(|s| !Arc::ptr_eq(s, &handle));
    }
    // Disconnect path never retires daemon-level rooms: the daemon owns the
    // membership and keeps its device-to-device connections alive even when
    // no WS client is listening. Subscription state dies with the session.
}
