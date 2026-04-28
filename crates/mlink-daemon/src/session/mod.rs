//! Per-connection WS session. Parses envelopes, routes to handlers, and
//! forwards Node events / routed backlog messages to the client based on its
//! room subscription set.
//!
//! Sessions no longer own the room on behalf of the daemon. Subscription is
//! purely a *routing filter*: the daemon is already joined to every room in
//! the persistent RoomStore, and backlog messages accumulate in `state.queue`
//! whether any WS client is listening or not. A `join` frame adds the code to
//! the client's sub set and drains any accumulated backlog into the socket;
//! `leave` removes the sub and, only if no other live session still holds the
//! code, retires the room from the Node + RoomStore. A WS disconnect removes
//! subs silently — daemon-side membership is unaffected.

mod dispatch;
mod events;
mod handlers;
mod handlers_send;
mod lifecycle;
mod outbound;
mod transport_debug;
mod transport_forward;
mod transport_list;
mod transport_toggle;
mod validate;

pub use lifecycle::run;
pub use transport_forward::spawn_session_event_forwarder;

/// Maximum accepted frame size, in bytes. Matches the product rule ("WS
/// single message upper bound 1 MB"). Oversize frames are rejected with
/// `payload_too_large` before being parsed.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Outbound mpsc capacity. Sized generously relative to `MAX_MESSAGES_PER_ROOM`
/// so a join-time drain of one full backlog fits without back-pressure, while
/// staying small enough that a wedged socket doesn't grow memory unboundedly.
const OUTBOUND_CAPACITY: usize = 1024;
