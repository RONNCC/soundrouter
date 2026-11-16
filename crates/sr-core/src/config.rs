use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_CH: usize = 64;
pub const MAX_FRAMES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    pub version: u32,
    pub devices: Vec<VirtualDevice>,
} // version = 1

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualDevice {
    pub id: String, // stable short id, e.g. "v1"; driver UID = DEVICE_UID_PREFIX + id
    pub name: String,
    pub enabled: bool,
    pub channels: u16, // 2..=MAX_CH, even
    pub volume_db: f32,
    pub muted: bool,
    pub sources: Vec<Source>,
    pub wires: Vec<Wire>,
    pub monitors: Vec<Monitor>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub kind: SourceKind,
    pub channels: u16,
    pub volume_db: f32,
    pub muted: bool,
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
    pub enabled: bool,
    pub volume_db: f32,
    pub wires: Vec<MonitorWire>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorWire {
    pub out_ch: u16,
    pub dst_ch: u16,
} // vdev ch -> monitor ch

impl Config {
    /// Checks ids are unique, channel counts are in range, and wires reference
    /// existing source ids and in-range channels.
    pub fn validate(&self) -> Result<(), String> {
        let mut dev_ids = HashSet::new();
        for dev in &self.devices {
            if !dev_ids.insert(dev.id.as_str()) {
                return Err(format!("duplicate device id '{}'", dev.id));
            }
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
    fn pass_thru_round_trips() {
        let mut cfg = example();
        cfg.devices[0].sources.push(Source {
            id: "p".into(),
            kind: SourceKind::PassThru,
            channels: 2,
            volume_db: 0.0,
            muted: false,
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
    fn rejects_dangling_wire() {
        let mut cfg = example();
        cfg.devices[0].wires[0].source = "nope".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_values() {
        type Mutate = fn(&mut Config);
        let bad: [(&str, Mutate); 10] = [
            ("dup device", |c| {
                let d = c.devices[0].clone();
                c.devices.push(d);
            }),
            ("dup source", |c| {
                let s = c.devices[0].sources[0].clone();
                c.devices[0].sources.push(s);
            }),
            ("odd channels", |c| c.devices[0].channels = 3),
            ("zero channels", |c| c.devices[0].channels = 0),
            ("too many channels", |c| c.devices[0].channels = 66),
            ("source zero ch", |c| c.devices[0].sources[0].channels = 0),
            ("src_ch range", |c| c.devices[0].wires[0].src_ch = 2),
            ("out_ch range", |c| c.devices[0].wires[0].out_ch = 2),
            ("monitor out_ch range", |c| {
                c.devices[0].monitors[0].wires[0].out_ch = 2
            }),
            ("source too wide", |c| c.devices[0].sources[0].channels = 65),
        ];
        for (name, f) in bad {
            let mut cfg = example();
            f(&mut cfg);
            assert!(cfg.validate().is_err(), "{name} accepted");
        }
        assert!(Config::default().validate().is_ok());
    }
}
