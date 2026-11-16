//! Config persistence: ~/Library/Application Support/SoundRouter/config.json.
use crate::model::default_config;
use sr_core::config::Config;
use std::path::{Path, PathBuf};

pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/SoundRouter/config.json"))
}

/// Missing file -> default config. Unreadable or invalid file -> default plus a message.
pub fn load(path: &Path) -> (Config, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (default_config(), None),
        Err(e) => {
            return (
                default_config(),
                Some(format!("Could not read config: {e}")),
            );
        }
    };
    match serde_json::from_str::<Config>(&text) {
        Ok(cfg) => match cfg.validate() {
            Ok(()) => (cfg, None),
            Err(e) => (
                default_config(),
                Some(format!("Ignoring invalid config: {e}")),
            ),
        },
        Err(e) => (
            default_config(),
            Some(format!("Ignoring unreadable config: {e}")),
        ),
    }
}

/// Write to a temp file next to the target, then rename over it.
pub fn save(path: &Path, cfg: &Config) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(&tmp, path)
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
    fn missing_file_gives_default() {
        let (cfg, msg) = load(&tmpdir("missing").join("config.json"));
        assert_eq!(cfg, default_config());
        assert!(msg.is_none());
    }

    #[test]
    fn garbage_and_invalid_fall_back_with_message() {
        let dir = tmpdir("bad");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{nope").unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.is_some());
        let mut bad = default_config();
        bad.devices.push(bad.devices[0].clone()); // duplicate id
        std::fs::write(&path, serde_json::to_string(&bad).unwrap()).unwrap();
        let (cfg, msg) = load(&path);
        assert_eq!(cfg, default_config());
        assert!(msg.unwrap().contains("duplicate"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
