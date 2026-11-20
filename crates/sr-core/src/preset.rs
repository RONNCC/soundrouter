//! Presets ("scenes"): the whole routing saved under a name, applied later as one undoable step.
//!
//! A preset holds every virtual device with its sources, wires, gains, mutes and monitors, plus the
//! Audio delay setting. [`plan`] merges one into the current config and says which driver-visible
//! changes that needs (new, turned-on or resized virtual devices); applying never changes the
//! driver's device list by itself, so the caller asks first when `Plan::changes` is not empty.
use crate::config::{Config, DEFAULT_BUFFER_FRAMES, MAX_NAME_CHARS, VirtualDevice};
use serde::{Deserialize, Serialize};

pub const PRESETS_VERSION: u32 = 1;

fn default_buffer() -> u32 {
    DEFAULT_BUFFER_FRAMES
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub name: String,
    #[serde(default = "default_buffer")]
    pub buffer_frames: u32,
    #[serde(default)]
    pub devices: Vec<VirtualDevice>,
}

/// The contents of `presets.json`. Unknown fields are ignored on load (and dropped on the next
/// save; a file with a newer `version` is never overwritten, see `preset_store`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PresetFile {
    pub version: u32,
    pub presets: Vec<Preset>,
}

impl Default for PresetFile {
    fn default() -> Self {
        PresetFile {
            version: PRESETS_VERSION,
            presets: Vec::new(),
        }
    }
}

/// A usable preset name: trimmed, 1..=`MAX_NAME_CHARS` characters, no control characters.
pub fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Give the preset a name.".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "Preset names can have at most {MAX_NAME_CHARS} characters."
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("Preset names cannot contain control characters.".into());
    }
    Ok(name.to_owned())
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

impl PresetFile {
    /// Case-insensitive lookup, so `soundrouter://preset/streaming` finds "Streaming".
    pub fn find(&self, name: &str) -> Option<&Preset> {
        self.presets
            .iter()
            .find(|p| p.name == name)
            .or_else(|| self.presets.iter().find(|p| same_name(&p.name, name)))
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.presets.iter().position(|p| same_name(&p.name, name))
    }

    /// Save `cfg`'s routing as `name`, replacing a preset of that name (any letter case).
    /// Returns whether one was replaced. The config must be valid: a preset is only ever a valid config.
    pub fn save_as(&mut self, name: &str, cfg: &Config) -> Result<bool, String> {
        let name = clean_name(name)?;
        cfg.validate()?;
        let preset = Preset {
            name,
            buffer_frames: cfg.buffer_frames,
            devices: cfg.devices.clone(),
        };
        Ok(match self.position(&preset.name) {
            Some(i) => {
                self.presets[i] = preset;
                true
            }
            None => {
                self.presets.push(preset);
                false
            }
        })
    }

    /// Rename `old` to `new`. Fails if `new` is another preset's name (changing only the letter
    /// case of the same preset is fine).
    pub fn rename(&mut self, old: &str, new: &str) -> Result<(), String> {
        let new = clean_name(new)?;
        let i = self
            .position(old)
            .ok_or_else(|| format!("There is no preset named \"{old}\"."))?;
        if self.position(&new).is_some_and(|j| j != i) {
            return Err(format!("A preset named \"{new}\" already exists."));
        }
        self.presets[i].name = new;
        Ok(())
    }

    /// Delete `name`; returns whether it existed.
    pub fn delete(&mut self, name: &str) -> bool {
        self.position(name)
            .map(|i| self.presets.remove(i))
            .is_some()
    }
}

/// A change to the virtual devices themselves (what the driver publishes) that applying needs.
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceChange {
    /// The preset's device does not exist here: it is added.
    Add(String),
    /// It exists but is turned off here: it is turned on.
    TurnOn(String),
    /// It is on here with another channel count: it gets the preset's.
    Channels(String, u16),
}

