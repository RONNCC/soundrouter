//! In-app backend for `--demo`: synthetic meters, fake apps and hardware.
use crate::model::{Edit, apply, default_config};
use sr_core::backend::{AppInfo, Backend, HwDevice, MeterReading};
use sr_core::config::{Config, SourceKind};
use std::time::Instant;

pub struct FakeBackend {
    cfg: Config,
    start: Instant,
    /// Meter clock pinned to this many seconds, so snapshots are deterministic.
    frozen: Option<f32>,
}

impl FakeBackend {
    pub fn new() -> Self {
        Self {
            cfg: Config::default(),
            start: Instant::now(),
            frozen: None,
        }
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
        let app = |id: &str, name: &str, running_output| AppInfo {
            bundle_id: id.into(),
            name: name.into(),
            running_output,
        };
        vec![
            app("com.apple.Safari", "Safari", true),
            app("com.spotify.client", "Spotify", true),
            app("us.zoom.xos", "Zoom", false),
        ]
    }

    fn meters(&self, device_id: &str) -> Option<MeterReading> {
        let dev = self.cfg.devices.iter().find(|d| d.id == device_id)?;
        let t = self
            .frozen
            .unwrap_or_else(|| self.start.elapsed().as_secs_f32());
        let mut channels = vec![0.0f32; usize::from(dev.channels)];
        let mut sources = Vec::new();
        for (i, s) in dev.sources.iter().enumerate() {
            let levels: Vec<f32> = (0..s.channels)
                .map(|c| {
                    if s.muted {
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
            let gain = 10f32.powf(s.volume_db / 20.0);
            for w in dev.wires.iter().filter(|w| w.source == s.id) {
                let l = levels[usize::from(w.src_ch)] * gain;
                let out = &mut channels[usize::from(w.out_ch)];
                *out = out.max(l);
            }
            sources.push((s.id.clone(), levels));
        }
        let gain = if dev.muted {
            0.0
        } else {
            10f32.powf(dev.volume_db / 20.0)
        };
        channels.iter_mut().for_each(|c| *c *= gain);
        Some(MeterReading { sources, channels })
    }
}

/// Populated config for `--demo` and screenshots.
pub fn demo_config() -> Config {
    let mut cfg = default_config();
    let app = |id: &str| SourceKind::App {
        bundle_id: id.into(),
    };
    apply(&mut cfg, Edit::AddSource(0, app("com.apple.Safari"), 2));
    apply(&mut cfg, Edit::AddSource(0, app("com.spotify.client"), 2));
    apply(&mut cfg, Edit::AddSource(0, SourceKind::PassThru, 2));
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
    fn demo_config_is_valid() {
        demo_config().validate().unwrap();
    }

    #[test]
    fn fake_meters_follow_wires_and_mute() {
        let mut b = FakeBackend::new();
        let mut cfg = demo_config();
        b.apply(&cfg).unwrap();
        let m = b.meters("v1").unwrap();
        assert_eq!(m.sources.len(), 3);
        assert_eq!(m.channels.len(), 2);
        assert!(b.meters("nope").is_none());
        cfg.devices[0].muted = true;
        b.apply(&cfg).unwrap();
        assert!(b.meters("v1").unwrap().channels.iter().all(|c| *c == 0.0));
    }
}
