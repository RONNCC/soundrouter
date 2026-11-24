//! Pure edit operations on the `Config` the GUI owns. The UI never mutates the config while
//! drawing: it emits `Edit`s, and `apply` runs them after the frame.
use sr_core::backend::{AppInfo, EngineStatus, HwDevice};
use sr_core::config::{
    Config, MAX_CH, MAX_DB, Monitor, MonitorWire, Source, SourceKind, VirtualDevice, Wire,
};

/// Lowest volume the GUI writes (0 % on the slider).
pub const MIN_DB: f32 = -80.0;

/// A connection endpoint in the graph. Indices refer to `VirtualDevice::{sources, monitors}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Port {
    /// Output port of a source channel.
    Src { src: usize, ch: u16 },
    /// Input port of a virtual-device channel.
    OutIn { ch: u16 },
    /// Output port of a virtual-device channel.
    OutOut { ch: u16 },
    /// Input port of a monitor channel.
    Mon { mon: usize, ch: u16 },
}

impl Port {
    pub fn is_output(self) -> bool {
        matches!(self, Port::Src { .. } | Port::OutOut { .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub from: Port,
    pub to: Port,
}

pub fn links(dev: &VirtualDevice) -> Vec<Link> {
    let mut out = Vec::new();
    for w in &dev.wires {
        if let Some(src) = dev.sources.iter().position(|s| s.id == w.source) {
            out.push(Link {
                from: Port::Src { src, ch: w.src_ch },
                to: Port::OutIn { ch: w.out_ch },
            });
        }
    }
    for (mon, m) in dev.monitors.iter().enumerate() {
        for w in &m.wires {
            out.push(Link {
                from: Port::OutOut { ch: w.out_ch },
                to: Port::Mon { mon, ch: w.dst_ch },
            });
        }
    }
    out
}

/// Slider percent (0..=100) <-> dB.
pub fn pct_to_db(pct: f32) -> f32 {
    if pct <= 0.0 {
        MIN_DB
    } else {
        (20.0 * (pct / 100.0).log10()).max(MIN_DB)
    }
}

pub fn db_to_pct(db: f32) -> f32 {
    if db <= MIN_DB {
        0.0
    } else {
        100.0 * 10f32.powf(db / 20.0)
    }
}

/// Extra delay the engine adds for a buffer size: two buffers (input and output side) at 48 kHz.
/// shortcut: assumes 48 kHz, read the live rate if users mind.
pub fn delay_ms(frames: u32) -> f64 {
    2.0 * f64::from(frames) / 48.0
}

/// Plain name of a buffer size in the footer and menu (`Config::buffer_frames`).
pub fn delay_name(frames: u32) -> &'static str {
    match frames {
        64 => "Lowest",
        128 => "Low",
        256 => "Balanced",
        512 => "Safest",
        _ => "Custom",
    }
}

/// Footer chip label, e.g. "Audio delay: Balanced".
pub fn delay_label(frames: u32) -> String {
    format!("Audio delay: {}", delay_name(frames))
}

/// One line under a menu option's name; empty when the name says enough.
pub fn delay_hint(frames: u32) -> &'static str {
    match frames {
        64 => "Uses the most CPU",
        256 => "Recommended",
        512 => "Best for recording and streaming, where delay does not matter",
        _ => "",
    }
}

/// A menu row as a screen reader reads it: "Balanced, +10.7 ms. Recommended".
pub fn delay_option(frames: u32) -> String {
    let hint = delay_hint(frames);
    let sep = if hint.is_empty() { "" } else { ". " };
    format!(
        "{}, +{:.1} ms{sep}{hint}",
        delay_name(frames),
        delay_ms(frames)
    )
}

/// What the engine's IOProc last measured through device `id`'s main path (mic, pass-thru), in ms;
/// `None` until it has run.
pub fn device_delay_ms(status: &EngineStatus, id: &str) -> Option<f64> {
    live_ms(&status.delays, id)
}

/// The path of apps captured into device `id`, in ms; `None` without a running app tap.
pub fn device_tap_ms(status: &EngineStatus, id: &str) -> Option<f64> {
    live_ms(&status.tap_delays, id)
}

fn live_ms(list: &[(String, sr_core::backend::Delay)], id: &str) -> Option<f64> {
    list.iter().find(|(d, _)| d == id).map(|(_, d)| d.ms())
}

/// Quiet readout in a device row, e.g. "Delay 5.3 ms".
pub fn delay_readout(ms: f64) -> String {
    format!("Delay {ms:.1} ms")
}

/// The muted second part of the readout while apps are captured, e.g. "Apps 29 ms".
pub fn apps_readout(ms: f64) -> String {
    format!("Apps {ms:.0} ms")
}

/// Tooltip of the readout; the second sentence only while apps are captured.
pub fn delay_readout_tip(apps_ms: Option<f64>) -> String {
    let tip = "Time audio takes to pass through this device.";
    match apps_ms {
        Some(ms) => format!("{tip} Apps you capture take about {ms:.0} ms."),
        None => tip.into(),
    }
}

/// Footer chip tooltip: what SoundRouter adds, from the live reading when there is one, else the
/// two-buffer formula at 48 kHz. `live` is the device's name and its measured delay in ms.
pub fn delay_chip_tip(frames: u32, live: Option<(&str, f64)>) -> String {
    match live {
        Some((name, ms)) => {
            format!("Added by SoundRouter: {ms:.1} ms, measured on {name}. Click to change.")
        }
        None => format!(
            "Added by SoundRouter: about {:.1} ms (two buffers at 48 kHz). Click to change.",
            delay_ms(frames)
        ),
    }
}

/// Shown in the delay menu while Low Power Mode is on and the chosen delay is a small one.
pub const LOW_POWER_HINT: &str =
    "Low Power Mode is on. Lower delay can crackle; choose Balanced or Safest.";

/// Whether to show `LOW_POWER_HINT`: Lowest (64) and Low (128) are the settings that glitch first.
pub fn low_power_hint_due(low_power_mode: bool, frames: u32) -> bool {
    low_power_mode && frames <= 128
}

#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    NewDevice,
    /// Engine IO buffer size in frames (global, `Config::buffer_frames`).
    BufferFrames(u32),
    RemoveDevice(usize),
    Rename(usize, String),
    Enable(usize, bool),
    Volume(usize, f32),
    Mute(usize, bool),
    /// Add (+) or remove (-) one channel pair.
    GrowChannels(usize),
    ShrinkChannels(usize),
    AddSource(usize, SourceKind, u16),
    RemoveSource(usize, usize),
    SourceOn(usize, usize, bool),
    SourceVolume(usize, usize, f32),
    /// App sources: silence the app's own output while it is tapped.
    SourceMuteOriginal(usize, usize, bool),
    AddMonitor(usize, String, u16),
    RemoveMonitor(usize, usize),
    MonitorOn(usize, usize, bool),
    MonitorVolume(usize, usize, f32),
    Connect(usize, Link),
    Unlink(usize, Link),
    /// Remove every wire ending at this input port.
    Disconnect(usize, Port),
    /// Trim of one cable in dB (`Wire::gain_db`); the engine checks the range, as for a volume.
    WireGain(usize, Link, f32),
    /// Flip the polarity of one cable.
    WireInvert(usize, Link, bool),
    /// Pan of a mono L/R cable pair, -1 (left) ..= 1 (right): rewrites both cables' gains. Either
    /// cable of the pair names it. Not stored: `pan_of` reads it back from the two gains.
    WirePan(usize, Link, f32),
    /// Trim every cable into one output channel so they sum back to unity (`balance_db`).
    BalanceCables(usize, u16),
}

