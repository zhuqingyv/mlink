//! Single-instance lock + daemon-info file management. `~/.mlink/daemon.json`
//! carries `{port, pid}`; we refuse to start if a previous daemon's PID is
//! still alive. Stale files are cleaned up automatically.

use std::path::PathBuf;

use mlink_core::protocol::errors::MlinkError;
use serde::{Deserialize, Serialize};

/// Contents of `~/.mlink/daemon.json`. Written on startup, removed on clean
/// shutdown. A stale file with a dead PID is overwritten rather than causing
/// the new instance to abort.
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub pid: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("another mlink-daemon is already running (pid={pid}, port={port})")]
    AlreadyRunning { pid: u32, port: u16 },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("mlink error: {0}")]
    Mlink(#[from] MlinkError),
    #[error("could not resolve home directory")]
    NoHome,
}

/// Default path for the daemon-info file. Honours `MLINK_DAEMON_FILE` when
/// set, otherwise falls back to `~/.mlink/daemon.json`. The override exists
/// primarily for integration tests that run the daemon in a tempdir.
pub fn daemon_info_path() -> Result<PathBuf, DaemonError> {
    if let Ok(p) = std::env::var("MLINK_DAEMON_FILE") {
        return Ok(PathBuf::from(p));
    }
    let home = dirs::home_dir().ok_or(DaemonError::NoHome)?;
    Ok(home.join(".mlink").join("daemon.json"))
}

/// Return true if a process with the given PID is still alive. Uses
/// `kill(pid, 0)` on Unix which returns 0 for a live PID, ESRCH for a dead
/// one, and EPERM when the PID exists but we can't signal it — the last case
/// still means "alive", so we treat ESRCH alone as proof of death.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Safety: `kill(pid, 0)` is a safe syscall wrapper with no side effects —
    // signal 0 is the "just check permissions/existence" probe.
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if res == 0 {
        return true;
    }
    // errno == ESRCH → no such process. Anything else (EPERM) → alive.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    // Windows support deferred — conservatively assume "not alive" so we
    // don't wedge a user who has no way to clear the file.
    false
}

/// Read `daemon.json` and, if it points at a live PID, refuse to start.
/// Returns `Ok(())` when the file doesn't exist, is unparseable, or the PID
/// is dead — any of which mean we can safely claim the slot. In the stale /
/// corrupt case we eagerly delete the file so a later crash between this
/// check and `write_daemon_info` can't leave the old `{port,pid}` visible
/// to `mlink status` or any other reader.
pub fn ensure_single_instance(path: &PathBuf) -> Result<(), DaemonError> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let info: DaemonInfo = match serde_json::from_slice(&raw) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "daemon.json unparseable, cleaning up before startup"
            );
            remove_daemon_info(path);
            return Ok(());
        }
    };
    if pid_alive(info.pid) {
        return Err(DaemonError::AlreadyRunning { pid: info.pid, port: info.port });
    }
    tracing::warn!(
        stale_pid = info.pid,
        stale_port = info.port,
        path = %path.display(),
        "daemon.json references a dead pid, cleaning up stale entry"
    );
    remove_daemon_info(path);
    Ok(())
}

/// Atomically-ish write the daemon info file. We write to a sibling tempfile
/// then rename so a crashed write doesn't leave a half-JSON blob on disk.
pub fn write_daemon_info(path: &PathBuf, info: &DaemonInfo) -> Result<(), DaemonError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(info)?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Best-effort cleanup. Ignores NotFound so a double-shutdown is idempotent.
pub fn remove_daemon_info(path: &PathBuf) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(error = %e, path = %path.display(), "failed to remove daemon info"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_info_roundtrip() {
        let dir = tempdir();
        let path = dir.join("daemon.json");
        let info = DaemonInfo { port: 12345, pid: std::process::id() };
        write_daemon_info(&path, &info).unwrap();
        // Our own PID is alive → ensure_single_instance must refuse.
        let err = ensure_single_instance(&path).unwrap_err();
        match err {
            DaemonError::AlreadyRunning { pid, port } => {
                assert_eq!(pid, info.pid);
                assert_eq!(port, info.port);
            }
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
        remove_daemon_info(&path);
    }

    #[test]
    fn stale_pid_is_ignored() {
        let dir = tempdir();
        let path = dir.join("daemon.json");
        // PID 0 is never a valid user process — pid_alive returns false, so
        // ensure_single_instance should succeed.
        let info = DaemonInfo { port: 54321, pid: 0 };
        write_daemon_info(&path, &info).unwrap();
        ensure_single_instance(&path).expect("stale pid should not block startup");
    }

    #[test]
    fn stale_pid_removes_daemon_info() {
        // Regression guard for the bug where `kill -9` left daemon.json on
        // disk with `{port, pid}` for the dead process. A subsequent startup
        // must not only ignore the stale entry but also wipe it, so status
        // probes run between `ensure_single_instance` and `write_daemon_info`
        // don't see the old values.
        let dir = tempdir();
        let path = dir.join("daemon.json");
        let info = DaemonInfo { port: 54321, pid: 0 };
        write_daemon_info(&path, &info).unwrap();
        assert!(path.exists(), "precondition: file written");
        ensure_single_instance(&path).expect("stale pid should not block startup");
        assert!(!path.exists(), "stale daemon.json should be deleted");
    }

    #[test]
    fn missing_file_is_ok() {
        let dir = tempdir();
        let path = dir.join("nope.json");
        ensure_single_instance(&path).expect("missing file should not block startup");
    }

    #[test]
    fn corrupt_file_is_ok() {
        let dir = tempdir();
        let path = dir.join("daemon.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not valid json").unwrap();
        ensure_single_instance(&path).expect("corrupt file should not block startup");
        assert!(!path.exists(), "corrupt daemon.json should be deleted");
    }

    #[test]
    fn live_pid_preserves_daemon_info() {
        // When another daemon really is running, we must NOT delete its
        // daemon.json — the error-exit path relies on the file staying put
        // so the live owner remains discoverable.
        let dir = tempdir();
        let path = dir.join("daemon.json");
        let info = DaemonInfo { port: 9999, pid: std::process::id() };
        write_daemon_info(&path, &info).unwrap();
        let err = ensure_single_instance(&path).unwrap_err();
        assert!(matches!(err, DaemonError::AlreadyRunning { .. }));
        assert!(path.exists(), "live daemon.json must be preserved");
        remove_daemon_info(&path);
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "mlink-daemon-test-{}-{}",
            std::process::id(),
            uniq()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn uniq() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
    }
}
