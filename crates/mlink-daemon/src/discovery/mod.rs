//! Background discovery. Started once at daemon boot so every WS client shares
//! a single scanner + accept loop per transport. Join/leave only toggles the
//! set of room hashes the node validates against at handshake time — the
//! scanner itself runs with no room filter so peers from every active room can
//! be surfaced through the same transport.
//!
//! Transport selection is driven by `MLINK_DAEMON_TRANSPORT`:
//! - unset (default) — `Dual`: BLE + TCP in parallel (falls back to TCP-only
//!   when BLE is unavailable, e.g. non-macOS hosts)
//! - `"tcp"` — TCP only
//! - `"ble"` — BLE only (macOS); falls back to TCP if unavailable
//! - `"dual"` — explicit opt-in, same as unset
//!
//! `spawn(node, transport)` returns a `TransportControllers` handle which the
//! WS layer uses to turn individual transports on and off at runtime without
//! tearing down the daemon. Already-established connections keep running —
//! disabling a transport only stops its discovery / accept loops.

use std::sync::{Arc, Mutex as StdMutex};

use mlink_core::core::node::Node;
use tokio::task::JoinHandle;

mod ble_ctl;
mod connect;
mod tcp_ctl;

pub use connect::connect_loop;

/// Choice of link-layer transport. Parsed from `MLINK_DAEMON_TRANSPORT` —
/// anything other than a literal `"tcp"` / `"ble"` / `"dual"` falls back to
/// the default (`Dual`) so a typo doesn't silently boot a radio the user
/// didn't ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonTransport {
    Tcp,
    Ble,
    Dual,
}

impl DaemonTransport {
    /// Parse `MLINK_DAEMON_TRANSPORT`. Unset or unrecognised values yield
    /// `Dual` — we default to starting both radios because the product-level
    /// expectation is that a daemon speaks whatever link-layer its peers do.
    pub fn from_env() -> Self {
        match std::env::var("MLINK_DAEMON_TRANSPORT").as_deref() {
            Ok("tcp") => DaemonTransport::Tcp,
            Ok("ble") => DaemonTransport::Ble,
            Ok("dual") => DaemonTransport::Dual,
            _ => DaemonTransport::Dual,
        }
    }

    fn label(self) -> &'static str {
        match self {
            DaemonTransport::Tcp => "tcp",
            DaemonTransport::Ble => "ble",
            DaemonTransport::Dual => "dual",
        }
    }
}

/// Handle returned by `spawn` — owns the abort handles for every running
/// transport so the WS layer can flip individual kinds on and off at runtime.
#[derive(Clone)]
pub struct TransportControllers {
    node: Arc<Node>,
    inner: Arc<StdMutex<Inner>>,
}

struct Inner {
    tcp: Option<TransportHandles>,
    ble: Option<TransportHandles>,
}

struct TransportHandles {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for TransportHandles {
    fn drop(&mut self) {
        for h in self.tasks.drain(..) {
            h.abort();
        }
    }
}

/// Simple identifier used on the WS wire for runtime enable/disable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKindWire {
    Tcp,
    Ble,
}

impl TransportKindWire {
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "tcp" => Some(Self::Tcp),
            "ble" => Some(Self::Ble),
            _ => None,
        }
    }
}

impl TransportControllers {
    /// Snapshot of which transports are currently running. Used by the
    /// `transport_state` frame so the UI can render the on/off checkboxes.
    pub fn snapshot(&self) -> (bool, bool) {
        let g = self.inner.lock().expect("transport ctl poisoned");
        (g.tcp.is_some(), g.ble.is_some())
    }

    /// Turn on the named transport if it isn't already running. Re-enabling a
    /// transport that's already up is a no-op (returns `Ok(false)`).
    pub fn enable(&self, kind: TransportKindWire) -> Result<bool, String> {
        let mut g = self.inner.lock().expect("transport ctl poisoned");
        match kind {
            TransportKindWire::Tcp => {
                if g.tcp.is_some() {
                    return Ok(false);
                }
                g.tcp = Some(tcp_ctl::spawn_tcp_controlled(Arc::clone(&self.node)));
                Ok(true)
            }
            TransportKindWire::Ble => {
                if g.ble.is_some() {
                    return Ok(false);
                }
                match ble_ctl::spawn_ble_controlled(Arc::clone(&self.node)) {
                    Some(h) => {
                        g.ble = Some(h);
                        Ok(true)
                    }
                    None => Err("ble transport unavailable on this platform".into()),
                }
            }
        }
    }

    /// Turn off the named transport. Already-established connections keep
    /// running — we only tear down the discovery / accept loops. No-op on a
    /// transport that isn't running.
    pub fn disable(&self, kind: TransportKindWire) -> bool {
        let mut g = self.inner.lock().expect("transport ctl poisoned");
        match kind {
            TransportKindWire::Tcp => g.tcp.take().is_some(),
            TransportKindWire::Ble => g.ble.take().is_some(),
        }
    }
}

/// Spawn long-lived discovery + auto-connect loops for the requested mode.
/// Returns a `TransportControllers` used later by the WS layer to enable /
/// disable individual transports at runtime. Errors bubble up as tracing
/// warnings — the daemon keeps serving WS regardless.
pub fn spawn(node: Arc<Node>, transport: DaemonTransport) -> TransportControllers {
    let mut tcp = None;
    let mut ble = None;
    match transport {
        DaemonTransport::Tcp => {
            tcp = Some(tcp_ctl::spawn_tcp_controlled(Arc::clone(&node)));
        }
        DaemonTransport::Ble => {
            ble = ble_ctl::spawn_ble_controlled(Arc::clone(&node));
            if ble.is_none() {
                tracing::warn!(
                    "ble unavailable; falling back to tcp-only discovery"
                );
                tcp = Some(tcp_ctl::spawn_tcp_controlled(Arc::clone(&node)));
            }
        }
        DaemonTransport::Dual => {
            tcp = Some(tcp_ctl::spawn_tcp_controlled(Arc::clone(&node)));
            ble = ble_ctl::spawn_ble_controlled(Arc::clone(&node));
            if ble.is_none() {
                tracing::warn!(
                    "ble unavailable; running tcp-only until ble is enabled"
                );
            }
        }
    }
    tracing::info!(transport = transport.label(), "discovery loop started");
    TransportControllers {
        node,
        inner: Arc::new(StdMutex::new(Inner { tcp, ble })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_from_str_covers_both_kinds() {
        assert_eq!(TransportKindWire::from_wire("tcp"), Some(TransportKindWire::Tcp));
        assert_eq!(TransportKindWire::from_wire("ble"), Some(TransportKindWire::Ble));
        assert_eq!(TransportKindWire::from_wire("wifi"), None);
    }
}