pub fn new_device(cfg: &mut Config) {
    let n = (1..)
        .find(|n| !cfg.devices.iter().any(|d| d.id == format!("v{n}")))
        .unwrap_or(1);
    // Numbered like its id, unless a renamed device already wears that name.
    let name = (n..)
        .map(|n| {
            if n == 1 {
                "Loopback Audio".to_string()
            } else {
                format!("Loopback Audio {n}")
            }
        })
        .find(|name| !cfg.devices.iter().any(|d| d.name.trim() == name))
        .unwrap_or_default();
    cfg.devices.push(VirtualDevice {
        id: format!("v{n}"),
        name,
        enabled: true,
        channels: 2,
        volume_db: 0.0,
        muted: false,
        sources: Vec::new(),
        wires: Vec::new(),
        monitors: Vec::new(),
        nodes: Vec::new(),
    });
    // Loopback-style: a new device starts as a plain loopback (1->1, 2->2), so apps playing into
    // it reach its input with no setup. The Pass-Thru source is only the device's own output.
    let last = cfg.devices.len() - 1;
    apply(cfg, Edit::AddSource(last, SourceKind::PassThru, 0));
}

pub fn default_config() -> Config {
    let mut cfg = Config::default();
    new_device(&mut cfg);
    cfg
}

/// `default_config` with its device stripped to no sources and no wires, a blank canvas.
#[cfg(test)]
pub fn bare_config() -> Config {
    let mut cfg = default_config();
    cfg.devices[0].sources.clear();
    cfg.devices[0].wires.clear();
    cfg
}

fn set_channels(dev: &mut VirtualDevice, n: u16) {
    dev.channels = n;
    for s in &mut dev.sources {
        if s.kind == SourceKind::PassThru {
            s.channels = n;
        }
    }
    let sources = &dev.sources;
    dev.wires.retain(|w| {
        w.out_ch < n
            && sources
                .iter()
                .any(|s| s.id == w.source && w.src_ch < s.channels)
    });
    for m in &mut dev.monitors {
        m.wires.retain(|w| w.out_ch < n);
    }
}

/// Default wiring for a new source: channel i -> output i, mono -> both of a pair.
fn auto_wire_source(dev: &mut VirtualDevice, id: &str, ch: u16) {
    for i in 0..ch.min(dev.channels) {
        dev.wires.push(Wire::new(id.to_string(), i, i));
    }
    if ch == 1 && dev.channels >= 2 {
        dev.wires.push(Wire::new(id.to_string(), 0, 1));
    }
}

fn next_source_id(dev: &VirtualDevice) -> String {
    let n = (1..)
        .find(|n| !dev.sources.iter().any(|s| s.id == format!("s{n}")))
        .unwrap_or(1);
    format!("s{n}")
}