impl DeviceChange {
    /// One line for the confirm sheet.
    pub fn describe(&self) -> String {
        match self {
            DeviceChange::Add(n) => format!("Add {n}"),
            DeviceChange::TurnOn(n) => format!("Turn on {n}"),
            DeviceChange::Channels(n, c) => format!("Set {n} to {c} channels"),
        }
    }
}

/// What applying a preset would do: the resulting config (valid) and the device changes in it.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub config: Config,
    pub changes: Vec<DeviceChange>,
}

impl Plan {
    /// Title, question and button label for the confirm sheet; `None` when nothing needs asking.
    pub fn confirm(&self) -> Option<(String, String, &'static str)> {
        if self.changes.is_empty() {
            return None;
        }
        let resizes = self
            .changes
            .iter()
            .any(|c| matches!(c, DeviceChange::Channels(..)));
        Some(if resizes {
            (
                "Change virtual devices?".into(),
                "This preset changes your virtual devices; apply it?".into(),
                "Apply",
            )
        } else {
            let n = self.changes.len();
            let (noun, be, it) = if n == 1 {
                ("device", "is", "it")
            } else {
                ("devices", "are", "them")
            };
            (
                "Add virtual devices?".into(),
                format!("This preset needs {n} virtual {noun} that {be} not set up; add {it}?"),
                "Add devices",
            )
        })
    }
}

/// Merge `preset` into `current`. Devices are matched by id.
///
/// - A preset device that exists keeps its place, name and on/off state and takes the preset's
///   routing, gain, mute, monitors and channel count; a preset that has it on turns it on.
/// - A preset device that does not exist is appended.
/// - Devices the preset does not mention are left alone.
/// - The Audio delay setting becomes the preset's.
///
/// Fails with the config validation message when the result is not valid (a hand-edited preset,
/// or more enabled devices than the driver allows).
pub fn plan(current: &Config, preset: &Preset) -> Result<Plan, String> {
    let mut config = current.clone();
    config.buffer_frames = preset.buffer_frames;
    let mut changes = Vec::new();
    for p in &preset.devices {
        match config.devices.iter_mut().find(|d| d.id == p.id) {
            None => {
                config.devices.push(p.clone());
                if p.enabled {
                    changes.push(DeviceChange::Add(p.name.clone()));
                }
            }
            Some(d) => {
                let was = (d.enabled, d.channels);
                let merged = VirtualDevice {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    enabled: d.enabled || p.enabled,
                    ..p.clone()
                };
                *d = merged;
                match was {
                    (false, _) if d.enabled => changes.push(DeviceChange::TurnOn(d.name.clone())),
                    (true, ch) if ch != d.channels => {
                        changes.push(DeviceChange::Channels(d.name.clone(), d.channels));
                    }
                    _ => {}
                }
            }
        }
    }
    config
        .validate()
        .map_err(|e| format!("Preset \"{}\" cannot be applied: {e}", preset.name))?;
    Ok(Plan { config, changes })
}

/// What a `soundrouter://` link asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UrlAction {
    /// `soundrouter://preset/<name>`
    Preset(String),
}

const SCHEME: &str = "soundrouter://";

/// Parse a `soundrouter://preset/<percent-encoded name>` link. Query and fragment are ignored.
pub fn parse_url(url: &str) -> Result<UrlAction, String> {
    let url = url.trim();
    let rest = url
        .get(..SCHEME.len())
        .filter(|s| s.eq_ignore_ascii_case(SCHEME))
        .map(|_| &url[SCHEME.len()..])
        .ok_or_else(|| format!("Not a SoundRouter link: {url}"))?;
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let (action, arg) = rest.split_once('/').unwrap_or((rest, ""));
    if !action.eq_ignore_ascii_case("preset") {
        return Err(format!("Unknown SoundRouter link action \"{action}\"."));
    }
    let name = percent_decode(arg.trim_end_matches('/'))?;
    if name.trim().is_empty() {
        return Err("The link names no preset.".into());
    }
    Ok(UrlAction::Preset(name))
}

