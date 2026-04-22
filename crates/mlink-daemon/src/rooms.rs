//! Persistent room membership for the daemon. Stored as a JSON array at
//! `~/.mlink/rooms.json` so the daemon restores its subscriptions across
//! restarts and WS clients don't have to re-join every boot. The path can be
//! overridden via `MLINK_ROOMS_FILE`, primarily for tests.
//!
//! A corrupt file is treated as empty rather than fatal — we'd rather drop
//! stale subscriptions than refuse to start — and writes go through a sibling
//! tempfile + rename so a crashed write never leaves half-JSON on disk.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::DaemonError;

/// Default path for the room-list file. Honours `MLINK_ROOMS_FILE` when set,
/// otherwise falls back to `~/.mlink/rooms.json`.
pub fn rooms_file_path() -> Result<PathBuf, DaemonError> {
    if let Ok(p) = std::env::var("MLINK_ROOMS_FILE") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or(DaemonError::NoHome)?;
    Ok(home.join(".mlink").join("rooms.json"))
}

/// On-disk + in-memory store for the daemon's joined-room codes.
pub struct RoomStore {
    path: PathBuf,
    rooms: HashSet<String>,
}

impl RoomStore {
    /// Read the file at `path`. Missing or corrupt files yield an empty store
    /// rather than an error — the daemon must be able to boot from a clean
    /// slate or recover from a truncated write.
    pub fn load(path: PathBuf) -> Self {
        let rooms = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<String>>(&bytes) {
                Ok(v) => v.into_iter().collect(),
                Err(e) => {
                    tracing::warn!(error = %e, path = %path.display(), "rooms.json unparseable, starting empty");
                    HashSet::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "rooms.json unreadable, starting empty");
                HashSet::new()
            }
        };
        Self { path, rooms }
    }

    /// Persist the current set to disk. Writes are best-effort: a disk error
    /// is logged but not propagated, so a transient filesystem hiccup never
    /// blocks a WS join/leave.
    pub fn save(&self) {
        if let Err(e) = Self::write_atomic(&self.path, &self.rooms) {
            tracing::warn!(error = %e, path = %self.path.display(), "failed to persist rooms.json");
        }
    }

    /// Insert `code` and persist. Idempotent.
    pub fn add(&mut self, code: &str) {
        if self.rooms.insert(code.to_string()) {
            self.save();
        }
    }

    /// Remove `code` and persist. Idempotent.
    pub fn remove(&mut self, code: &str) {
        if self.rooms.remove(code) {
            self.save();
        }
    }

    /// Current snapshot of joined codes. Order is not guaranteed.
    pub fn list(&self) -> Vec<String> {
        self.rooms.iter().cloned().collect()
    }

    pub fn contains(&self, code: &str) -> bool {
        self.rooms.contains(code)
    }

    fn write_atomic(path: &Path, rooms: &HashSet<String>) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Sort for stable on-disk ordering — makes diffs and tests deterministic
        // without costing anything at daemon runtime.
        let mut sorted: Vec<&String> = rooms.iter().collect();
        sorted.sort();
        let bytes = serde_json::to_vec_pretty(&sorted)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!(
            "mlink-rooms-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempdir();
        let path = dir.join("rooms.json");
        let store = RoomStore::load(path);
        assert!(store.list().is_empty());
    }

    #[test]
    fn add_persists_and_reload_sees_it() {
        let dir = tempdir();
        let path = dir.join("rooms.json");

        let mut store = RoomStore::load(path.clone());
        store.add("123456");
        store.add("567892");
        store.add("123456"); // duplicate no-op

        let reloaded = RoomStore::load(path);
        let mut got = reloaded.list();
        got.sort();
        assert_eq!(got, vec!["123456".to_string(), "567892".to_string()]);
        assert!(reloaded.contains("123456"));
        assert!(reloaded.contains("567892"));
        assert!(!reloaded.contains("999999"));
    }

    #[test]
    fn remove_persists() {
        let dir = tempdir();
        let path = dir.join("rooms.json");

        let mut store = RoomStore::load(path.clone());
        store.add("123456");
        store.add("567892");
        store.remove("123456");
        store.remove("000000"); // non-existent, no-op

        let reloaded = RoomStore::load(path);
        assert_eq!(reloaded.list(), vec!["567892".to_string()]);
        assert!(!reloaded.contains("123456"));
    }

    #[test]
    fn save_writes_sorted_json_array() {
        let dir = tempdir();
        let path = dir.join("rooms.json");

        let mut store = RoomStore::load(path.clone());
        store.add("567892");
        store.add("123456");

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: Vec<String> = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, vec!["123456".to_string(), "567892".to_string()]);
    }

    #[test]
    fn corrupt_file_yields_empty_store() {
        let dir = tempdir();
        let path = dir.join("rooms.json");
        std::fs::write(&path, b"{ not a json array").unwrap();
        let store = RoomStore::load(path);
        assert!(store.list().is_empty());
    }

    #[test]
    fn empty_array_loads_empty() {
        let dir = tempdir();
        let path = dir.join("rooms.json");
        std::fs::write(&path, b"[]").unwrap();
        let store = RoomStore::load(path);
        assert!(store.list().is_empty());
    }
}
