//! In-app backend for `--demo`: synthetic meters, fake apps and hardware.
use crate::model::{Edit, FxEdit, Module, NewNode, apply, default_config};
use sr_core::backend::{
    AppInfo, Backend, Delay, EngineOptions, EngineStatus, HwDevice, MeterReading, ReadAheadState,
    SelfTest, TccStatus,
};
use sr_core::config::{Config, Fx, SourceKind};
use std::collections::HashMap;
use std::time::Instant;

/// Driver version the fake reports; the GUI treats it as "no real driver to test or uninstall".
pub const DEMO_DRIVER: &str = "demo";

/// Healthy engine: driver present, every permission granted, nothing running.
pub fn demo_status() -> EngineStatus {
    EngineStatus {
        tcc: TccStatus::Granted,
        mic: TccStatus::Granted,
        driver_version: Some(DEMO_DRIVER.into()),
        read_ahead: ReadAheadState::Off,
        ..EngineStatus::default()
    }
}

pub struct FakeBackend {
    cfg: Config,
    start: Instant,
    /// Meter clock pinned to this many seconds, so snapshots are deterministic.
    frozen: Option<f32>,
    /// What `status()` reports; tests drive the GUI through each engine state with it.
    pub status: EngineStatus,
    /// What `self_test` returns; `None` makes it fail.
    pub self_test: Option<SelfTest>,
    /// What `measure_delay` returns, in ms; `None` makes it fail.
    pub measured_ms: Option<f64>,
    /// The last options `set_options` got.
    pub options: EngineOptions,
    /// Meters stay at silence: stands in for the engine when the real one could not start.
    silent: bool,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            cfg: Config::default(),
            start: Instant::now(),
            frozen: None,
            status: demo_status(),
            self_test: Some(SelfTest {
                rms_dbfs: -6.0,
                dropouts: 0,
                rate: 48_000.0,
                passed: true,
            }),
            measured_ms: Some(5.4),
            options: EngineOptions::default(),
            silent: false,
        }
    }

    /// Placeholder for an engine that failed to start: accepts configs, plays and shows nothing.
    pub fn silent() -> Self {
        Self {
            silent: true,
            self_test: None, // never claim audio flows
            measured_ms: None,
            ..Self::new()
        }
    }

    /// Meters pinned to second `t`: same readings every call, for snapshot tests. It reports the
    /// delays `demo_config` would at 256 frames from the first poll, which comes before the apply.
    #[cfg(test)]
    pub fn frozen(t: f32) -> Self {
        let at = |frames| Delay {
            frames,
            rate_hz: 48_000,
        };
        let mut b = Self {
            frozen: Some(t),
            ..Self::new()
        };
        b.status.delays = vec![("v1".into(), at(512)), ("v2".into(), at(512))];
        b.status.tap_delays = vec![("v1".into(), at(1_384))]; // apps captured into v1 take longer
        b
    }
}

/// Bouncy linear peak, different per seed.
fn wave(t: f32, seed: f32) -> f32 {
    let env = 0.55 + 0.45 * (t * 6.1 + seed * 1.7).sin();
    let x = ((0.4 + 0.3 * (t * (1.3 + seed * 0.37) + seed).sin()) * env).clamp(0.0, 1.0);
    10f32.powf(-3.0 * (1.0 - x)) // x is the 0..1 bar position over -60..0 dB
}

impl Backend for FakeBackend {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()> {
        self.cfg = cfg.clone();
        Ok(())
    }

    fn set_options(&mut self, o: &EngineOptions) -> anyhow::Result<()> {
        self.options = o.clone();
        // A driver without read-ahead (status says Unsupported) stays that way.
        if self.status.read_ahead != ReadAheadState::Unsupported {
            self.status.read_ahead = if o.read_ahead {
                ReadAheadState::On
            } else {
                ReadAheadState::Off
            };
        }
        Ok(())
    }

    fn hardware(&self) -> Vec<HwDevice> {
        let hw = |uid: &str, name: &str, in_ch, out_ch| HwDevice {
            uid: uid.into(),
            name: name.into(),
            in_ch,
            out_ch,
        };
        vec![
            hw("BuiltInSpeakerDevice", "MacBook Pro Speakers", 0, 2),
            hw("BuiltInMicrophoneDevice", "MacBook Pro Microphone", 1, 0),
            hw("USBHeadset", "USB Headset", 2, 2),
        ]
    }