/// The link that applies `name`; the inverse of [`parse_url`].
pub fn preset_url(name: &str) -> String {
    let mut out = format!("{SCHEME}preset/");
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(s: &str) -> Result<String, String> {
    let bad = || format!("Bad escape in link: {s}");
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.bytes();
    while let Some(b) = it.next() {
        if b == b'%' {
            let hex: Vec<u8> = it.by_ref().take(2).collect();
            let hex = std::str::from_utf8(&hex).map_err(|_| bad())?;
            if hex.len() != 2 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(bad());
            }
            out.push(u8::from_str_radix(hex, 16).map_err(|_| bad())?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Monitor, MonitorWire, Source, SourceKind, Wire};

    fn dev(id: &str, name: &str) -> VirtualDevice {
        VirtualDevice {
            id: id.into(),
            name: name.into(),
            enabled: true,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            sources: vec![Source {
                id: "s1".into(),
                kind: SourceKind::PassThru,
                channels: 2,
                volume_db: 0.0,
                muted: false,
                mute_original: false,
                name: String::new(),
            }],
            wires: vec![Wire {
                source: "s1".into(),
                src_ch: 0,
                out_ch: 0,
            }],
            monitors: vec![],
        }
    }

    fn cfg(devs: Vec<VirtualDevice>) -> Config {
        Config {
            devices: devs,
            ..Config::default()
        }
    }

    /// Streaming scene: v1 muted at -6 dB with a monitor, 128-frame delay.
    fn streaming() -> Config {
        let mut d = dev("v1", "Mix");
        d.volume_db = -6.0;
        d.muted = true;
        d.monitors.push(Monitor {
            device_uid: "spk".into(),
            enabled: true,
            volume_db: -3.0,
            wires: vec![MonitorWire {
                out_ch: 0,
                dst_ch: 0,
            }],
            name: "Speakers".into(),
        });
        Config {
            buffer_frames: 128,
            ..cfg(vec![d])
        }
    }

    #[test]
    fn save_apply_round_trips_the_whole_routing() {
        let scene = streaming();
        let mut f = PresetFile::default();
        assert_eq!(f.save_as("Streaming", &scene), Ok(false));
        let json = serde_json::to_string(&f).unwrap();
        let f: PresetFile = serde_json::from_str(&json).unwrap();
        // Applied onto a different routing, everything comes back.
        let mut other = cfg(vec![dev("v1", "Mix")]);
        other.buffer_frames = 512;
        let plan = plan(&other, f.find("Streaming").unwrap()).unwrap();
        assert_eq!(plan.config, scene);
        assert!(plan.changes.is_empty());
        assert!(plan.confirm().is_none());
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_ones_default() {
        let json = r#"{"version":1,"future":true,"presets":[{"name":"A","extra":[1],
            "devices":[{"id":"v1","name":"V","new_thing":1,"sources":[],"wires":[],"monitors":[]}]}]}"#;
        let f: PresetFile = serde_json::from_str(json).unwrap();
        assert_eq!(f.presets[0].buffer_frames, DEFAULT_BUFFER_FRAMES);
        assert_eq!(f.presets[0].devices[0].channels, 2);
        assert!(f.presets[0].devices[0].enabled);
    }

    #[test]
    fn names_are_cleaned_unique_and_case_insensitive() {
        let c = cfg(vec![dev("v1", "V")]);
        let mut f = PresetFile::default();
        f.save_as("  Studio  ", &c).unwrap();
        assert_eq!(f.presets[0].name, "Studio");
        assert_eq!(f.save_as("studio", &c), Ok(true), "replaces, no duplicate");
        assert_eq!(f.presets.len(), 1);
        assert_eq!(f.find("STUDIO").unwrap().name, "studio");
        for bad in ["", "   ", "a\nb", &"x".repeat(MAX_NAME_CHARS + 1)] {
            assert!(f.save_as(bad, &c).is_err(), "{bad:?}");
        }
        assert!(f.save_as(&"x".repeat(MAX_NAME_CHARS), &c).is_ok());
    }

    #[test]
    fn save_refuses_an_invalid_config() {
        let mut c = cfg(vec![dev("v1", "V")]);
        c.devices[0].wires[0].source = "gone".into();
        assert!(PresetFile::default().save_as("x", &c).is_err());
    }

    #[test]
    fn rename_and_delete() {
        let c = cfg(vec![dev("v1", "V")]);
        let mut f = PresetFile::default();
        f.save_as("A", &c).unwrap();
        f.save_as("B", &c).unwrap();
        assert!(f.rename("A", "b").unwrap_err().contains("already exists"));
        assert!(f.rename("A", " ").is_err());
        assert!(f.rename("nope", "C").unwrap_err().contains("no preset"));
        f.rename("A", "a").unwrap(); // letter case only
        f.rename("a", "C").unwrap();
        assert_eq!(
            f.presets.iter().map(|p| &p.name[..]).collect::<Vec<_>>(),
            ["C", "B"]
        );
        assert!(f.delete("c") && !f.delete("c"));
        assert_eq!(f.presets.len(), 1);
    }

    #[test]
    fn missing_device_is_added_after_confirm() {
        let mut two = cfg(vec![dev("v1", "Mix"), dev("v2", "Chat")]);
        two.devices[1].volume_db = -9.0;
        let mut f = PresetFile::default();
        f.save_as("Two", &two).unwrap();
        let one = cfg(vec![dev("v1", "Mix")]);
        let p = plan(&one, f.find("Two").unwrap()).unwrap();
        assert_eq!(p.config.devices.len(), 2);
        assert_eq!(p.config.devices[1].volume_db, -9.0);
        assert_eq!(p.changes, [DeviceChange::Add("Chat".into())]);
        let (title, body, ok) = p.confirm().unwrap();
        assert_eq!(title, "Add virtual devices?");
        assert_eq!(
            body,
            "This preset needs 1 virtual device that is not set up; add it?"
        );
        assert_eq!(ok, "Add devices");
    }

    #[test]
    fn confirm_counts_several_devices() {
        let mut three = cfg(vec![dev("v1", "A"), dev("v2", "B"), dev("v3", "C")]);
        three.devices[0].enabled = false;
        let mut f = PresetFile::default();
        f.save_as("Three", &three).unwrap();
        let p = plan(&cfg(vec![dev("v1", "A")]), f.find("Three").unwrap()).unwrap();
        // v1 is on here, the preset has it off: it stays on; v2 and v3 are new.
        assert_eq!(p.changes.len(), 2);
        assert!(p.config.devices[0].enabled);
        let (_, body, _) = p.confirm().unwrap();
        assert_eq!(
            body,
            "This preset needs 2 virtual devices that are not set up; add them?"
        );
    }

    #[test]
    fn a_disabled_device_the_preset_turns_on_asks_first() {
        let mut off = dev("v1", "Mix");
        off.enabled = false;
        let now = cfg(vec![off]);
        let mut f = PresetFile::default();
        f.save_as("On", &cfg(vec![dev("v1", "Mix")])).unwrap();
        let p = plan(&now, f.find("On").unwrap()).unwrap();
        assert_eq!(p.changes, [DeviceChange::TurnOn("Mix".into())]);
        assert!(p.config.devices[0].enabled);
        // The reverse never turns a device off.
        let mut f = PresetFile::default();
        let mut off = cfg(vec![dev("v1", "Mix")]);
        off.devices[0].enabled = false;
        f.save_as("Off", &off).unwrap();
        let p = plan(&cfg(vec![dev("v1", "Mix")]), f.find("Off").unwrap()).unwrap();
        assert!(p.config.devices[0].enabled && p.changes.is_empty());
    }

    #[test]
    fn a_disabled_device_stays_disabled_without_questions() {
        let mut off = dev("v1", "Mix");
        off.enabled = false;
        off.volume_db = -12.0;
        let now = cfg(vec![off.clone()]);
        let mut f = PresetFile::default();
        f.save_as("P", &now).unwrap();
        let mut changed = now.clone();
        changed.devices[0].volume_db = 0.0;
        let p = plan(&changed, f.find("P").unwrap()).unwrap();
        assert_eq!(p.config, now);
        assert!(p.changes.is_empty());
    }

    #[test]
    fn channel_changes_ask_and_keep_the_device_name() {
        let mut wide = dev("v1", "Preset name");
        wide.channels = 4;
        wide.sources[0].channels = 4;
        let mut f = PresetFile::default();
        f.save_as("Wide", &cfg(vec![wide])).unwrap();
        let p = plan(&cfg(vec![dev("v1", "My name")]), f.find("Wide").unwrap()).unwrap();
        assert_eq!(p.config.devices[0].name, "My name");
        assert_eq!(p.config.devices[0].channels, 4);
        assert_eq!(p.changes, [DeviceChange::Channels("My name".into(), 4)]);
        assert_eq!(p.changes[0].describe(), "Set My name to 4 channels");
        let (title, _, ok) = p.confirm().unwrap();
        assert_eq!((title.as_str(), ok), ("Change virtual devices?", "Apply"));
    }

    #[test]
    fn devices_the_preset_does_not_mention_are_untouched() {
        let mut f = PresetFile::default();
        f.save_as("One", &cfg(vec![dev("v1", "A")])).unwrap();
        let mut now = cfg(vec![dev("v1", "A"), dev("v2", "B")]);
        now.devices[1].muted = true;
        let p = plan(&now, f.find("One").unwrap()).unwrap();
        assert_eq!(p.config.devices[1], now.devices[1]);
    }

    #[test]
    fn an_invalid_preset_is_refused_with_the_validation_message() {
        let mut bad = dev("v1", "A");
        bad.wires[0].out_ch = 9;
        let p = Preset {
            name: "Broken".into(),
            buffer_frames: 256,
            devices: vec![bad],
        };
        let e = plan(&cfg(vec![]), &p).unwrap_err();
        assert!(e.starts_with("Preset \"Broken\" cannot be applied"), "{e}");
        assert!(e.contains("out_ch 9"), "{e}");
        let odd = Preset {
            buffer_frames: 100,
            devices: vec![],
            ..p.clone()
        };
        assert!(
            plan(&cfg(vec![]), &odd)
                .unwrap_err()
                .contains("buffer_frames")
        );
        // Too many enabled devices once merged.
        let many = Preset {
            devices: (0..20).map(|i| dev(&format!("n{i}"), "N")).collect(),
            ..p
        };
        assert!(plan(&cfg(vec![]), &many).unwrap_err().contains("enabled"));
    }

    #[test]
    fn url_parsing() {
        let ok = |u: &str| parse_url(u).unwrap();
        assert_eq!(
            ok("soundrouter://preset/Streaming"),
            UrlAction::Preset("Streaming".into())
        );
        assert_eq!(
            ok("SoundRouter://PRESET/a%20b%2Fc/"),
            UrlAction::Preset("a b/c".into())
        );
        assert_eq!(
            ok("soundrouter://preset/Caf%C3%A9?x=1#y"),
            UrlAction::Preset("Café".into())
        );
        assert_eq!(
            ok("  soundrouter://preset/x  "),
            UrlAction::Preset("x".into())
        );
        for bad in [
            "",
            "https://preset/x",
            "soundrouter:/preset/x",
            "soundrouter://",
            "soundrouter://preset",
            "soundrouter://preset/",
            "soundrouter://preset/%20",
            "soundrouter://preset/%zz",
            "soundrouter://preset/%4",
            "soundrouter://preset/%+1",
            "soundrouter://preset/%ff",
            "soundrouter://driver/x",
            "soundrouter://é",
        ] {
            assert!(parse_url(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn url_round_trips_awkward_names() {
        for name in ["Streaming", "Café ☕ 100%", "a/b?c#d e", "--", "ü"] {
            assert_eq!(
                parse_url(&preset_url(name)),
                Ok(UrlAction::Preset(name.into())),
                "{name}"
            );
        }
        assert_eq!(preset_url("a b"), "soundrouter://preset/a%20b");
    }
}
