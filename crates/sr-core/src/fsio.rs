//! File helpers shared by the config and preset stores.
use std::io::Write;
use std::path::{Path, PathBuf};

/// Move a file we cannot use out of the way so the next save cannot destroy it.
/// `name.bad`, or `name.bad.<unix secs>[.n]` if that exists.
pub fn quarantine(path: &Path) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut dest = path.with_file_name(format!("{name}.bad"));
    let mut n = 0;
    while dest.exists() {
        let sfx = if n == 0 {
            String::new()
        } else {
            format!(".{n}")
        };
        dest = path.with_file_name(format!("{name}.bad.{secs}{sfx}"));
        n += 1;
    }
    std::fs::rename(path, &dest)?;
    Ok(dest)
}

/// `version` of a JSON document, if it has a numeric one.
pub fn file_version(text: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(text)
        .ok()?
        .get("version")?
        .as_u64()
}

/// Whether the file at `path` may be replaced by a save. Refused when it was written by a newer
/// version (`version` above `supported`), and when it exists but cannot be read right now: the
/// load fell back to defaults for it, and replacing it would destroy what we could not read.
pub fn check_replaceable(path: &Path, supported: u64) -> std::io::Result<()> {
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(std::io::Error::new(
                e.kind(),
                format!("{}: {e}; not overwriting it", path.display()),
            ));
        }
    };
    match file_version(&text).filter(|&v| v > supported) {
        Some(v) => Err(std::io::Error::other(format!(
            "{} was written by a newer SoundRouter (version {v}); not overwriting it",
            path.display()
        ))),
        None => Ok(()),
    }
}

/// Write to a temp file next to the target, fsync it, then rename over the target. The temp name
/// is unique per call (two writers, such as the app and `sr-cli`, never share one), and a symlink
/// at `path` is followed so it stays a symlink. Parent directories are created; errors name the path.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ctx = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(ctx)?;
    }
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let res = (|| {
        // `create_new`: a leftover temp file of a crashed process with the same pid is never reused.
        let (tmp, mut f) = loop {
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = path.with_file_name(format!(".{name}.{}.{n}.tmp", std::process::id()));
            match std::fs::File::create_new(&tmp) {
                Ok(f) => break (tmp, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        };
        let done = f
            .write_all(bytes)
            .and_then(|()| f.sync_all())
            .and_then(|()| std::fs::rename(&tmp, &path));
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done
    })();
    res.map_err(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarantine_never_overwrites_an_earlier_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        let mut kept = Vec::new();
        for i in 0..3 {
            std::fs::write(&p, format!("v{i}")).unwrap();
            kept.push(quarantine(&p).unwrap());
            assert!(!p.exists());
        }
        kept.sort();
        kept.dedup();
        assert_eq!(kept.len(), 3);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("x.json.bad")).unwrap(),
            "v0"
        );
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn write_atomic_creates_parents_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/x.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(names(p.parent().unwrap()), ["x.json"]);
    }

    #[test]
    fn concurrent_writers_never_fail_and_the_file_is_always_one_whole_payload() {
        // The app and `sr-cli` can save the same file at once. With one shared temp name the
        // writers truncate each other's temp file and the loser's rename fails or a torn file lands.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        let payloads: Vec<Vec<u8>> = (b'a'..=b'h').map(|c| vec![c; 256 * 1024]).collect();
        std::thread::scope(|s| {
            for payload in &payloads {
                let p = &p;
                s.spawn(move || {
                    for _ in 0..40 {
                        write_atomic(p, payload).unwrap();
                        let got = std::fs::read(p).unwrap();
                        assert_eq!(got.len(), payload.len());
                        assert!(got.iter().all(|b| *b == got[0]), "torn file");
                    }
                });
            }
        });
        assert_eq!(names(dir.path()), ["config.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_stays_a_symlink_and_its_target_is_updated() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/config.json");
        write_atomic(&real, b"one").unwrap();
        let link = dir.path().join("config.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, b"two").unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"two");
    }

    #[test]
    fn replace_guard_allows_missing_and_current_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        check_replaceable(&p, 1).unwrap();
        std::fs::write(&p, r#"{"version":1}"#).unwrap();
        check_replaceable(&p, 1).unwrap();
        std::fs::write(&p, "not json at all").unwrap();
        check_replaceable(&p, 1).unwrap();
    }

    #[test]
    fn replace_guard_refuses_a_newer_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        std::fs::write(&p, r#"{"version":2}"#).unwrap();
        let err = check_replaceable(&p, 1).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn replace_guard_refuses_a_file_it_cannot_read() {
        // A file the load could not read became "defaults" in memory; saving them must not
        // replace the user's real config.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        std::fs::write(&p, r#"{"version":1}"#).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            return; // running as root: permissions are not enforced
        }
        let err = check_replaceable(&p, 1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("not overwriting"), "{err}");
    }
}