    fn apps(&self) -> Vec<AppInfo> {
        let app = |id: &str, name: &str, running_output, regular| AppInfo {
            bundle_id: id.into(),
            name: name.into(),
            running_output,
            regular,
        };
        vec![
            app("com.apple.CoreSpeech", "CoreSpeech", false, false),
            app("com.apple.Safari", "Safari", true, true),
            app("com.apple.PowerChime", "PowerChime", false, false),
            app("com.spotify.client", "Spotify", true, true),
            app("us.zoom.xos", "Zoom", false, true),
            app("com.apple.finder", "Finder", false, true),
        ]
    }

    fn status(&self) -> EngineStatus {
        let mut s = self.status.clone();
        // The demo engine reports a delay per enabled device like the real one: two buffers.
        if s.delays.is_empty() && s.driver_version.as_deref() == Some(DEMO_DRIVER) {
            s.delays = self
                .cfg
                .devices
                .iter()
                .filter(|d| d.enabled)
                .map(|d| {
                    let delay = Delay {
                        frames: 2 * self.cfg.buffer_frames,
                        rate_hz: 48_000,
                    };
                    (d.id.clone(), delay)
                })
                .collect();
        }
        s
    }

    fn self_test(&mut self, _id: &str) -> anyhow::Result<SelfTest> {
        self.self_test
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no audio came back"))
    }

    fn measure_delay(&mut self, _id: &str) -> anyhow::Result<f64> {
        self.measured_ms
            .ok_or_else(|| anyhow::anyhow!("The click did not come back through the device."))
    }

    fn meters(&self, device_id: &str) -> Option<MeterReading> {
        let dev = self.cfg.devices.iter().find(|d| d.id == device_id)?;
        let t = self
            .frozen
            .unwrap_or_else(|| self.start.elapsed().as_secs_f32());
        let lin = |db: f32| 10f32.powf(db / 20.0);
        // What leaves each source channel (after its volume) and each node row (after its fader).
        let mut leaves: HashMap<&str, Vec<f32>> = HashMap::new();
        let mut sources = Vec::new();
        for (i, s) in dev.sources.iter().enumerate() {
            let levels: Vec<f32> = (0..s.channels)
                .map(|c| {
                    if s.muted || self.silent {
                        0.0
                    } else {
                        wave(t, i as f32 * 2.3 + f32::from(c) * 0.6)
                    }
                })
                .collect();
            // Pass-Thru stays quiet unless something plays into the device, like an idle app.
            let levels = if s.kind == SourceKind::PassThru {
                vec![0.0; levels.len()]
            } else {
                levels
            };
            let gain = lin(s.volume_db);
            leaves.insert(&s.id, levels.iter().map(|l| l * gain).collect());
            sources.push((s.id.clone(), levels));
        }
        // Peak of the cables into `to` (`None`: the device), per channel, as a max like a meter.
        let into = |leaves: &HashMap<&str, Vec<f32>>, to: Option<&str>, n: u16| {
            let mut rows = vec![0.0f32; usize::from(n)];
            for w in dev.wires.iter().filter(|w| w.to.as_deref() == to) {
                let l = leaves
                    .get(w.source.as_str())
                    .and_then(|v| v.get(usize::from(w.src_ch)))
                    .map_or(0.0, |l| l * lin(w.gain_db));
                if let Some(r) = rows.get_mut(usize::from(w.out_ch)) {
                    *r = r.max(l);
                }
            }
            rows
        };
        let mut nodes = Vec::new();
        let mut limiter_reduction = Vec::new();
        // Nodes only feed forward: one pass per node settles every chain.
        for _ in 0..dev.nodes.len() {
            (nodes, limiter_reduction) = dev
                .nodes
                .iter()
                .map(|n| {
                    let mut rows = into(&leaves, Some(&n.id), n.channels);
                    let peak = rows.iter().copied().fold(0.0, f32::max);
                    let mut reduction = 0.0;
                    for fx in &n.fx {
                        if let Fx::Limiter { ceiling_db, .. } = fx
                            && peak > lin(*ceiling_db)
                        {
                            reduction = 1.0 - lin(*ceiling_db) / peak;
                            rows.iter_mut().for_each(|r| *r = r.min(lin(*ceiling_db)));
                        }
                    }
                    let gain = if n.muted { 0.0 } else { lin(n.gain_db) };
                    rows.iter_mut().for_each(|r| *r *= gain);
                    ((n.id.clone(), rows), (n.id.clone(), reduction))
                })
                .unzip();
            for (id, rows) in &nodes {
                if let Some(n) = dev.nodes.iter().find(|n| &n.id == id) {
                    leaves.insert(&n.id, rows.clone());
                }
            }
        }
        let gain = if dev.muted { 0.0 } else { lin(dev.volume_db) };
        let mut channels = into(&leaves, None, dev.channels);
        channels.iter_mut().for_each(|c| *c *= gain);
        Some(MeterReading {
            sources,
            channels,
            nodes,
            limiter_reduction,
        })
    }
}

