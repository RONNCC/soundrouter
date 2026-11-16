//! Pure edit operations on the `Config` the GUI owns. The UI never mutates the config while
//! drawing: it emits `Edit`s, and `apply` runs them after the frame.
use sr_core::config::{
    Config, MAX_CH, Monitor, MonitorWire, Source, SourceKind, VirtualDevice, Wire,
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

#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    NewDevice,
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
    AddMonitor(usize, String, u16),
    RemoveMonitor(usize, usize),
    MonitorOn(usize, usize, bool),
    MonitorVolume(usize, usize, f32),
    Connect(usize, Link),
    Unlink(usize, Link),
    /// Remove every wire ending at this input port.
    Disconnect(usize, Port),
}

pub fn new_device(cfg: &mut Config) {
    let n = (1..)
        .find(|n| !cfg.devices.iter().any(|d| d.id == format!("v{n}")))
        .unwrap_or(1);
    let name = if n == 1 {
        "Loopback Audio".to_string()
    } else {
        format!("Loopback Audio {n}")
    };
    cfg.version = 1;
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
    });
}

pub fn default_config() -> Config {
    let mut cfg = Config {
        version: 1,
        devices: Vec::new(),
    };
    new_device(&mut cfg);
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
        dev.wires.push(Wire {
            source: id.to_string(),
            src_ch: i,
            out_ch: i,
        });
    }
    if ch == 1 && dev.channels >= 2 {
        dev.wires.push(Wire {
            source: id.to_string(),
            src_ch: 0,
            out_ch: 1,
        });
    }
}

fn next_source_id(dev: &VirtualDevice) -> String {
    let n = (1..)
        .find(|n| !dev.sources.iter().any(|s| s.id == format!("s{n}")))
        .unwrap_or(1);
    format!("s{n}")
}

pub fn apply(cfg: &mut Config, edit: Edit) {
    if matches!(edit, Edit::NewDevice) {
        new_device(cfg);
        return;
    }
    let d = match &edit {
        Edit::NewDevice => unreachable!(),
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
        | Edit::AddMonitor(d, ..)
        | Edit::RemoveMonitor(d, _)
        | Edit::MonitorOn(d, ..)
        | Edit::MonitorVolume(d, ..)
        | Edit::Connect(d, _)
        | Edit::Unlink(d, _)
        | Edit::Disconnect(d, _) => *d,
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
        Edit::NewDevice | Edit::RemoveDevice(_) => {}
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
            let w = Wire {
                source: s.id.clone(),
                src_ch: ch,
                out_ch,
            };
            if !dev.wires.contains(&w) {
                dev.wires.push(w);
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

/// Valid drop target for a drag that started at `from`.
pub fn can_link(from: Port, to: Port) -> bool {
    matches!(
        (from, to),
        (Port::Src { .. }, Port::OutIn { .. }) | (Port::OutOut { .. }, Port::Mon { .. })
    )
}

/// Channel row label: L/R for a stereo pair, the number otherwise.
pub fn ch_label(ch: u16, total: u16) -> String {
    match (total, ch) {
        (2, 0) => "L".into(),
        (2, 1) => "R".into(),
        _ => (ch + 1).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_dev() -> Config {
        let mut c = default_config();
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
        assert_eq!(ch_label(0, 2), "L");
        assert_eq!(ch_label(1, 2), "R");
        assert_eq!(ch_label(0, 1), "1");
        assert_eq!(ch_label(3, 4), "4");
    }
}
