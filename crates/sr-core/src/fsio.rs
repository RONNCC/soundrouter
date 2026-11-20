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

/// Write to a temp file next to the target (`<name>.tmp`), fsync it, then rename over the target.
/// Parent directories are created; errors name the path.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let ctx = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(ctx)?;
    }
    let tmp = path.with_extension("json.tmp");
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
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

    #[test]
    fn write_atomic_creates_parents_replaces_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/x.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert!(!p.with_extension("json.tmp").exists());
    }
}
