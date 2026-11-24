//! The app's own preferences: `~/Library/Application Support/SoundRouter/settings.json`, next to
//! config.json (the routing) and presets.json. A separate file, so "Restore default settings" never
//! touches routing, an older app that saves config.json never drops these fields, and the engine
//! options travel to the backend as one value.
use sr_core::backend::{EngineOptions, EngineStatus, ReadAheadState};
use sr_core::fsio;
use std::path::{Path, PathBuf};

/// Highest `version` this build understands. A newer file is left alone and defaults apply.
const SUPPORTED_VERSION: u64 = 1;

/// Anything this big is corrupt or hostile; a real file is a few hundred bytes.
const MAX_BYTES: u64 = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u64,
    /// The menu-bar item exists.
    pub show_in_menu_bar: bool,
    /// Closing the window hides it and routing continues; off, closing quits.
    pub keep_running_on_close: bool,
    pub engine: EngineOptions,
    /// The safety net turned read-ahead off after driver dropouts; it stays off, across launches,
    /// until the user presses "Try again".
    pub read_ahead_tripped: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SUPPORTED_VERSION,
            show_in_menu_bar: true,
            keep_running_on_close: true,
            engine: EngineOptions::default(),
            read_ahead_tripped: false,
        }
    }
}

impl Settings {
    /// The values a hand-edited or older file may hold that the app must not act on: a persisted
    /// trip beats `read_ahead`, and the capture margin is 1 or 2 buffers, never 0.
    fn sanitized(mut self) -> Self {
        if self.read_ahead_tripped {
            self.engine.read_ahead = false;
        }
        self.engine.tap_cushion_blocks = self.engine.tap_cushion_blocks.clamp(1, 2);
        self
    }

    /// Turn read-ahead on or off by the user's choice; turning it on is also "Try again".
    pub fn set_read_ahead(&mut self, on: bool) {
        self.engine.read_ahead = on;
        if on {
            self.read_ahead_tripped = false;
        }
    }

    /// The safety net fired: read-ahead is off and stays off until the user tries again.
    pub fn trip(&mut self) {
        self.engine.read_ahead = false;
        self.read_ahead_tripped = true;
    }
}

pub fn default_path() -> Option<PathBuf> {
    Some(crate::store::default_path()?.with_file_name("settings.json"))
}

fn set_aside(path: &Path, problem: &str) -> (Settings, Option<String>) {
    let kept = match fsio::quarantine(path) {
        Ok(p) => format!("saved as {}", p.display()),
        Err(e) => format!("could not move it aside: {e}"),
    };
    (
        Settings::default(),
        Some(format!("Ignoring {problem} ({kept})")),
    )
}

/// Missing file -> defaults. Newer-version file -> defaults, file untouched (and `save` refuses to
/// overwrite it). Corrupt file -> moved to `settings.json.bad`, defaults, and a message.
pub fn load(path: &Path) -> (Settings, Option<String>) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        return set_aside(path, "oversized settings");
    }
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
        Err(e) => {
            return (
                Settings::default(),
                Some(format!("Could not read settings: {e}")),
            );
        }
    };
    if let Some(v) = fsio::file_version(&text).filter(|&v| v > SUPPORTED_VERSION) {
        return (
            Settings::default(),
            Some(format!(
                "Settings were written by a newer SoundRouter (version {v}); using defaults and leaving the file untouched"
            )),
        );
    }
    match serde_json::from_slice::<Settings>(&text) {
        Ok(s) => (s.sanitized(), None),
        Err(e) => set_aside(path, &format!("unreadable settings: {e}")),
    }
}

/// Atomic write; refuses to replace a file written by a newer version.
pub fn save(path: &Path, s: &Settings) -> std::io::Result<()> {
    fsio::check_replaceable(path, SUPPORTED_VERSION)?;
    fsio::write_atomic(path, &serde_json::to_vec_pretty(s)?)
}

