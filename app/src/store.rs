//! Config persistence: ~/Library/Application Support/SoundRouter/config.json.
use crate::model::default_config;
use sr_core::config::Config;
use std::path::{Path, PathBuf};

pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/SoundRouter/config.json"))
}

/// Highest config `version` this build understands.
const SUPPORTED_VERSION: u64 = 1;

/// `version` of a JSON document, if it has a numeric one.
fn file_version(text: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(text)
        .ok()?
        .get("version")?
        .as_u64()
}

fn future_version_msg(v: u64) -> String {
    format!(
        "Config was written by a newer SoundRouter (version {v}); using defaults and leaving the file untouched"
    )
}

/// Move a file we cannot use out of the way so the next save cannot destroy it.
/// `config.json.bad`, or `config.json.bad.<unix secs>[.n]` if that exists.
fn quarantine(path: &Path) -> std::io::Result<PathBuf> {
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

/// A real config is a few KB; anything this big is corrupt or hostile and is not read into memory.
const MAX_CONFIG_BYTES: u64 = 4 << 20;

/// Quarantine the unusable file at `path` and fall back to defaults with a message.
fn set_aside(path: &Path, problem: &str) -> (Config, Option<String>) {
    let kept = match quarantine(path) {
        Ok(p) => format!("saved as {}", p.display()),
        Err(e) => format!("could not move it aside: {e}"),
    };
    (
        default_config(),
        Some(format!("Ignoring {problem} ({kept})")),
    )
}

/// Missing file -> default config. Newer-version file -> defaults, file kept (and `save` refuses
/// to overwrite it). Corrupt or invalid file -> moved to `config.json.bad`, defaults, and a message.
pub fn load(path: &Path) -> (Config, Option<String>) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_CONFIG_BYTES) {
        return set_aside(path, "oversized config");
    }
    // Bytes, not a String: a file that is not UTF-8 is corrupt and must be quarantined like any other.
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (default_config(), None),
        Err(e) => {
            return (
                default_config(),
                Some(format!("Could not read config: {e}")),
            );
        }
    };
    if let Some(v) = file_version(&text).filter(|&v| v > SUPPORTED_VERSION) {
        return (default_config(), Some(future_version_msg(v)));
    }
    let problem = match serde_json::from_slice::<Config>(&text) {
        Ok(cfg) => match cfg.validate() {
            Ok(()) => return (cfg, None),
            Err(e) => format!("invalid config: {e}"),
        },
        Err(e) => format!("unreadable config: {e}"),
    };
    set_aside(path, &problem)
}

