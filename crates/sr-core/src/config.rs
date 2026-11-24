use crate::driver_proto::{COMPANION_SUFFIX, MAX_DEVICES};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::HashSet;

pub const MAX_CH: usize = 64;
pub const MAX_FRAMES: usize = 4096;
/// Longest device name; matches the driver, which truncates beyond it.
pub const MAX_NAME_CHARS: usize = 64;
/// Mixer nodes per virtual device, channels per node, and channels over all of a device's nodes.
pub const MAX_NODES: usize = 16;
pub const MAX_NODE_CH: usize = 8;
pub const MAX_NODE_CH_TOTAL: usize = 32;
/// Effects in one node's insert list, the longest `Fx::Delay` in ms, and delay-line channels over a
/// device's nodes (each is a 200 ms ring, 54 KB at 48 kHz).
pub const MAX_FX: usize = 4;
pub const MAX_DELAY_MS: f32 = 200.0;
pub const MAX_DELAY_CH: usize = 8;
/// Highest accepted volume in dB; anything lower (even -200) is valid and just means near-silence.
pub const MAX_DB: f32 = 24.0;
/// Highest `version` this build reads. [`Config`] writes the lowest version that can hold its content
/// (1 unless a wire carries a trim), so a file stays readable by older builds until it uses something
/// they would silently ignore; see `Config::written_version`.
pub const CONFIG_VERSION: u32 = 2;
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

#[derive(Clone, Debug, PartialEq, Deserialize)]
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
            version: 1,
            devices: Vec::new(),
            buffer_frames: DEFAULT_BUFFER_FRAMES,
        }
    }
}

impl Serialize for Config {
    /// Writes `version` as [`Config::written_version`], not the field: an old build reads a file that
    /// uses nothing new, and refuses (rather than half-understands) one that does.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Out<'a> {
            version: u32,
            devices: &'a [VirtualDevice],
            buffer_frames: u32,
        }
        Out {
            version: self.written_version(),
            devices: &self.devices,
            buffer_frames: self.buffer_frames,
        }
        .serialize(s)
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
    /// Mixer strips between the sources and the output channels, see `docs/NODES.md`. A wire reaches a
    /// node through `Wire::to` and leaves it through `Wire::source`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<Node>,
}

/// A mixer strip: `channels` rows, each the sum of its incoming wires, then a fader and a mute.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Unique among the device's sources and nodes: a wire's `source` or `to` names it.
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default = "stereo")]
    pub channels: u16,
    /// Fader, `<= MAX_DB`; applied as the gain of the node's outgoing wires.
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub muted: bool,
    /// Inserts, run in order over the summed rows before the fader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fx: Vec<Fx>,
}

