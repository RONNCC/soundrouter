use crate::driver_proto::{COMPANION_SUFFIX, MAX_DEVICES};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_CH: usize = 64;
pub const MAX_FRAMES: usize = 4096;
/// Longest device name; matches the driver, which truncates beyond it.
pub const MAX_NAME_CHARS: usize = 64;
/// Highest accepted volume in dB; anything lower (even -200) is valid and just means near-silence.
pub const MAX_DB: f32 = 24.0;
pub const CONFIG_VERSION: u32 = 1;
/// Longest device id; the driver uid `soundrouter:<id>` (and its `.engine` companion) must stay a valid driver uid.
pub const MAX_ID_BYTES: usize = 64;
/// IO buffer sizes (frames) the engine's aggregate may run at: each one adds one buffer of
/// latency; smaller ones wake the IOProc more often (more CPU and battery).
pub const BUFFER_FRAMES: [u32; 4] = [64, 128, 256, 512];
pub const DEFAULT_BUFFER_FRAMES: u32 = 256;

fn yes() -> bool {
    true
}
fn stereo() -> u16 {
    2
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub devices: Vec<VirtualDevice>,
    /// Engine IO buffer size in frames, one of [`BUFFER_FRAMES`]; global so every device shares it.
    pub buffer_frames: u32,
}
impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            devices: Vec::new(),
            buffer_frames: DEFAULT_BUFFER_FRAMES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualDevice {
    pub id: String,   // stable short id, e.g. "v1"; driver UID = DEVICE_UID_PREFIX + id
    pub name: String, // non-blank after trim, at most MAX_NAME_CHARS chars
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "stereo")]
    pub channels: u16, // 2..=MAX_CH, even
    #[serde(default)]
    pub volume_db: f32,
    #[serde(default)]
    pub muted: bool,
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub wires: Vec<Wire>,
    #[serde(default)]
    pub monitors: Vec<Monitor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub kind: SourceKind,
    #[serde(default = "stereo")]
    pub channels: u16,
    #[serde(default)]
    pub volume_db: f32,
    #[serde(default)]
    pub muted: bool,
    /// App sources only: silence the app's own output while it is tapped (`CATapMuteBehavior`).
    #[serde(default)]
    pub mute_original: bool,
    /// Name of the app or input device when it was last seen, so a UI can still name it while
    /// it is absent. Display only; may be empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceKind {
    App {
        bundle_id: String,
    },
    Device {
        uid: String,
    },
    /// What apps play directly into the virtual device; has the device's channel count.
    PassThru,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wire {
    pub source: String,
    pub src_ch: u16,
    pub out_ch: u16,
} // source ch -> vdev ch

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    pub device_uid: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub volume_db: f32,
    #[serde(default)]
    pub wires: Vec<MonitorWire>,
    /// Name of the output device when it was last seen (see `Source::name`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorWire {
    pub out_ch: u16,
    pub dst_ch: u16,
} // vdev ch -> monitor ch; dst_ch can't be range-checked here (hardware channel count unknown)

fn check_db(what: &str, db: f32) -> Result<(), String> {
    if db.is_finite() && db <= MAX_DB {
        Ok(())
    } else {
        Err(format!(
            "{what}: volume {db} dB must be finite and <= {MAX_DB}"
        ))
    }
}