/// The settings and the read-ahead state as lines for "Copy diagnostics".
pub fn diagnostics(s: &Settings, status: &EngineStatus, overloads_since_on: u64) -> String {
    let on = |b: bool| if b { "on" } else { "off" };
    let state = match status.read_ahead {
        ReadAheadState::Unsupported => "unsupported (driver too old or missing)",
        ReadAheadState::Off => "off",
        ReadAheadState::On => "on",
    };
    let counters = |c: &[(String, u64)]| {
        if c.is_empty() {
            "none".to_owned()
        } else {
            c.iter()
                .map(|(id, n)| format!("{id}={n}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    [
        format!(
            "settings: menu bar item {}, keep running on close {}",
            on(s.show_in_menu_bar),
            on(s.keep_running_on_close)
        ),
        format!(
            "engine options: lower delay {}, split app capture {}, capture margin {} buffer(s)",
            on(s.engine.read_ahead),
            on(s.engine.tap_split),
            s.engine.tap_cushion_blocks
        ),
        format!(
            "lower delay (read-ahead): {state}, turned off by the safety net: {}, engine safety net: {}",
            if s.read_ahead_tripped { "yes" } else { "no" },
            if status.read_ahead_tripped {
                "tripped"
            } else {
                "quiet"
            }
        ),
        format!(
            "driver overloads: {} (since lower delay turned on: {overloads_since_on})",
            counters(&status.driver_overloads)
        ),
        format!("xruns: {}", counters(&status.xruns)),
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("sr-settings-{name}-"))
            .tempdir()
            .unwrap()
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn defaults_are_the_safe_ones() {
        let d = Settings::default();
        assert_eq!(d.version, 1);
        assert!(d.show_in_menu_bar && d.keep_running_on_close && !d.read_ahead_tripped);
        assert!(!d.engine.read_ahead && d.engine.tap_split && d.engine.tap_cushion_blocks == 1);
        assert_eq!(serde_json::from_str::<Settings>("{}").unwrap(), d);
    }

    #[test]
    fn a_missing_file_gives_defaults_and_no_message() {
        let dir = tmpdir("missing");
        assert_eq!(
            load(&dir.path().join("settings.json")),
            (Settings::default(), None)
        );
    }

    #[test]
    fn round_trip_keeps_every_field_and_leaves_no_temp_file() {
        let dir = tmpdir("rt");
        let path = dir.path().join("nested/settings.json");
        let s = Settings {
            show_in_menu_bar: false,
            keep_running_on_close: false,
            engine: EngineOptions {
                read_ahead: true,
                tap_split: false,
                tap_cushion_blocks: 2,
            },
            ..Settings::default()
        };
        save(&path, &s).unwrap();
        assert_eq!(files(path.parent().unwrap()), ["settings.json"]);
        assert_eq!(load(&path), (s, None));
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let dir = tmpdir("fields");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"version":1,"keep_running_on_close":false,"engine":{"tap_split":false,"future":7},"theme":"dark"}"#,
        )
        .unwrap();
        let (s, msg) = load(&path);
        assert!(msg.is_none());
        assert!(!s.keep_running_on_close && s.show_in_menu_bar);
        assert!(!s.engine.tap_split && s.engine.tap_cushion_blocks == 1);
    }

    #[test]
    fn a_corrupt_file_is_set_aside_with_a_message() {
        let dir = tmpdir("corrupt");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        let (s, msg) = load(&path);
        assert_eq!(s, Settings::default());
        assert!(msg.unwrap().contains("settings.json.bad"));
        assert_eq!(files(dir.path()), ["settings.json.bad"]);
        // Saving now writes a fresh file; the bad one stays.
        save(&path, &s).unwrap();
        assert_eq!(files(dir.path()), ["settings.json", "settings.json.bad"]);
    }

    #[test]
    fn a_wrongly_typed_field_counts_as_corrupt() {
        let dir = tmpdir("typed");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"show_in_menu_bar":"yes"}"#).unwrap();
        let (s, msg) = load(&path);
        assert_eq!(s, Settings::default());
        assert!(msg.unwrap().starts_with("Ignoring unreadable settings"));
    }

    #[test]
    fn a_newer_file_gives_defaults_and_is_never_overwritten() {
        let dir = tmpdir("newer");
        let path = dir.path().join("settings.json");
        let newer = r#"{"version":2,"show_in_menu_bar":false,"brand_new":true}"#;
        std::fs::write(&path, newer).unwrap();
        let (s, msg) = load(&path);
        assert_eq!(s, Settings::default());
        assert!(msg.unwrap().contains("version 2"));
        assert!(save(&path, &s).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
        assert_eq!(files(dir.path()), ["settings.json"]);
    }

    #[test]
    fn a_persisted_trip_beats_read_ahead_and_the_margin_is_never_zero() {
        let dir = tmpdir("sane");
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"read_ahead_tripped":true,"engine":{"read_ahead":true,"tap_cushion_blocks":0}}"#,
        )
        .unwrap();
        let (s, _) = load(&path);
        assert!(s.read_ahead_tripped && !s.engine.read_ahead);
        assert_eq!(s.engine.tap_cushion_blocks, 1);
        std::fs::write(&path, r#"{"engine":{"tap_cushion_blocks":9}}"#).unwrap();
        assert_eq!(load(&path).0.engine.tap_cushion_blocks, 2);
    }

    #[test]
    fn turning_read_ahead_on_is_trying_again() {
        let mut s = Settings::default();
        s.engine.read_ahead = true;
        s.trip();
        assert!(!s.engine.read_ahead && s.read_ahead_tripped);
        s.set_read_ahead(false);
        assert!(s.read_ahead_tripped, "off does not clear the trip");
        s.set_read_ahead(true);
        assert!(s.engine.read_ahead && !s.read_ahead_tripped);
    }

    #[test]
    fn diagnostics_name_the_settings_and_the_read_ahead_state() {
        let mut s = Settings::default();
        s.trip();
        let status = EngineStatus {
            read_ahead: ReadAheadState::Off,
            driver_overloads: vec![("v1".into(), 4)],
            xruns: vec![("v1".into(), 2)],
            ..EngineStatus::default()
        };
        let d = diagnostics(&s, &status, 3);
        for line in [
            "settings: menu bar item on, keep running on close on",
            "engine options: lower delay off, split app capture on, capture margin 1 buffer(s)",
            "lower delay (read-ahead): off, turned off by the safety net: yes, engine safety net: quiet",
            "driver overloads: v1=4 (since lower delay turned on: 3)",
            "xruns: v1=2",
        ] {
            assert!(d.lines().any(|l| l == line), "{line}\n{d}");
        }
    }
}