/// A built-in effect of a node (`docs/NODES.md` section 2). The list is closed: an unknown `type` fails
/// the parse, so a build never half-understands a file that uses an effect it lacks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fx {
    /// Average each channel pair (0,1), (2,3), ...
    Mono,
    /// Zero look-ahead soft clip: `ceiling_db` -24..=0, `knee_db` 0..=12.
    Limiter {
        #[serde(default = "limiter_ceiling")]
        ceiling_db: f32,
        #[serde(default = "limiter_knee")]
        knee_db: f32,
    },
    /// Low and high shelf: `low_hz` 20..=2000, `high_hz` 1000..=20000, gains -18..=18 dB.
    Eq {
        #[serde(default = "eq_low_hz")]
        low_hz: f32,
        #[serde(default)]
        low_db: f32,
        #[serde(default = "eq_high_hz")]
        high_hz: f32,
        #[serde(default)]
        high_db: f32,
    },
    /// Sample-accurate delay, `ms` 0..=`MAX_DELAY_MS`; adds exactly that much latency.
    Delay {
        #[serde(default)]
        ms: f32,
    },
}
fn limiter_ceiling() -> f32 {
    -1.0
}
fn limiter_knee() -> f32 {
    6.0
}
fn eq_low_hz() -> f32 {
    120.0
}
fn eq_high_hz() -> f32 {
    8000.0
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

/// One cable: source channel -> vdev channel, with an optional trim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wire {
    pub source: String,
    pub src_ch: u16,
    pub out_ch: u16,
    /// Trim on this cable alone, `<= MAX_DB`; 0 is unity and is not written. Two mono sources onto one
    /// output channel add +6 dB, a -6 dB trim per wire fixes it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub gain_db: f32,
    /// Flip polarity (the wire's gain is negated); false is not written.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    /// Node id this wire feeds; `out_ch` is then the node's input channel. Absent: a device output
    /// channel, as before. When `source` names a node, `src_ch` is that node's output channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}
fn is_zero(x: &f32) -> bool {
    *x == 0.0
}
impl Wire {
    /// A plain cable at unity.
    pub fn new(source: impl Into<String>, src_ch: u16, out_ch: u16) -> Wire {
        Wire {
            source: source.into(),
            src_ch,
            out_ch,
            gain_db: 0.0,
            invert: false,
            to: None,
        }
    }
    /// Whether an older build would misread this wire (see [`Config::written_version`]).
    fn needs_v2(&self) -> bool {
        self.gain_db != 0.0 || self.invert || self.to.is_some()
    }
}

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

/// Node ids are unique among sources and nodes (`ids` holds the sources'), counts and channel counts are
/// within the caps, faders sane.
fn validate_nodes<'a>(dev: &'a VirtualDevice, ids: &mut HashSet<&'a str>) -> Result<(), String> {
    if dev.nodes.len() > MAX_NODES {
        return Err(format!(
            "device '{}': {} nodes, at most {MAX_NODES}",
            dev.id,
            dev.nodes.len()
        ));
    }
    let mut total = 0;
    let mut delay_ch = 0;
    for n in &dev.nodes {
        if n.id.is_empty() || n.id.len() > MAX_ID_BYTES || n.id.contains('\0') {
            return Err(format!(
                "device '{}': node id '{}' must be 1..={MAX_ID_BYTES} bytes without NUL",
                dev.id, n.id
            ));
        }
        if !ids.insert(n.id.as_str()) {
            return Err(format!(
                "device '{}': node id '{}' is also another source or node id",
                dev.id, n.id
            ));
        }
        if n.name.chars().count() > MAX_NAME_CHARS {
            return Err(format!(
                "node '{}': name longer than {MAX_NAME_CHARS} characters",
                n.id
            ));
        }
        if !(1..=MAX_NODE_CH).contains(&usize::from(n.channels)) {
            return Err(format!(
                "node '{}': channels {} must be in 1..={MAX_NODE_CH}",
                n.id, n.channels
            ));
        }
        total += usize::from(n.channels);
        check_db(&format!("node '{}'", n.id), n.gain_db)?;
        if n.fx.len() > MAX_FX {
            return Err(format!(
                "node '{}': {} effects, at most {MAX_FX}",
                n.id,
                n.fx.len()
            ));
        }
        for fx in &n.fx {
            check_fx(&n.id, fx)?;
            if matches!(fx, Fx::Delay { .. }) {
                delay_ch += usize::from(n.channels);
            }
        }
    }
    if delay_ch > MAX_DELAY_CH {
        return Err(format!(
            "device '{}': {delay_ch} delay channels, at most {MAX_DELAY_CH}",
            dev.id
        ));
    }
    if total > MAX_NODE_CH_TOTAL {
        return Err(format!(
            "device '{}': {total} node channels, at most {MAX_NODE_CH_TOTAL}",
            dev.id
        ));
    }
    Ok(())
}