pub fn apply(cfg: &mut Config, edit: Edit) {
    match edit {
        Edit::NewDevice => {
            new_device(cfg);
            return;
        }
        Edit::BufferFrames(n) => {
            cfg.buffer_frames = n;
            return;
        }
        _ => {}
    }
    let d = match &edit {
        Edit::NewDevice | Edit::BufferFrames(_) => unreachable!(),
        Edit::RemoveDevice(d)
        | Edit::Rename(d, _)
        | Edit::Enable(d, _)
        | Edit::Volume(d, _)
        | Edit::Mute(d, _)
        | Edit::GrowChannels(d)
        | Edit::ShrinkChannels(d)
        | Edit::AddSource(d, ..)
        | Edit::RemoveSource(d, _)
        | Edit::SourceOn(d, ..)
        | Edit::SourceVolume(d, ..)
        | Edit::SourceMuteOriginal(d, ..)
        | Edit::AddMonitor(d, ..)
        | Edit::RemoveMonitor(d, _)
        | Edit::MonitorOn(d, ..)
        | Edit::MonitorVolume(d, ..)
        | Edit::Connect(d, _)
        | Edit::Unlink(d, _)
        | Edit::Disconnect(d, _)
        | Edit::WireGain(d, ..)
        | Edit::WireInvert(d, ..)
        | Edit::WirePan(d, ..)
        | Edit::BalanceCables(d, _) => *d,
    };
    if let Edit::RemoveDevice(_) = edit {
        if d < cfg.devices.len() {
            cfg.devices.remove(d);
        }
        return;
    }
    let Some(dev) = cfg.devices.get_mut(d) else {
        return;
    };
    match edit {
        Edit::NewDevice | Edit::BufferFrames(_) | Edit::RemoveDevice(_) => {}
        Edit::Rename(_, name) => dev.name = name,
        Edit::Enable(_, on) => dev.enabled = on,
        Edit::Volume(_, db) => dev.volume_db = db,
        Edit::Mute(_, m) => dev.muted = m,
        Edit::GrowChannels(_) => {
            if usize::from(dev.channels) + 2 <= MAX_CH {
                set_channels(dev, dev.channels + 2);
            }
        }
        Edit::ShrinkChannels(_) => {
            if dev.channels > 2 {
                set_channels(dev, dev.channels - 2);
            }
        }
        Edit::AddSource(_, kind, channels) => {
            if dev.sources.iter().any(|s| s.kind == kind) {
                return;
            }
            let channels = if kind == SourceKind::PassThru {
                dev.channels
            } else {
                channels.max(1)
            };
            let id = next_source_id(dev);
            dev.sources.push(Source {
                id: id.clone(),
                kind,
                channels,
                volume_db: 0.0,
                muted: false,
                mute_original: false,
                name: String::new(),
            });
            auto_wire_source(dev, &id, channels);
        }
        Edit::RemoveSource(_, s) => {
            if s < dev.sources.len() {
                let id = dev.sources.remove(s).id;
                dev.wires.retain(|w| w.source != id);
            }
        }
        Edit::SourceOn(_, s, on) => {
            if let Some(s) = dev.sources.get_mut(s) {
                s.muted = !on;
            }
        }
        Edit::SourceVolume(_, s, db) => {
            if let Some(s) = dev.sources.get_mut(s) {
                s.volume_db = db;
            }
        }
        Edit::SourceMuteOriginal(_, s, on) => {
            if let Some(s) = dev.sources.get_mut(s) {
                s.mute_original = on && matches!(s.kind, SourceKind::App { .. });
            }
        }
        Edit::AddMonitor(_, uid, hw_out) => {
            if dev.monitors.iter().any(|m| m.device_uid == uid) {
                return;
            }
            let mut wires: Vec<MonitorWire> = (0..dev.channels.min(hw_out))
                .map(|i| MonitorWire {
                    out_ch: i,
                    dst_ch: i,
                })
                .collect();
            if hw_out == 1 && dev.channels >= 2 {
                wires.push(MonitorWire {
                    out_ch: 1,
                    dst_ch: 0,
                });
            }
            dev.monitors.push(Monitor {
                device_uid: uid,
                enabled: true,
                volume_db: 0.0,
                wires,
                name: String::new(),
            });
        }
        Edit::RemoveMonitor(_, m) => {
            if m < dev.monitors.len() {
                dev.monitors.remove(m);
            }
        }
        Edit::MonitorOn(_, m, on) => {
            if let Some(m) = dev.monitors.get_mut(m) {
                m.enabled = on;
            }
        }
        Edit::MonitorVolume(_, m, db) => {
            if let Some(m) = dev.monitors.get_mut(m) {
                m.volume_db = db;
            }
        }
        Edit::WireGain(_, l, db) => {
            if let Some(w) = wire_mut(dev, l) {
                w.gain_db = db;
            }
        }
        Edit::WireInvert(_, l, on) => {
            if let Some(w) = wire_mut(dev, l) {
                w.invert = on;
            }
        }
        Edit::WirePan(_, l, pan) => {
            if let Some([left, right]) = pan_pair(dev, l) {
                let (gl, gr) = pan_gains(pair_level_db(&dev.wires[left], &dev.wires[right]), pan);
                dev.wires[left].gain_db = gl;
                dev.wires[right].gain_db = gr;
            }
        }
        Edit::BalanceCables(_, ch) => {
            let n = fan_in(dev, ch);
            if n >= 2 {
                for i in wires_into(dev, ch) {
                    dev.wires[i].gain_db = balance_db(n);
                }
            }
        }
        Edit::Connect(_, Link { from, to }) => connect(dev, from, to),
        Edit::Unlink(_, link) => unlink(dev, link),
        Edit::Disconnect(_, port) => match port {
            Port::OutIn { ch } => dev.wires.retain(|w| w.out_ch != ch),
            Port::Mon { mon, ch } => {
                if let Some(m) = dev.monitors.get_mut(mon) {
                    m.wires.retain(|w| w.dst_ch != ch);
                }
            }
            _ => {}
        },
    }
}

fn connect(dev: &mut VirtualDevice, from: Port, to: Port) {
    match (from, to) {
        (Port::Src { src, ch }, Port::OutIn { ch: out_ch }) => {
            let Some(s) = dev.sources.get(src) else {
                return;
            };
            if ch >= s.channels || out_ch >= dev.channels {
                return;
            }
            let same = |w: &Wire| {
                w.to.is_none() && w.source == s.id && w.src_ch == ch && w.out_ch == out_ch
            };
            // A cable keeps its trim when it is plugged in again: match by ends, not by value.
            if !dev.wires.iter().any(same) {
                dev.wires.push(Wire::new(s.id.clone(), ch, out_ch));
            }
        }
        (Port::OutOut { ch: out_ch }, Port::Mon { mon, ch }) => {
            if out_ch >= dev.channels {
                return;
            }
            let Some(m) = dev.monitors.get_mut(mon) else {
                return;
            };
            let w = MonitorWire { out_ch, dst_ch: ch };
            if !m.wires.contains(&w) {
                m.wires.push(w);
            }
        }
        _ => {}
    }
}

fn unlink(dev: &mut VirtualDevice, Link { from, to }: Link) {
    match (from, to) {
        (Port::Src { src, ch }, Port::OutIn { ch: out_ch }) => {
            if let Some(id) = dev.sources.get(src).map(|s| s.id.clone()) {
                dev.wires
                    .retain(|w| !(w.source == id && w.src_ch == ch && w.out_ch == out_ch));
            }
        }
        (Port::OutOut { ch: out_ch }, Port::Mon { mon, ch }) => {
            if let Some(m) = dev.monitors.get_mut(mon) {
                m.wires.retain(|w| !(w.out_ch == out_ch && w.dst_ch == ch));
            }
        }
        _ => {}
    }
}

/// The cable of `dev` that `l` draws, if it is a source cable.
pub fn wire_of(dev: &VirtualDevice, l: Link) -> Option<&Wire> {
    wire_index(dev, l).map(|i| &dev.wires[i])
}

