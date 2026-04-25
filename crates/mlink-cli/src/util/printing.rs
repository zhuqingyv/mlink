//! Shared pretty-printers for the CLI. Kept out of the command modules so
//! both `scan` and `doctor` can reuse the same Bluetooth-permission hint
//! text, and so `print_peer_list` has a single home across legacy commands.

use mlink_core::transport::DiscoveredPeer;

/// Print a Bluetooth permission remediation hint. macOS silently returns an
/// empty scan list when Core Bluetooth access is denied, so we print this on
/// both explicit error *and* an empty discover result — the user can't tell
/// the difference otherwise.
pub(crate) fn print_bluetooth_permission_hint() {
    eprintln!("[mlink] If this persists, Bluetooth access may be denied.");
    eprintln!(
        "[mlink] Fix: System Settings → Privacy & Security → Bluetooth → add your terminal app"
    );
    eprintln!("[mlink] Or install as app: bash scripts/install.sh");
}

pub(crate) fn print_peer_list(peers: &[DiscoveredPeer]) {
    if peers.is_empty() {
        println!("no mlink devices found");
        return;
    }
    println!("{:<40}  {:<24}  {}", "ID", "NAME", "RSSI");
    for p in peers {
        let rssi = match p.rssi {
            Some(v) => format!("{} dBm", v),
            None => "-".into(),
        };
        println!("{:<40}  {:<24}  {}", p.id, p.name, rssi);
    }
}