/// Every parameter finite and in its range.
fn check_fx(node: &str, fx: &Fx) -> Result<(), String> {
    let ok = |what: &str, x: f32, lo: f32, hi: f32| {
        if x.is_finite() && (lo..=hi).contains(&x) {
            Ok(())
        } else {
            Err(format!(
                "node '{node}': {what} {x} must be finite and in {lo}..={hi}"
            ))
        }
    };
    match *fx {
        Fx::Mono => Ok(()),
        Fx::Limiter {
            ceiling_db,
            knee_db,
        } => {
            ok("limiter ceiling_db", ceiling_db, -24.0, 0.0)?;
            ok("limiter knee_db", knee_db, 0.0, 12.0)
        }
        Fx::Eq {
            low_hz,
            low_db,
            high_hz,
            high_db,
        } => {
            ok("eq low_hz", low_hz, 20.0, 2000.0)?;
            ok("eq low_db", low_db, -18.0, 18.0)?;
            ok("eq high_hz", high_hz, 1000.0, 20000.0)?;
            ok("eq high_db", high_db, -18.0, 18.0)
        }
        Fx::Delay { ms } => ok("delay ms", ms, 0.0, MAX_DELAY_MS),
    }
}

/// Kahn's algorithm over the node-to-node wires; what it cannot order is a cycle (a self wire included).
fn check_acyclic(dev: &VirtualDevice) -> Result<(), String> {
    let pos = |id: &str| dev.nodes.iter().position(|n| n.id == id);
    let edges: Vec<(usize, usize)> = dev
        .wires
        .iter()
        .filter_map(|w| Some((pos(&w.source)?, pos(w.to.as_deref()?)?)))
        .collect();
    let mut indeg = vec![0usize; dev.nodes.len()];
    for &(_, to) in &edges {
        indeg[to] += 1;
    }
    let mut ready: Vec<usize> = (0..indeg.len()).filter(|&i| indeg[i] == 0).collect();
    let mut done = 0;
    while let Some(n) = ready.pop() {
        done += 1;
        for &(from, to) in &edges {
            if from == n {
                indeg[to] -= 1;
                if indeg[to] == 0 {
                    ready.push(to);
                }
            }
        }
    }
    match indeg.iter().position(|&d| d > 0) {
        Some(i) if done < indeg.len() => Err(format!(
            "device '{}': node '{}' is part of a loop; nodes only feed forward",
            dev.id, dev.nodes[i].id
        )),
        _ => Ok(()),
    }
}

impl VirtualDevice {
    /// Whether an older build would misread this device (see [`Config::written_version`]).
    pub fn needs_v2(&self) -> bool {
        !self.nodes.is_empty() || self.wires.iter().any(Wire::needs_v2)
    }
}

impl Config {
    /// The lowest version that can hold this content: 1 unless a wire has a trim or goes to a node, or a node exists. Serde ignores unknown
    /// fields, so a v1 build reading trims would route at unity and then overwrite the file on its next
    /// save; at version 2 it takes the newer-version path instead (defaults, file untouched).
    pub fn written_version(&self) -> u32 {
        1 + u32::from(self.devices.iter().any(VirtualDevice::needs_v2))
    }