impl Config {
    /// Checks version, at most `MAX_DEVICES` enabled, ids are unique, names usable, volumes sane, channel counts in range,
    /// and wires reference existing source ids and in-range channels.
    /// `MonitorWire::dst_ch` is not checked: the hardware channel count is unknown here,
    /// so the mixer silently drops wires to channels the monitor device lacks.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != CONFIG_VERSION {
            return Err(format!(
                "unsupported config version {} (expected {CONFIG_VERSION})",
                self.version
            ));
        }
        if !BUFFER_FRAMES.contains(&self.buffer_frames) {
            return Err(format!(
                "buffer_frames {} must be one of {BUFFER_FRAMES:?}",
                self.buffer_frames
            ));
        }
        // Each enabled device is a driver device; the driver refuses a longer list outright.
        let enabled = self.devices.iter().filter(|d| d.enabled).count();
        if enabled > MAX_DEVICES {
            return Err(format!(
                "{enabled} enabled devices; at most {MAX_DEVICES} can be enabled at once"
            ));
        }
        let mut dev_ids = HashSet::new();
        for dev in &self.devices {
            if !dev_ids.insert(dev.id.as_str()) {
                return Err(format!("duplicate device id '{}'", dev.id));
            }
            if dev.id.is_empty()
                || dev.id.len() > MAX_ID_BYTES
                || dev.id.contains('\0')
                || dev.id.ends_with(COMPANION_SUFFIX)
            {
                return Err(format!(
                    "device id '{}' must be 1..={MAX_ID_BYTES} bytes without NUL and not end with {COMPANION_SUFFIX}",
                    dev.id
                ));
            }
            if dev.name.trim().is_empty() {
                return Err(format!("device '{}': name is empty", dev.id));
            }
            if dev.name.chars().count() > MAX_NAME_CHARS {
                return Err(format!(
                    "device '{}': name longer than {MAX_NAME_CHARS} characters",
                    dev.id
                ));
            }
            check_db(&format!("device '{}'", dev.id), dev.volume_db)?;
            let ch = usize::from(dev.channels);
            if !(2..=MAX_CH).contains(&ch) || ch % 2 != 0 {
                return Err(format!(
                    "device '{}': channels {} must be even and in 2..={MAX_CH}",
                    dev.id, dev.channels
                ));
            }
            let mut src_ids = HashSet::new();
            for src in &dev.sources {
                if !src_ids.insert(src.id.as_str()) {
                    return Err(format!(
                        "device '{}': duplicate source id '{}'",
                        dev.id, src.id
                    ));
                }
                let sc = usize::from(src.channels);
                if !(1..=MAX_CH).contains(&sc) {
                    return Err(format!(
                        "source '{}': channels {} must be in 1..={MAX_CH}",
                        src.id, src.channels
                    ));
                }
                check_db(&format!("source '{}'", src.id), src.volume_db)?;
                // A process tap is always a stereo mixdown; other channels would never carry audio.
                if matches!(src.kind, SourceKind::App { .. }) && src.channels != 2 {
                    return Err(format!(
                        "source '{}': app sources are stereo, channels {} must be 2",
                        src.id, src.channels
                    ));
                }
                if src.kind == SourceKind::PassThru && src.channels != dev.channels {
                    return Err(format!(
                        "source '{}': pass_thru channels {} must equal device channels {}",
                        src.id, src.channels, dev.channels
                    ));
                }
            }
            for w in &dev.wires {
                let Some(src) = dev.sources.iter().find(|s| s.id == w.source) else {
                    return Err(format!(
                        "device '{}': wire references unknown source '{}'",
                        dev.id, w.source
                    ));
                };
                if w.src_ch >= src.channels {
                    return Err(format!(
                        "device '{}': wire src_ch {} out of range for source '{}' ({} ch)",
                        dev.id, w.src_ch, src.id, src.channels
                    ));
                }
                if w.out_ch >= dev.channels {
                    return Err(format!(
                        "device '{}': wire out_ch {} out of range ({} ch)",
                        dev.id, w.out_ch, dev.channels
                    ));
                }
            }
            for m in &dev.monitors {
                check_db(&format!("monitor '{}'", m.device_uid), m.volume_db)?;
                for mw in &m.wires {
                    if mw.out_ch >= dev.channels {
                        return Err(format!(
                            "device '{}': monitor '{}' out_ch {} out of range ({} ch)",
                            dev.id, m.device_uid, mw.out_ch, dev.channels
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{"version":1,"devices":[{"id":"v1","name":"Loopback Audio","enabled":true,"channels":2,"volume_db":0.0,"muted":false,
 "sources":[{"id":"s1","kind":{"type":"app","bundle_id":"com.apple.Safari"},"channels":2,"volume_db":0.0,"muted":false},
            {"id":"s2","kind":{"type":"device","uid":"BuiltInMicrophoneDevice"},"channels":1,"volume_db":0.0,"muted":false}],
 "wires":[{"source":"s1","src_ch":0,"out_ch":0},{"source":"s1","src_ch":1,"out_ch":1},
          {"source":"s2","src_ch":0,"out_ch":0},{"source":"s2","src_ch":0,"out_ch":1}],
 "monitors":[{"device_uid":"BuiltInSpeakerDevice","enabled":true,"volume_db":-6.0,
              "wires":[{"out_ch":0,"dst_ch":0},{"out_ch":1,"dst_ch":1}]}]}]}"#;

    #[test]
    fn example_round_trips() {
        let cfg: Config = serde_json::from_str(EXAMPLE).unwrap();
        cfg.validate().unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, back);
    }

    fn example() -> Config {
        serde_json::from_str(EXAMPLE).unwrap()
    }

    #[test]
    fn mute_original_defaults_false_and_round_trips() {
        let mut cfg = example();
        assert!(!cfg.devices[0].sources[0].mute_original);
        cfg.devices[0].sources[0].mute_original = true;
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(cfg, serde_json::from_str::<Config>(&json).unwrap());
    }

    #[test]
    fn buffer_frames_defaults_for_old_configs_and_is_checked() {
        let mut cfg = example(); // EXAMPLE predates the field
        assert_eq!(cfg.buffer_frames, DEFAULT_BUFFER_FRAMES);
        for n in BUFFER_FRAMES {
            cfg.buffer_frames = n;
            cfg.validate().unwrap();
            let json = serde_json::to_string(&cfg).unwrap();
            assert_eq!(cfg, serde_json::from_str::<Config>(&json).unwrap());
        }
        for n in [0, 63, 100, 1024] {
            cfg.buffer_frames = n;
            assert!(cfg.validate().unwrap_err().contains("buffer_frames"));
        }
    }

    #[test]
    fn names_are_optional_omitted_when_empty_and_round_trip() {
        let mut cfg = example();
        assert!(cfg.devices[0].sources[1].name.is_empty());
        assert!(cfg.devices[0].monitors[0].name.is_empty());
        assert!(
            !serde_json::to_string(&cfg)
                .unwrap()
                .contains(r#""name":"""#)
        );
        cfg.devices[0].sources[1].name = "USB Mic".into();
        cfg.devices[0].monitors[0].name = "Speakers".into();
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(cfg, serde_json::from_str::<Config>(&json).unwrap());
    }

    #[test]
    fn pass_thru_round_trips() {
        let mut cfg = example();
        cfg.devices[0].sources.push(Source {
            id: "p".into(),
            kind: SourceKind::PassThru,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            mute_original: false,
            name: String::new(),
        });
        cfg.devices[0].wires.push(Wire {
            source: "p".into(),
            src_ch: 1,
            out_ch: 0,
        });
        cfg.validate().unwrap();
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains(r#""kind":{"type":"pass_thru"}"#));
        assert_eq!(cfg, serde_json::from_str::<Config>(&json).unwrap());
        cfg.devices[0].sources[2].channels = 4;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn channel_counts_must_match_exactly_and_be_at_least_two() {
        let mut cfg = example();
        cfg.devices[0].channels = 0;
        assert!(cfg.validate().unwrap_err().contains("channels"));
        // pass_thru must have the device's channel count: neither narrower nor wider.
        let mut cfg = example();
        cfg.devices[0].channels = 4;
        cfg.devices[0].sources.push(Source {
            id: "p".into(),
            kind: SourceKind::PassThru,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            mute_original: false,
            name: String::new(),
        });
        assert!(cfg.validate().unwrap_err().contains("pass_thru"));
        cfg.devices[0].sources[2].channels = 4;
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_dangling_wire() {
        let mut cfg = example();
        cfg.devices[0].wires[0].source = "nope".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_values() {
        type Mutate = fn(&mut Config);
        let bad: [(&str, Mutate); 22] = [
            ("dup device", |c| {
                let d = c.devices[0].clone();
                c.devices.push(d);
            }),
            ("dup source", |c| {
                let s = c.devices[0].sources[0].clone();
                c.devices[0].sources.push(s);
            }),
            ("empty id", |c| c.devices[0].id.clear()),
            ("long id", |c| {
                c.devices[0].id = "v".repeat(MAX_ID_BYTES + 1)
            }),
            ("nul id", |c| c.devices[0].id = "v\0".into()),
            ("companion id", |c| c.devices[0].id = "v1.engine".into()),
            ("odd channels", |c| c.devices[0].channels = 3),
            ("zero channels", |c| c.devices[0].channels = 0),
            ("too many channels", |c| c.devices[0].channels = 66),
            ("source zero ch", |c| c.devices[0].sources[1].channels = 0),
            ("src_ch range", |c| c.devices[0].wires[0].src_ch = 2),
            ("out_ch range", |c| c.devices[0].wires[0].out_ch = 2),
            ("monitor out_ch range", |c| {
                c.devices[0].monitors[0].wires[0].out_ch = 2
            }),
            ("source too wide", |c| c.devices[0].sources[1].channels = 65),
            ("version 2", |c| c.version = 2),
            ("version 0", |c| c.version = 0),
            ("blank name", |c| c.devices[0].name = " \t ".into()),
            ("long name", |c| c.devices[0].name = "x".repeat(65)),
            ("nan device volume", |c| c.devices[0].volume_db = f32::NAN),
            ("loud source", |c| c.devices[0].sources[1].volume_db = 24.5),
            ("inf monitor volume", |c| {
                c.devices[0].monitors[0].volume_db = f32::NEG_INFINITY
            }),
            ("mono app source", |c| c.devices[0].sources[0].channels = 1),
        ];
        for (name, f) in bad {
            let mut cfg = example();
            f(&mut cfg);
            assert!(cfg.validate().is_err(), "{name} accepted");
        }
        let good: [(&str, Mutate); 4] = [
            ("max gain", |c| c.devices[0].volume_db = 24.0),
            ("near silence", |c| {
                c.devices[0].sources[0].volume_db = -200.0
            }),
            ("64 char name", |c| c.devices[0].name = "é".repeat(64)),
            ("mono device source", |c| {
                c.devices[0].sources[1].channels = 1
            }),
        ];
        for (name, f) in good {
            let mut cfg = example();
            f(&mut cfg);
            cfg.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert!(Config::default().validate().is_ok());
    }

    #[test]
    fn enabled_devices_are_capped_at_what_the_driver_hosts() {
        // The driver refuses a list longer than MAX_DEVICES, which failed the whole apply.
        let mut cfg = example();
        let d = cfg.devices[0].clone();
        cfg.devices = (0..=MAX_DEVICES)
            .map(|i| VirtualDevice {
                id: format!("v{i}"),
                ..d.clone()
            })
            .collect();
        assert!(cfg.validate().unwrap_err().contains("enabled devices"));
        // Disabled devices never reach the driver, so they do not count.
        cfg.devices[0].enabled = false;
        cfg.validate().unwrap();
    }

    #[test]
    fn minimal_json_parses_with_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg, Config::default());
        let cfg: Config = serde_json::from_str(
            r#"{"devices":[{"id":"v","name":"N","sources":[{"id":"s","kind":{"type":"pass_thru"},"channels":2}],
                "monitors":[{"device_uid":"u"}]}]}"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let d = &cfg.devices[0];
        assert!(d.enabled && !d.muted && d.channels == 2 && d.volume_db == 0.0);
        assert!(d.wires.is_empty() && d.monitors[0].enabled && d.monitors[0].wires.is_empty());
    }
}
