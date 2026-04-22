//! Verifies that `build_state()` auto-joins every room persisted in
//! `~/.mlink/rooms.json` (or the override pointed at by `MLINK_ROOMS_FILE`)
//! before handing control back to the caller. No mocks — we seed a real file,
//! call the real `build_state`, and assert via the real Node API.
//!
//! Both scenarios (seeded file / missing file) share the `MLINK_ROOMS_FILE`
//! process-wide env var, so they're folded into a single serial test to avoid
//! cross-test interference.

use mlink_core::core::room::room_hash;
use mlink_daemon::build_state;

#[tokio::test(flavor = "current_thread")]
async fn startup_auto_joins_persisted_rooms_and_missing_file_is_empty() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();

    // --- Scenario A: seeded file ---
    let seeded = std::env::temp_dir().join(format!("mlink-rooms-seed-{}.json", nanos));
    std::fs::write(&seeded, br#"["123456","567892"]"#).unwrap();
    std::env::set_var("MLINK_ROOMS_FILE", &seeded);

    let state = build_state().await.expect("build_state (seeded)");
    let hashes = state.node.room_hashes();
    assert!(
        hashes.contains(&room_hash("123456")),
        "room 123456 should be auto-joined at startup"
    );
    assert!(
        hashes.contains(&room_hash("567892")),
        "room 567892 should be auto-joined at startup"
    );
    let mut listed = state.rooms.lock().unwrap().list();
    listed.sort();
    assert_eq!(listed, vec!["123456".to_string(), "567892".to_string()]);
    drop(state);
    let _ = std::fs::remove_file(&seeded);

    // --- Scenario B: missing file → empty ---
    let missing = std::env::temp_dir().join(format!("mlink-rooms-missing-{}.json", nanos));
    assert!(!missing.exists());
    std::env::set_var("MLINK_ROOMS_FILE", &missing);

    let state = build_state().await.expect("build_state (missing)");
    assert!(
        state.node.room_hashes().is_empty(),
        "no persisted rooms → no auto-joins"
    );
    assert!(state.rooms.lock().unwrap().list().is_empty());
}
