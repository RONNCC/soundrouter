//! `presets.json` next to `config.json`: atomic writes, versioned, forward-compatible.
use crate::fsio::{quarantine, write_atomic};
use crate::lock::support_dir;
use crate::preset::{PRESETS_VERSION, PresetFile};
use std::path::{Path, PathBuf};

pub fn default_path() -> Option<PathBuf> {
    Some(support_dir()?.join("presets.json"))
}

/// A real file is a few KB; anything this big is corrupt or hostile and is not read into memory.
const MAX_BYTES: u64 = 4 << 20;

fn file_version(text: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(text)
        .ok()?
        .get("version")?
        .as_u64()
}

/// The presets at `path`, plus a message when something was wrong.
///
/// - Missing file: no presets, no message.
/// - Written by a newer SoundRouter: no presets, a message, the file untouched (`save` refuses it too).
/// - Unreadable, oversized, corrupt or not UTF-8: moved to `presets.json.bad`, no presets, a message.
///
/// Unknown fields are ignored. A preset that no longer validates is kept; applying it reports why.
pub fn load(path: &Path) -> (PresetFile, Option<String>) {
    let set_aside = |problem: &str| {
        let kept = match quarantine(path) {
            Ok(p) => format!("saved as {}", p.display()),
            Err(e) => format!("could not move it aside: {e}"),
        };
        (
            PresetFile::default(),
            Some(format!("Ignoring {problem} ({kept})")),
        )
    };
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        return set_aside("oversized presets file");
    }
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (PresetFile::default(), None),
        Err(e) => {
            return (
                PresetFile::default(),
                Some(format!("Could not read presets: {e}")),
            );
        }
    };
    if let Some(v) = file_version(&text).filter(|&v| v > u64::from(PRESETS_VERSION)) {
        return (
            PresetFile::default(),
            Some(format!(
                "Presets were written by a newer SoundRouter (version {v}); ignoring them and leaving the file untouched"
            )),
        );
    }
    match serde_json::from_slice::<PresetFile>(&text) {
        Ok(f) => (f, None),
        Err(e) => set_aside(&format!("unreadable presets file: {e}")),
    }
}

/// Atomic write; refuses to replace a file written by a newer version.
pub fn save(path: &Path, presets: &PresetFile) -> std::io::Result<()> {
    if let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|t| file_version(&t))
        .filter(|&v| v > u64::from(PRESETS_VERSION))
    {
        return Err(std::io::Error::other(format!(
            "{} was written by a newer SoundRouter (version {v}); not overwriting it",
            path.display()
        )));
    }
    write_atomic(path, &serde_json::to_vec_pretty(presets)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, VirtualDevice};

    fn scene() -> Config {
        Config {
            devices: vec![VirtualDevice {
                id: "v1".into(),
                name: "Mix".into(),
                enabled: true,
                channels: 2,
                volume_db: -3.0,
                muted: false,
                sources: vec![],
                wires: vec![],
                monitors: vec![],
            }],
            ..Config::default()
        }
    }

    #[test]
    fn missing_file_is_empty_without_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let (f, msg) = load(&dir.path().join("presets.json"));
        assert_eq!((f, msg), (PresetFile::default(), None));
    }

    #[test]
    fn save_then_load_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/presets.json");
        let mut f = PresetFile::default();
        f.save_as("Streaming", &scene()).unwrap();
        f.save_as("Podcast", &scene()).unwrap();
        save(&path, &f).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        assert_eq!(load(&path), (f.clone(), None));
        f.delete("Podcast");
        save(&path, &f).unwrap();
        assert_eq!(load(&path).0.presets.len(), 1);
    }

    #[test]
    fn corrupt_file_is_backed_up_and_ignored_with_a_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presets.json");
        std::fs::write(&path, "{nope").unwrap();
        let (f, msg) = load(&path);
        assert_eq!(f, PresetFile::default());
        assert!(msg.unwrap().contains("presets.json.bad"));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("presets.json.bad")).unwrap(),
            "{nope"
        );
        // A fresh save does not touch the backup.
        save(&path, &f).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("presets.json.bad")).unwrap(),
            "{nope"
        );
    }

    #[test]
    fn wrong_shapes_and_non_utf8_are_corrupt_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presets.json");
        for bytes in [
            &br#"{"version":1,"presets":[{"devices":[]}]}"#[..], // preset without a name
            br#"[1,2]"#,
            &[0xff, 0xfe, b'{'],
        ] {
            std::fs::write(&path, bytes).unwrap();
            let (f, msg) = load(&path);
            assert_eq!(f, PresetFile::default());
            assert!(msg.is_some() && !path.exists());
        }
    }

    #[test]
    fn oversized_file_is_set_aside_unread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presets.json");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        let (_, msg) = load(&path);
        assert!(msg.unwrap().contains("oversized"));
        assert!(!path.exists());
    }

    #[test]
    fn newer_version_is_ignored_kept_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presets.json");
        let text = r#"{"version":2,"presets":[],"shiny":{"a":1}}"#;
        std::fs::write(&path, text).unwrap();
        let (f, msg) = load(&path);
        assert_eq!(f, PresetFile::default());
        assert!(msg.unwrap().contains("version 2"));
        let err = save(&path, &f).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn unknown_fields_load_and_an_invalid_preset_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("presets.json");
        std::fs::write(
            &path,
            r#"{"version":1,"extra":1,"presets":[{"name":"Odd","color":"red","buffer_frames":100,"devices":[]}]}"#,
        )
        .unwrap();
        let (f, msg) = load(&path);
        assert!(msg.is_none());
        assert_eq!(f.presets[0].buffer_frames, 100);
        assert!(crate::preset::plan(&Config::default(), &f.presets[0]).is_err());
    }
}
