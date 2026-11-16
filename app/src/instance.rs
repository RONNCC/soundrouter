//! Single-instance guard: an advisory file lock held for the process lifetime.
//! The OS drops it on exit or crash, so a stale lock can never block a launch.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

pub const BUNDLE_ID: &str = "com.ronncc.soundrouter";

pub fn lock_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/SoundRouter/instance.lock"))
}

/// Held lock; released on drop.
pub struct Guard(#[allow(dead_code)] File);

/// `Ok(None)` means another instance holds the lock.
pub fn acquire(path: &Path) -> std::io::Result<Option<Guard>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(Guard(f))),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// Test/CI paths never contend for the real lock.
pub fn bypasses_guard(demo: bool) -> bool {
    demo
}

/// Ask the running (bundled) instance to come forward; best effort.
pub fn raise_existing() {
    let ok = std::process::Command::new("open")
        .args(["-b", BUNDLE_ID])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("SoundRouter is already running.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_until_first_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/instance.lock");
        let first = acquire(&p).unwrap().expect("first gets lock");
        assert!(acquire(&p).unwrap().is_none());
        drop(first);
        assert!(acquire(&p).unwrap().is_some());
    }

    #[test]
    fn demo_bypasses() {
        assert!(bypasses_guard(true));
        assert!(!bypasses_guard(false));
    }
}