/// Write to a temp file next to the target, fsync it, then rename over the target.
/// Refuses to replace a file written by a newer version. Unknown JSON fields are not preserved
/// (shortcut: `Config` has no catch-all; the version refusal protects newer files, upgrade if
/// same-version extensions appear).
pub fn save(path: &Path, cfg: &Config) -> std::io::Result<()> {
    use std::io::Write;
    let ctx = |e: std::io::Error| std::io::Error::new(e.kind(), format!("{}: {e}", path.display()));
    if let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|t| file_version(&t))
        .filter(|&v| v > SUPPORTED_VERSION)
    {
        return Err(std::io::Error::other(format!(
            "{} was written by a newer SoundRouter (version {v}); not overwriting it",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(ctx)?;
    }
    let tmp = path.with_extension("json.tmp");
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(cfg)?)?;
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
    use crate::model::{Edit, apply};
    use sr_core::config::SourceKind;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sr-app-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trip() {
        let dir = tmpdir("rt");
        let path = dir.join("nested/config.json");
        let mut cfg = default_config();
        apply(&mut cfg, Edit::AddSource(0, SourceKind::PassThru, 0));
        apply(&mut cfg, Edit::AddMonitor(0, "spk".into(), 2));
        save(&path, &cfg).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        let (back, msg) = load(&path);
        assert_eq!(back, cfg);
        assert!(msg.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn oversized_file_is_quarantined_unread() {
        let dir = tmpdir("big");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().contains("oversized config"));
        assert!(!path.exists() && !bad_files(&path).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_file_gives_default() {
        let (cfg, msg) = load(&tmpdir("missing").join("config.json"));
        assert_eq!(cfg, default_config());
        assert!(msg.is_none());
    }

    fn bad_files(path: &Path) -> Vec<PathBuf> {
        let mut v: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".bad"))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn corrupt_file_is_quarantined_not_overwritten() {
        let dir = tmpdir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{nope").unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().contains("config.json.bad"));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json.bad")).unwrap(),
            "{nope"
        );
        // The next save writes a fresh file; the quarantined one survives.
        save(&path, &cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("config.json.bad")).unwrap(),
            "{nope"
        );
        assert_eq!(load(&path).0, cfg);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn non_utf8_file_is_quarantined_not_overwritten() {
        let dir = tmpdir("binary");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let junk = [0xff, 0xfe, b'{', 0x80];
        std::fs::write(&path, junk).unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().contains("config.json.bad"));
        save(&path, &cfg).unwrap();
        assert_eq!(std::fs::read(dir.join("config.json.bad")).unwrap(), junk);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unreadable_path_gives_defaults_and_says_why() {
        let dir = tmpdir("isdir");
        let path = dir.join("config.json");
        std::fs::create_dir_all(&path).unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().starts_with("Could not read config"));
        assert!(path.is_dir() && bad_files(&path).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_file_is_quarantined_and_second_one_gets_distinct_name() {
        let dir = tmpdir("invalid");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut bad = default_config();
        bad.devices.push(bad.devices[0].clone()); // duplicate id
        let text = serde_json::to_string(&bad).unwrap();
        std::fs::write(&path, &text).unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().contains("duplicate"));
        std::fs::write(&path, "garbage").unwrap();
        load(&path);
        let kept = bad_files(&path);
        assert_eq!(kept.len(), 2);
        let contents: Vec<_> = kept
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        assert!(contents.contains(&text) && contents.contains(&"garbage".to_string()));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn future_version_is_kept_and_save_refuses() {
        let dir = tmpdir("future");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let text = r#"{"version":2,"devices":[],"new_field":{"a":1}}"#;
        std::fs::write(&path, text).unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().contains("version 2"));
        let err = save(&path, &cfg).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert!(bad_files(&path).is_empty());
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn save_into_read_only_dir_errors_cleanly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir("ro");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::File::create(dir.join("probe")).is_ok() {
            // running as root: permissions are not enforced, nothing to test
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _ = std::fs::remove_dir_all(dir);
            return;
        }
        let path = dir.join("config.json");
        let err = save(&path, &default_config()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("config.json"));
        assert!(!path.exists() && !path.with_extension("json.tmp").exists());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_replaces_existing_current_version_file() {
        let dir = tmpdir("replace");
        let path = dir.join("config.json");
        save(&path, &default_config()).unwrap();
        let mut cfg = default_config();
        apply(&mut cfg, Edit::NewDevice);
        save(&path, &cfg).unwrap();
        assert_eq!(load(&path), (cfg, None));
        let _ = std::fs::remove_dir_all(dir);
    }
    use proptest::prelude::Strategy;

    proptest::proptest! {
        /// Whatever is on disk, `load` returns a config that validates, never panics, and never loses the file:
        /// it is still there, or it was moved to a `.bad` sibling byte for byte.
        #[test]
        fn any_file_loads_to_a_valid_config_and_is_never_lost(bytes in proptest::prop_oneof![
            proptest::collection::vec(proptest::prelude::any::<u8>(), 0..300),
            proptest::prelude::any::<u64>().prop_map(|v| format!(r#"{{"version":{v},"devices":[]}}"#).into_bytes()),
        ]) {
            static CASE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let dir = tmpdir(&format!("fuzz{}", CASE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.json");
            std::fs::write(&path, &bytes).unwrap();
            let (cfg, _msg) = load(&path);
            proptest::prop_assert!(cfg.validate().is_ok());
            let kept = path.exists() && std::fs::read(&path).unwrap() == bytes
                || bad_files(&path).iter().any(|p| std::fs::read(p).unwrap() == bytes);
            proptest::prop_assert!(kept, "file contents lost");
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
