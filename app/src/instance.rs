//! Single-instance guard: an advisory file lock held for the process lifetime.
//! The OS drops it on exit or crash, so a stale lock can never block a launch.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const BUNDLE_ID: &str = "com.ronncc.soundrouter";

pub fn lock_path() -> Option<PathBuf> {
    sr_core::lock::instance_lock_path()
}

/// `Ok(None)` means another instance holds the lock; the returned file holds it until dropped.
/// The holder records its app bundle in the file so that a second launch can raise exactly this
/// copy (see `raise_existing`).
pub fn acquire(path: &Path) -> std::io::Result<Option<File>> {
    let Some(f) = sr_core::lock::try_lock(path)? else {
        return Ok(None);
    };
    let exe = std::env::current_exe().ok();
    let app = exe.as_deref().and_then(bundle_of).unwrap_or(Path::new(""));
    // Best effort: without a record a second launch falls back to `open -b`.
    let _ = f
        .set_len(0)
        .and_then(|()| (&f).write_all(app.as_os_str().as_encoded_bytes()));
    Ok(Some(f))
}

/// `X.app` for an executable at `X.app/Contents/MacOS/<exe>`.
fn bundle_of(exe: &Path) -> Option<&Path> {
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension()? == "app").then_some(app)
}

/// The app bundle the lock holder recorded, if it still exists.
fn holder_app(lock: &Path) -> Option<PathBuf> {
    let app = PathBuf::from(std::fs::read_to_string(lock).ok()?);
    app.is_dir().then_some(app)
}

/// Test/CI paths never contend for the real lock.
pub fn bypasses_guard(demo: bool) -> bool {
    demo
}

/// Ask the running instance to come forward; best effort. `open <its bundle>` sends that very
/// process the Dock-click reopen event, which shows a hidden window; `open -b` goes to whichever
/// registered copy LaunchServices prefers, which may be another one that then just exits.
pub fn raise_existing() {
    let mut open = std::process::Command::new("/usr/bin/open");
    match lock_path().as_deref().and_then(holder_app) {
        Some(app) => open.arg(app),
        None => open.args(["-b", BUNDLE_ID]),
    };
    if !open.status().is_ok_and(|s| s.success()) {
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

    #[test]
    fn bundle_of_finds_the_app_around_its_executable() {
        assert_eq!(
            bundle_of(Path::new("/A/SoundRouter.app/Contents/MacOS/SoundRouter")),
            Some(Path::new("/A/SoundRouter.app"))
        );
        assert_eq!(bundle_of(Path::new("/r/target/release/SoundRouter")), None);
    }

    #[test]
    fn holder_replaces_a_stale_bundle_record() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("instance.lock");
        let old = dir.path().join("Old.app");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(&lock, old.to_str().unwrap()).unwrap();
        assert_eq!(holder_app(&lock), Some(old));
        let _g = acquire(&lock).unwrap().expect("gets lock");
        // The test binary is not inside an app bundle, so this holder records none.
        assert_eq!(holder_app(&lock), None);
    }
}
