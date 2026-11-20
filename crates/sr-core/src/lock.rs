//! Advisory OS file locks shared by the app and `sr-cli`. The OS drops a lock when its holder
//! exits or crashes, so a stale lock file never blocks anything.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

/// `~/Library/Application Support/SoundRouter`.
pub fn support_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/SoundRouter"))
}

/// Single app instance; its content is the holder's app bundle path (see the app's `instance`).
pub fn instance_lock_path() -> Option<PathBuf> {
    Some(support_dir()?.join("instance.lock"))
}

/// Whoever drives the audio engine (the app or `sr-cli run`) holds this. Content is unused.
pub fn engine_lock_path() -> Option<PathBuf> {
    Some(support_dir()?.join("engine.lock"))
}

/// Try to take the lock at `path` (parent dirs created, file never truncated). `Ok(None)` means
/// another holder has it. The lock lasts as long as the returned file stays open.
pub fn try_lock(path: &Path) -> std::io::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_fails_until_first_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/engine.lock");
        let first = try_lock(&p).unwrap().expect("first gets lock");
        assert!(try_lock(&p).unwrap().is_none());
        drop(first);
        assert!(try_lock(&p).unwrap().is_some());
    }

    #[test]
    fn stale_lock_file_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("engine.lock");
        std::fs::write(&p, "left behind").unwrap();
        assert!(try_lock(&p).unwrap().is_some());
    }
}