/// Populated config for `--demo` and screenshots.
pub fn demo_config() -> Config {
    let mut cfg = default_config();
    cfg.devices[0].sources.clear();
    cfg.devices[0].wires.clear();
    let app = |id: &str| SourceKind::App {
        bundle_id: id.into(),
    };
    apply(&mut cfg, Edit::AddSource(0, app("com.apple.Safari"), 2));
    apply(&mut cfg, Edit::AddSource(0, app("com.spotify.client"), 2));
    apply(&mut cfg, Edit::AddSource(0, SourceKind::PassThru, 2));
    // Spotify runs through a "Music" mixer, so the demo shows the middle of the graph.
    apply(
        &mut cfg,
        Edit::AddNode(0, NewNode::ForSource(1), Module::Mixer),
    );
    apply(&mut cfg, Edit::RenameNode(0, 0, "Music".into()));
    for fx in [
        Fx::Eq {
            low_hz: 120.0,
            low_db: 2.0,
            high_hz: 8000.0,
            high_db: 0.0,
        },
        Fx::Limiter {
            ceiling_db: -1.0,
            knee_db: 6.0,
        },
    ] {
        apply(&mut cfg, Edit::NodeFx(0, 0, FxEdit::Add(fx)));
    }
    apply(
        &mut cfg,
        Edit::AddMonitor(0, "BuiltInSpeakerDevice".into(), 2),
    );
    apply(&mut cfg, Edit::NewDevice);
    apply(&mut cfg, Edit::Rename(1, "Stream Mix".into()));
    apply(
        &mut cfg,
        Edit::AddSource(
            1,
            SourceKind::Device {
                uid: "BuiltInMicrophoneDevice".into(),
            },
            1,
        ),
    );
    apply(&mut cfg, Edit::NewDevice);
    apply(&mut cfg, Edit::Rename(2, "Zoom Out".into()));
    apply(&mut cfg, Edit::Enable(2, false));
    apply(&mut cfg, Edit::Volume(2, -6.0));
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_config_is_valid_and_has_a_mixer() {
        let cfg = demo_config();
        cfg.validate().unwrap();
        assert_eq!(cfg.devices[0].nodes.len(), 1);
        assert_eq!(cfg.devices[0].nodes[0].name, "Music");
    }

    #[test]
    fn fake_meters_follow_wires_and_mute() {
        let mut b = FakeBackend::frozen(1.5);
        let mut cfg = demo_config();
        b.apply(&cfg).unwrap();
        let m = b.meters("v1").unwrap();
        assert_eq!(m.sources.len(), 3);
        assert_eq!(m.channels.len(), 2);
        // The Music mixer reads what Spotify sends it.
        assert_eq!(m.nodes.len(), 1);
        assert_eq!(m.nodes[0].0, "n1");
        assert!(m.nodes[0].1.iter().all(|p| *p > 0.0), "{:?}", m.nodes);
        assert_eq!(m.limiter_reduction, [("n1".to_string(), 0.0)]);
        assert!(b.meters("nope").is_none());
        // A limiter below the signal reports how much it takes off.
        cfg.devices[0].sources[1].volume_db = 12.0;
        cfg.devices[0].nodes[0].fx[1] = Fx::Limiter {
            ceiling_db: -24.0,
            knee_db: 6.0,
        };
        b.apply(&cfg).unwrap();
        let m = b.meters("v1").unwrap();
        assert!(m.limiter_reduction[0].1 > 0.0);
        assert!(
            m.nodes[0]
                .1
                .iter()
                .all(|p| *p <= 10f32.powf(-24.0 / 20.0) + 1e-6)
        );
        cfg.devices[0].nodes[0].muted = true;
        b.apply(&cfg).unwrap();
        assert!(b.meters("v1").unwrap().nodes[0].1.iter().all(|p| *p == 0.0));
        cfg.devices[0].muted = true;
        b.apply(&cfg).unwrap();
        assert!(b.meters("v1").unwrap().channels.iter().all(|c| *c == 0.0));
    }
}