    /// Checks version, at most `MAX_DEVICES` enabled, ids are unique, names usable, volumes sane, channel counts in range,
    /// and wires reference existing source ids and in-range channels.
    /// `MonitorWire::dst_ch` is not checked: the hardware channel count is unknown here,
    /// so the mixer silently drops wires to channels the monitor device lacks.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=CONFIG_VERSION).contains(&self.version) {
            return Err(format!(
                "unsupported config version {} (expected 1..={CONFIG_VERSION})",
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
            validate_nodes(dev, &mut src_ids)?;
            for w in &dev.wires {
                let src_ch_max = if let Some(s) = dev.sources.iter().find(|s| s.id == w.source) {
                    s.channels
                } else if let Some(n) = dev.nodes.iter().find(|n| n.id == w.source) {
                    n.channels
                } else {
                    return Err(format!(
                        "device '{}': wire references unknown source '{}'",
                        dev.id, w.source
                    ));
                };
                if w.src_ch >= src_ch_max {
                    return Err(format!(
                        "device '{}': wire src_ch {} out of range for source '{}' ({} ch)",
                        dev.id, w.src_ch, w.source, src_ch_max
                    ));
                }
                let (dst, dst_ch) = match &w.to {
                    None => ("the device".to_string(), dev.channels),
                    Some(to) => match dev.nodes.iter().find(|n| n.id == *to) {
                        Some(n) => (format!("node '{to}'"), n.channels),
                        None => {
                            return Err(format!(
                                "device '{}': wire to '{to}', which is not a node",
                                dev.id
                            ));
                        }
                    },
                };
                if w.out_ch >= dst_ch {
                    return Err(format!(
                        "device '{}': wire out_ch {} out of range for {dst} ({dst_ch} ch)",
                        dev.id, w.out_ch
                    ));
                }
                check_db(
                    &format!("device '{}': wire from '{}'", dev.id, w.source),
                    w.gain_db,
                )?;
            }
            check_acyclic(dev)?;
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
        cfg.devices[0].wires.push(Wire::new("p", 1, 0));
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
        let bad: [(&str, Mutate); 25] = [
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
            ("version 3", |c| c.version = 3),
            ("version 0", |c| c.version = 0),
            ("blank name", |c| c.devices[0].name = " \t ".into()),
            ("long name", |c| c.devices[0].name = "x".repeat(65)),
            ("nan device volume", |c| c.devices[0].volume_db = f32::NAN),
            ("loud source", |c| c.devices[0].sources[1].volume_db = 24.5),
            ("inf monitor volume", |c| {
                c.devices[0].monitors[0].volume_db = f32::NEG_INFINITY
            }),
            ("mono app source", |c| c.devices[0].sources[0].channels = 1),
            ("loud wire", |c| c.devices[0].wires[0].gain_db = 24.5),
            ("nan wire", |c| c.devices[0].wires[0].gain_db = f32::NAN),
            ("inf wire", |c| {
                c.devices[0].wires[0].gain_db = f32::INFINITY
            }),
        ];
        for (name, f) in bad {
            let mut cfg = example();
            f(&mut cfg);
            assert!(cfg.validate().is_err(), "{name} accepted");
        }
        let good: [(&str, Mutate); 6] = [
            ("wire max gain", |c| c.devices[0].wires[0].gain_db = 24.0),
            ("wire near silence", |c| {
                c.devices[0].wires[0].gain_db = -200.0
            }),
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

    /// A config without trims is written exactly as before, at version 1; the first trim moves it to
    /// version 2 and back again when removed, and a v1-labelled file holding trims still loads.
    #[test]
    fn trims_write_the_lowest_version_that_holds_them() {
        let mut cfg = example();
        let plain = serde_json::to_string(&cfg).unwrap();
        assert!(plain.starts_with(r#"{"version":1,"#));
        assert!(!plain.contains("gain_db") && !plain.contains("invert"));
        assert_eq!(cfg.written_version(), 1);
        cfg.devices[0].wires[0].gain_db = -6.0;
        assert_eq!(cfg.written_version(), 2);
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.starts_with(r#"{"version":2,"#));
        assert!(json.contains(r#""out_ch":0,"gain_db":-6.0}"#));
        let back: Config = serde_json::from_str(&json).unwrap();
        back.validate().unwrap();
        assert_eq!(back.devices, cfg.devices);
        cfg.devices[0].wires[0].gain_db = 0.0;
        cfg.devices[0].wires[1].invert = true;
        assert!(
            serde_json::to_string(&cfg)
                .unwrap()
                .contains(r#""invert":true"#)
        );
        assert_eq!(cfg.written_version(), 2);
        cfg.devices[0].wires[1].invert = false;
        assert_eq!(serde_json::to_string(&cfg).unwrap(), plain);
        let relabelled = json.replacen(r#""version":2"#, r#""version":1"#, 1);
        let loaded: Config = serde_json::from_str(&relabelled).unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.devices[0].wires[0].gain_db, -6.0);
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