fn wire_index(dev: &VirtualDevice, l: Link) -> Option<usize> {
    let (Port::Src { src, ch }, Port::OutIn { ch: out_ch }) = (l.from, l.to) else {
        return None;
    };
    let id = &dev.sources.get(src)?.id;
    dev.wires
        .iter()
        .position(|w| w.to.is_none() && &w.source == id && w.src_ch == ch && w.out_ch == out_ch)
}

fn wire_mut(dev: &mut VirtualDevice, l: Link) -> Option<&mut Wire> {
    wire_index(dev, l).map(|i| &mut dev.wires[i])
}

/// Indices of the cables that end at output channel `ch` (their source exists).
fn wires_into(dev: &VirtualDevice, ch: u16) -> Vec<usize> {
    (0..dev.wires.len())
        .filter(|&i| {
            dev.wires[i].to.is_none()
                && dev.wires[i].out_ch == ch
                && dev.sources.iter().any(|s| s.id == dev.wires[i].source)
        })
        .collect()
}

/// How many cables feed output channel `ch`: two or more add up.
pub fn fan_in(dev: &VirtualDevice, ch: u16) -> usize {
    wires_into(dev, ch).len()
}

/// Trim per cable that brings `n` equal cables back to the level of one: `-20 log10 n`, to 0.1 dB
/// (two cables: -6.0).
pub fn balance_db(n: usize) -> f32 {
    (-20.0 * (n.max(1) as f32).log10() * 10.0).round() / 10.0
}

/// Whether every cable into `ch` already carries `balance_db` (nothing left to balance).
pub fn balanced(dev: &VirtualDevice, ch: u16) -> bool {
    let want = balance_db(fan_in(dev, ch));
    wires_into(dev, ch)
        .iter()
        .all(|&i| (dev.wires[i].gain_db - want).abs() < 0.05)
}

/// Lowest gain a pan writes to the far side at the hard stops: finite, as a wire's gain must be,
/// and far below anything audible.
const PAN_FLOOR_DB: f32 = -80.0;

/// Indices `[left, right]` of the two cables that carry one source channel to both channels of an
/// output pair, when `l` is one of them: the cables a pan control moves.
pub fn pan_pair(dev: &VirtualDevice, l: Link) -> Option<[usize; 2]> {
    let i = wire_index(dev, l)?;
    let w = &dev.wires[i];
    let other = dev
        .wires
        .iter()
        .position(|o| o.source == w.source && o.src_ch == w.src_ch && o.out_ch == w.out_ch ^ 1)?;
    Some(if w.out_ch.is_multiple_of(2) {
        [i, other]
    } else {
        [other, i]
    })
}

/// Combined level of a left/right pair in dB power (two equal cables at 0 dB: +3.01).
fn pair_level_db(left: &Wire, right: &Wire) -> f32 {
    let p = |w: &Wire| 10f32.powf(w.gain_db / 10.0);
    10.0 * (p(left) + p(right)).log10()
}

/// Constant-power pair for `pan` in -1..=1 at `level` dB: `cos t` and `sin t` with
/// `t = (pan + 1) pi/4`, so centre is level - 3.01 dB on each side and the power never moves.
fn pan_gains(level: f32, pan: f32) -> (f32, f32) {
    let t = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    let db = |x: f32| (level + 20.0 * x.max(1e-9).log10()).clamp(PAN_FLOOR_DB, MAX_DB);
    (db(t.cos()), db(t.sin()))
}

/// Pan of the cable pair `l` belongs to, -1 (left) ..= 1 (right), read from the two gains.
pub fn pan_of(dev: &VirtualDevice, l: Link) -> Option<f32> {
    let [left, right] = pan_pair(dev, l)?;
    let lin = |i: usize| 10f32.powf(dev.wires[i].gain_db / 20.0);
    let t = lin(right).atan2(lin(left));
    Some((t / std::f32::consts::FRAC_PI_4 - 1.0).clamp(-1.0, 1.0))
}

/// Valid drop target for a drag that started at `from`.
pub fn can_link(from: Port, to: Port) -> bool {
    matches!(
        (from, to),
        (Port::Src { .. }, Port::OutIn { .. }) | (Port::OutOut { .. }, Port::Mon { .. })
    )
}

/// Channel row label: `1 (L)` / `2 (R)` for a stereo pair, the number otherwise.
/// `total == 0` means an output-card row, which is always one half of a pair.
pub fn ch_label(ch: u16, total: u16) -> String {
    match (total, ch % 2) {
        (0 | 2, side) => format!("{} ({})", ch + 1, if side == 0 { "L" } else { "R" }),
        _ => (ch + 1).to_string(),
    }
}

/// Copy the names of the apps and devices that are present onto the sources and monitors using
/// them, so one that is gone later is still shown by name. Returns whether anything changed.
pub fn learn_names(
    cfg: &mut Config,
    hw: &[HwDevice],
    apps: &[AppInfo],
    installed: &[AppInfo],
) -> bool {
    let hw_name = |uid: &str| hw.iter().find(|h| h.uid == uid).map(|h| h.name.as_str());
    let mut changed = false;
    let mut learn = |name: &mut String, seen: Option<&str>| {
        if let Some(seen) = seen.filter(|s| !s.is_empty() && *s != name.as_str()) {
            *name = seen.to_string();
            changed = true;
        }
    };
    for d in &mut cfg.devices {
        for s in &mut d.sources {
            let seen = match &s.kind {
                SourceKind::App { bundle_id } => apps
                    .iter()
                    .chain(installed)
                    .find(|a| &a.bundle_id == bundle_id)
                    .map(|a| a.name.as_str()),
                SourceKind::Device { uid } => hw_name(uid),
                SourceKind::PassThru => None,
            };
            learn(&mut s.name, seen);
        }
        for m in &mut d.monitors {
            learn(&mut m.name, hw_name(&m.device_uid));
        }
    }
    changed
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn cfg_with_dev() -> Config {
        let mut c = bare_config();
        apply(
            &mut c,
            Edit::AddSource(
                0,
                SourceKind::App {
                    bundle_id: "com.apple.Safari".into(),
                },
                2,
            ),
        );
        c
    }

    #[test]
    fn default_is_one_stereo_device() {
        let c = default_config();
        assert_eq!(c.devices.len(), 1);
        assert_eq!(c.devices[0].id, "v1");
        assert_eq!(c.devices[0].channels, 2);
        c.validate().unwrap();
    }

    #[test]
    fn buffer_frames_edit_and_label() {
        let mut c = default_config();
        assert_eq!(delay_label(c.buffer_frames), "Audio delay: Balanced");
        apply(&mut c, Edit::BufferFrames(128));
        assert_eq!(c.buffer_frames, 128);
        c.validate().unwrap();
        assert_eq!(delay_label(128), "Audio delay: Low");
        assert_eq!(delay_option(64), "Lowest, +2.7 ms. Uses the most CPU");
        assert_eq!(delay_option(128), "Low, +5.3 ms");
        assert_eq!(delay_option(256), "Balanced, +10.7 ms. Recommended");
        assert_eq!(
            delay_option(512),
            "Safest, +21.3 ms. Best for recording and streaming, where delay does not matter"
        );
    }

    #[test]
    fn live_delay_readouts_and_hints() {
        use sr_core::backend::Delay;
        let status = EngineStatus {
            delays: vec![(
                "v1".into(),
                Delay {
                    frames: 256,
                    rate_hz: 48_000,
                },
            )],
            tap_delays: vec![(
                "v1".into(),
                Delay {
                    frames: 1384,
                    rate_hz: 48_000,
                },
            )],
            ..EngineStatus::default()
        };
        let ms = device_delay_ms(&status, "v1").unwrap();
        assert_eq!(delay_readout(ms), "Delay 5.3 ms");
        assert_eq!(device_delay_ms(&status, "v2"), None);
        let apps = device_tap_ms(&status, "v1").unwrap();
        assert_eq!(apps_readout(apps), "Apps 29 ms");
        assert_eq!(device_tap_ms(&status, "v2"), None);
        assert_eq!(
            delay_chip_tip(128, Some(("Loopback Audio", ms))),
            "Added by SoundRouter: 5.3 ms, measured on Loopback Audio. Click to change."
        );
        assert_eq!(
            delay_chip_tip(256, None),
            "Added by SoundRouter: about 10.7 ms (two buffers at 48 kHz). Click to change."
        );
        assert!(low_power_hint_due(true, 64) && low_power_hint_due(true, 128));
        assert!(!low_power_hint_due(true, 256) && !low_power_hint_due(false, 64));
        assert_eq!(
            delay_readout_tip(None),
            "Time audio takes to pass through this device."
        );
        assert_eq!(
            delay_readout_tip(Some(28.8)),
            "Time audio takes to pass through this device. Apps you capture take about 29 ms."
        );
    }

    #[test]
    fn names_are_learned_while_present_and_kept_when_gone() {
        let mut c = cfg_with_dev();
        let mic = SourceKind::Device { uid: "mic".into() };
        apply(&mut c, Edit::AddSource(0, mic, 1));
        apply(&mut c, Edit::AddMonitor(0, "spk".into(), 2));
        let hw = |uid: &str, name: &str| HwDevice {
            uid: uid.into(),
            name: name.into(),
            in_ch: 1,
            out_ch: 2,
        };
        let safari = AppInfo {
            bundle_id: "com.apple.Safari".into(),
            name: "Safari".into(),
            running_output: false,
            regular: true,
        };
        let present = [hw("mic", "USB Mic"), hw("spk", "Speakers")];
        assert!(learn_names(&mut c, &present, &[], &[safari]));
        let d = &c.devices[0];
        let names: Vec<&str> = d.sources.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Safari", "USB Mic"]);
        assert_eq!(d.monitors[0].name, "Speakers");
        assert!(!learn_names(&mut c, &present, &[], &[]), "nothing new");
        // unplugged: the last name stays; a renamed device updates it
        let before = c.clone();
        assert!(!learn_names(&mut c, &[], &[], &[]));
        assert_eq!(c, before);
        assert!(learn_names(&mut c, &[hw("spk", "Desk Speakers")], &[], &[]));
        assert_eq!(c.devices[0].monitors[0].name, "Desk Speakers");
        c.validate().unwrap();
    }

    #[test]
    fn new_device_starts_as_unit_loopback() {
        let mut c = default_config();
        apply(&mut c, Edit::NewDevice);
        for d in &c.devices {
            assert_eq!(d.sources.len(), 1);
            assert_eq!(d.sources[0].kind, SourceKind::PassThru);
            assert_eq!(d.sources[0].channels, 2);
            let w: Vec<_> = d.wires.iter().map(|w| (w.src_ch, w.out_ch)).collect();
            assert_eq!(w, [(0, 0), (1, 1)]);
            assert!(d.monitors.is_empty());
        }
        c.validate().unwrap();
    }

    #[test]
    fn new_device_skips_names_a_renamed_device_already_uses() {
        let mut c = default_config();
        apply(&mut c, Edit::Rename(0, "Loopback Audio 2".into()));
        apply(&mut c, Edit::NewDevice);
        apply(&mut c, Edit::NewDevice);
        let names: Vec<_> = c.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["Loopback Audio 2", "Loopback Audio 3", "Loopback Audio 4"]
        );
        c.validate().unwrap();
    }

    #[test]
    fn new_device_ids_are_unique() {
        let mut c = default_config();
        apply(&mut c, Edit::NewDevice);
        apply(&mut c, Edit::NewDevice);
        apply(&mut c, Edit::RemoveDevice(1));
        apply(&mut c, Edit::NewDevice);
        let ids: Vec<_> = c.devices.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["v1", "v3", "v2"]);
        c.validate().unwrap();
    }

    #[test]
    fn mute_original_applies_to_app_sources_only() {
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::AddSource(0, SourceKind::PassThru, 2));
        let (app, pt) = (
            c.devices[0].sources.len() - 2,
            c.devices[0].sources.len() - 1,
        );
        assert!(matches!(
            c.devices[0].sources[app].kind,
            SourceKind::App { .. }
        ));
        apply(&mut c, Edit::SourceMuteOriginal(0, app, true));
        apply(&mut c, Edit::SourceMuteOriginal(0, pt, true));
        assert!(c.devices[0].sources[app].mute_original);
        assert!(!c.devices[0].sources[pt].mute_original);
        apply(&mut c, Edit::SourceMuteOriginal(0, app, false));
        assert!(!c.devices[0].sources[app].mute_original);
        c.validate().unwrap();
    }

    #[test]
    fn add_source_auto_wires_and_dedups() {
        let mut c = cfg_with_dev();
        apply(
            &mut c,
            Edit::AddSource(
                0,
                SourceKind::App {
                    bundle_id: "com.apple.Safari".into(),
                },
                2,
            ),
        );
        let d = &c.devices[0];
        assert_eq!(d.sources.len(), 1);
        assert_eq!(d.wires.len(), 2);
        c.validate().unwrap();
        apply(
            &mut c,
            Edit::AddSource(0, SourceKind::Device { uid: "mic".into() }, 1),
        );
        // mono source feeds both output channels
        assert_eq!(
            c.devices[0]
                .wires
                .iter()
                .filter(|w| w.source == "s2")
                .count(),
            2
        );
        c.validate().unwrap();
    }

    #[test]
    fn connect_and_unlink_wires() {
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::Disconnect(0, Port::OutIn { ch: 1 }));
        assert_eq!(c.devices[0].wires.len(), 1);
        let l = Link {
            from: Port::Src { src: 0, ch: 0 },
            to: Port::OutIn { ch: 1 },
        };
        apply(&mut c, Edit::Connect(0, l));
        apply(&mut c, Edit::Connect(0, l)); // duplicate ignored
        assert_eq!(c.devices[0].wires.len(), 2);
        assert!(links(&c.devices[0]).contains(&l));
        apply(&mut c, Edit::Unlink(0, l));
        assert_eq!(c.devices[0].wires.len(), 1);
        c.validate().unwrap();
    }

    #[test]
    fn connect_rejects_bad_ports() {
        let mut c = cfg_with_dev();
        let n = c.devices[0].wires.len();
        apply(
            &mut c,
            Edit::Connect(
                0,
                Link {
                    from: Port::Src { src: 0, ch: 5 },
                    to: Port::OutIn { ch: 0 },
                },
            ),
        );
        apply(
            &mut c,
            Edit::Connect(
                0,
                Link {
                    from: Port::Src { src: 0, ch: 0 },
                    to: Port::OutIn { ch: 9 },
                },
            ),
        );
        apply(
            &mut c,
            Edit::Connect(
                0,
                Link {
                    from: Port::Src { src: 0, ch: 0 },
                    to: Port::Mon { mon: 0, ch: 0 },
                },
            ),
        );
        assert_eq!(c.devices[0].wires.len(), n);
    }

    #[test]
    fn remove_source_drops_its_wires() {
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::RemoveSource(0, 0));
        assert!(c.devices[0].sources.is_empty());
        assert!(c.devices[0].wires.is_empty());
        c.validate().unwrap();
    }

    fn src_link(src: usize, ch: u16, out: u16) -> Link {
        Link {
            from: Port::Src { src, ch },
            to: Port::OutIn { ch: out },
        }
    }

    /// One mono source wired to both sides of the pair, as `AddSource` does for a microphone.
    fn mono_pair() -> Config {
        let mut c = bare_config();
        let mic = SourceKind::Device { uid: "mic".into() };
        apply(&mut c, Edit::AddSource(0, mic, 1));
        assert_eq!(c.devices[0].wires.len(), 2);
        c
    }

    #[test]
    fn wire_gain_and_invert_edit_one_cable() {
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::WireGain(0, src_link(0, 1, 1), -6.0));
        apply(&mut c, Edit::WireInvert(0, src_link(0, 0, 0), true));
        let w = &c.devices[0].wires;
        assert_eq!((w[0].gain_db, w[0].invert), (0.0, true));
        assert_eq!((w[1].gain_db, w[1].invert), (-6.0, false));
        assert_eq!(wire_of(&c.devices[0], src_link(0, 1, 1)), Some(&w[1]));
        c.validate().unwrap();
        // A cable that is not there, or not a source cable, changes nothing.
        let before = c.clone();
        apply(&mut c, Edit::WireGain(0, src_link(0, 0, 1), -3.0));
        apply(&mut c, Edit::WireGain(5, src_link(0, 0, 0), -3.0));
        apply(
            &mut c,
            Edit::WireInvert(
                0,
                Link {
                    from: Port::OutOut { ch: 0 },
                    to: Port::Mon { mon: 0, ch: 0 },
                },
                true,
            ),
        );
        assert_eq!(c, before);
    }

    #[test]
    fn a_wire_gain_out_of_range_fails_validation() {
        for bad in [24.5, 100.0, f32::NAN, f32::INFINITY] {
            let mut c = cfg_with_dev();
            apply(&mut c, Edit::WireGain(0, src_link(0, 0, 0), bad));
            let e = c.validate().unwrap_err();
            assert!(e.contains("must be finite and <= 24"), "{bad}: {e}");
        }
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::WireGain(0, src_link(0, 0, 0), 24.0));
        apply(&mut c, Edit::WireGain(0, src_link(0, 1, 1), -200.0));
        c.validate().unwrap();
    }

    #[test]
    fn a_trim_survives_unplugging_and_replugging_nothing_else() {
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::WireGain(0, src_link(0, 0, 0), -6.0));
        // Connecting a cable that exists keeps its trim and adds no twin.
        apply(&mut c, Edit::Connect(0, src_link(0, 0, 0)));
        assert_eq!(c.devices[0].wires.len(), 2);
        assert_eq!(c.devices[0].wires[0].gain_db, -6.0);
        apply(&mut c, Edit::Unlink(0, src_link(0, 0, 0)));
        apply(&mut c, Edit::Connect(0, src_link(0, 0, 0)));
        assert_eq!(c.devices[0].wires.last().unwrap().gain_db, 0.0);
    }

    #[test]
    fn a_trim_writes_version_2_and_clearing_it_goes_back() {
        let mut c = cfg_with_dev();
        let json = |c: &Config| serde_json::to_string(c).unwrap();
        assert!(json(&c).starts_with(r#"{"version":1,"#));
        apply(&mut c, Edit::WireInvert(0, src_link(0, 0, 0), true));
        assert!(json(&c).starts_with(r#"{"version":2,"#));
        apply(&mut c, Edit::WireInvert(0, src_link(0, 0, 0), false));
        assert!(json(&c).starts_with(r#"{"version":1,"#));
    }

    #[test]
    fn pan_writes_two_constant_power_cables_and_reads_back() {
        let mut c = mono_pair();
        let (l, r) = (src_link(0, 0, 0), src_link(0, 0, 1));
        let dev = |c: &Config| c.devices[0].clone();
        // Two cables at 0 dB are the centre; reading it changes nothing.
        assert_eq!(pan_of(&dev(&c), l), Some(0.0));
        assert_eq!(pan_of(&dev(&c), r), Some(0.0));
        apply(&mut c, Edit::WirePan(0, r, 0.0));
        assert!(c.devices[0].wires.iter().all(|w| w.gain_db.abs() < 1e-4));
        for pan in [-1.0f32, -0.5, 0.25, 1.0] {
            apply(&mut c, Edit::WirePan(0, l, pan));
            let w = &c.devices[0].wires;
            let lin = |i: usize| 10f32.powf(w[i].gain_db / 20.0);
            // Constant power: the pair's power is what it was at the centre (+3.01 dB).
            assert!(
                (lin(0).powi(2) + lin(1).powi(2) - 2.0).abs() < 1e-3,
                "{pan}"
            );
            assert!((pan_of(&dev(&c), r).unwrap() - pan).abs() < 1e-3, "{pan}");
            assert_eq!(pan_of(&dev(&c), l), pan_of(&dev(&c), r));
            c.validate().unwrap();
        }
        // Hard right keeps the left cable finite (the engine refuses -inf).
        assert!(c.devices[0].wires[0].gain_db <= -79.0);
        // From unity at 0 dB the unit-level law: each side is cos t / sin t, so centre is -3.01 dB
        // below the pair's own level.
        let mut c = mono_pair();
        c.devices[0].wires[0].gain_db = -3.0103;
        c.devices[0].wires[1].gain_db = -3.0103;
        apply(&mut c, Edit::WirePan(0, l, 0.0));
        assert!((c.devices[0].wires[0].gain_db + 3.0103).abs() < 1e-3);
        assert!(pan_of(&dev(&c), l).unwrap().abs() < 1e-4);
    }

    #[test]
    fn only_a_cable_pair_of_one_source_channel_has_a_pan() {
        let c = cfg_with_dev(); // 0->0 and 1->1: two separate channels, no pair
        assert!(pan_pair(&c.devices[0], src_link(0, 0, 0)).is_none());
        assert_eq!(pan_of(&c.devices[0], src_link(0, 1, 1)), None);
        let mut c = mono_pair();
        apply(&mut c, Edit::Unlink(0, src_link(0, 0, 1)));
        assert!(pan_pair(&c.devices[0], src_link(0, 0, 0)).is_none());
        // A pan edit on a lone cable does nothing.
        let before = c.clone();
        apply(&mut c, Edit::WirePan(0, src_link(0, 0, 0), 0.7));
        assert_eq!(c, before);
    }

    #[test]
    fn balancing_trims_every_cable_into_a_channel() {
        let mut c = default_config();
        apply(&mut c, Edit::AddSource(0, SourceKind::PassThru, 0));
        apply(
            &mut c,
            Edit::AddSource(
                0,
                SourceKind::App {
                    bundle_id: "a.b".into(),
                },
                2,
            ),
        );
        apply(&mut c, Edit::WireInvert(0, src_link(1, 0, 0), true));
        let d = &c.devices[0];
        assert_eq!(fan_in(d, 0), 2);
        assert_eq!(fan_in(d, 1), 2);
        assert!(!balanced(d, 0));
        apply(&mut c, Edit::BalanceCables(0, 0));
        let d = &c.devices[0];
        assert!(balanced(d, 0) && !balanced(d, 1));
        let gains: Vec<_> = d
            .wires
            .iter()
            .map(|w| (w.out_ch, w.gain_db, w.invert))
            .collect();
        assert_eq!(
            gains,
            [
                (0, -6.0, false),
                (1, 0.0, false),
                (0, -6.0, true),
                (1, 0.0, false)
            ]
        );
        c.validate().unwrap();
        // One cable alone, or a channel nothing feeds, is left alone.
        let mut one = cfg_with_dev();
        let before = one.clone();
        apply(&mut one, Edit::BalanceCables(0, 0));
        apply(&mut one, Edit::BalanceCables(0, 1));
        assert_eq!(one, before);
    }

    #[test]
    fn balance_trim_is_minus_twenty_log_n_to_a_tenth() {
        assert_eq!(balance_db(2), -6.0);
        assert_eq!(balance_db(3), -9.5);
        assert_eq!(balance_db(4), -12.0);
        assert_eq!(balance_db(1), 0.0);
        assert_eq!(balance_db(0), 0.0);
    }

    #[test]
    fn monitor_wiring_and_channel_shrink() {
        let mut c = cfg_with_dev();
        apply(&mut c, Edit::GrowChannels(0));
        assert_eq!(c.devices[0].channels, 4);
        apply(&mut c, Edit::AddMonitor(0, "spk".into(), 2));
        apply(
            &mut c,
            Edit::Connect(
                0,
                Link {
                    from: Port::OutOut { ch: 3 },
                    to: Port::Mon { mon: 0, ch: 1 },
                },
            ),
        );
        assert_eq!(c.devices[0].monitors[0].wires.len(), 3);
        apply(&mut c, Edit::ShrinkChannels(0));
        assert_eq!(c.devices[0].channels, 2);
        assert_eq!(c.devices[0].monitors[0].wires.len(), 2);
        c.validate().unwrap();
        apply(&mut c, Edit::ShrinkChannels(0)); // floor is 2
        assert_eq!(c.devices[0].channels, 2);
        apply(&mut c, Edit::RemoveMonitor(0, 0));
        assert!(c.devices[0].monitors.is_empty());
    }

    #[test]
    fn pass_thru_follows_channel_count() {
        let mut c = default_config();
        apply(&mut c, Edit::AddSource(0, SourceKind::PassThru, 0));
        assert_eq!(c.devices[0].sources[0].channels, 2);
        apply(&mut c, Edit::GrowChannels(0));
        assert_eq!(c.devices[0].sources[0].channels, 4);
        c.validate().unwrap();
    }

    #[test]
    fn volume_percent_round_trip() {
        assert_eq!(pct_to_db(100.0), 0.0);
        assert_eq!(pct_to_db(0.0), MIN_DB);
        assert!((pct_to_db(50.0) + 6.0206).abs() < 1e-3);
        assert!((db_to_pct(pct_to_db(37.0)) - 37.0).abs() < 1e-2);
        assert_eq!(db_to_pct(MIN_DB), 0.0);
    }

    #[test]
    fn labels() {
        assert_eq!(ch_label(0, 2), "1 (L)");
        assert_eq!(ch_label(1, 2), "2 (R)");
        assert_eq!(ch_label(2, 0), "3 (L)");
        assert_eq!(ch_label(0, 1), "1");
        assert_eq!(ch_label(3, 4), "4");
    }

    pub(crate) mod prop {
        use super::*;
        use proptest::prelude::*;

        fn port() -> impl Strategy<Value = Port> {
            prop_oneof![
                (0usize..4, 0u16..70).prop_map(|(src, ch)| Port::Src { src, ch }),
                (0u16..70).prop_map(|ch| Port::OutIn { ch }),
                (0u16..70).prop_map(|ch| Port::OutOut { ch }),
                (0usize..4, 0u16..70).prop_map(|(mon, ch)| Port::Mon { mon, ch }),
            ]
        }

        fn kind() -> impl Strategy<Value = SourceKind> {
            prop_oneof![
                Just(SourceKind::PassThru),
                "[a-c]{1,2}".prop_map(|b| SourceKind::App { bundle_id: b }),
                "[a-c]{1,2}".prop_map(|uid| SourceKind::Device { uid }),
            ]
        }

        pub(crate) fn edit() -> impl Strategy<Value = Edit> {
            let d = 0usize..4;
            let i = 0usize..4;
            let db = -100.0f32..12.0;
            prop_oneof![
                Just(Edit::NewDevice),
                d.clone().prop_map(Edit::RemoveDevice),
                (d.clone(), "[ -~]{0,6}").prop_map(|(d, n)| Edit::Rename(d, n)),
                (d.clone(), any::<bool>()).prop_map(|(d, b)| Edit::Enable(d, b)),
                (d.clone(), db.clone()).prop_map(|(d, v)| Edit::Volume(d, v)),
                (d.clone(), any::<bool>()).prop_map(|(d, b)| Edit::Mute(d, b)),
                d.clone().prop_map(Edit::GrowChannels),
                d.clone().prop_map(Edit::ShrinkChannels),
                (d.clone(), kind(), 0u16..70).prop_map(|(d, k, c)| Edit::AddSource(d, k, c)),
                (d.clone(), i.clone()).prop_map(|(d, s)| Edit::RemoveSource(d, s)),
                (d.clone(), i.clone(), any::<bool>()).prop_map(|(d, s, b)| Edit::SourceOn(d, s, b)),
                (d.clone(), i.clone(), db.clone())
                    .prop_map(|(d, s, v)| Edit::SourceVolume(d, s, v)),
                (d.clone(), i.clone(), any::<bool>())
                    .prop_map(|(d, s, b)| Edit::SourceMuteOriginal(d, s, b)),
                (d.clone(), "[a-c]{1,2}", 0u16..70).prop_map(|(d, u, c)| Edit::AddMonitor(d, u, c)),
                (d.clone(), i.clone()).prop_map(|(d, m)| Edit::RemoveMonitor(d, m)),
                (d.clone(), i.clone(), any::<bool>())
                    .prop_map(|(d, m, b)| Edit::MonitorOn(d, m, b)),
                (d.clone(), i.clone(), db).prop_map(|(d, m, v)| Edit::MonitorVolume(d, m, v)),
                (d.clone(), port(), port())
                    .prop_map(|(d, from, to)| Edit::Connect(d, Link { from, to })),
                (d.clone(), port(), port())
                    .prop_map(|(d, from, to)| Edit::Unlink(d, Link { from, to })),
                (d.clone(), port()).prop_map(|(d, p)| Edit::Disconnect(d, p)),
                (d.clone(), port(), port(), -100.0f32..=24.0)
                    .prop_map(|(d, from, to, g)| Edit::WireGain(d, Link { from, to }, g)),
                (d.clone(), port(), port(), any::<bool>())
                    .prop_map(|(d, from, to, b)| Edit::WireInvert(d, Link { from, to }, b)),
                (d.clone(), port(), port(), -2.0f32..2.0)
                    .prop_map(|(d, from, to, p)| Edit::WirePan(d, Link { from, to }, p)),
                (d, 0u16..70).prop_map(|(d, ch)| Edit::BalanceCables(d, ch)),
            ]
        }

        proptest! {
            /// Mirrors `App::commit`: apply, validate, roll back on failure. Whatever the edit
            /// sequence (including out-of-range indices and ports), the config the app keeps
            /// validates, survives a JSON round trip, and only text/size-sensitive edits may be refused.
            #[test]
            fn edit_sequences_keep_config_valid(edits in proptest::collection::vec(edit(), 0..60)) {
                let mut cfg = default_config();
                for e in edits {
                    let prev = cfg.clone();
                    apply(&mut cfg, e.clone());
                    if cfg.validate().is_err() {
                        prop_assert!(
                            matches!(e, Edit::Rename(..) | Edit::AddSource(..) | Edit::AddMonitor(..) | Edit::NewDevice | Edit::Enable(..)),
                            "unexpected invalid result from {e:?}"
                        );
                        cfg = prev;
                    }
                    prop_assert!(cfg.validate().is_ok());
                }
                let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
                prop_assert_eq!(back, cfg);
            }

            #[test]
            fn pct_db_round_trip(pct in 0.5f32..400.0) {
                let back = db_to_pct(pct_to_db(pct));
                prop_assert!((back - pct).abs() <= pct * 1e-3, "{pct} -> {back}");
            }
        }
    }
}
