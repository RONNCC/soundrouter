//! Loopback self-test orchestration, shared by the app's engine and the CLI.
//!
//! A virtual device's input carries what the engine mixes once an engine runs for it (apps' audio
//! reaches it only through a Pass-Thru route); only without an engine does the driver loop the
//! device's output straight back. So while an engine runs the test must route the probe's tone through
//! the engine, as a user would, and put the user's routing back exactly afterwards.
use crate::backend::{Backend, SelfTest, TccStatus};
use crate::config::{Config, Source, SourceKind, VirtualDevice, Wire};
use anyhow::{Context, Result, bail};

/// Below this the input is silent, not merely quiet or glitchy.
pub const SILENT_DBFS: f64 = -90.0;

/// `cfg` with device `id` reduced to a unit Pass-Thru route (every channel to itself), or `None`
/// when the device is absent or disabled (no engine runs for it).
///
/// Only wires, gains and monitor wires change, so a running engine applies it as a plan swap: the
/// aggregate, taps and monitor devices stay as they are. The tone must never reach a monitor's
/// hardware, so every monitor wire is dropped.
pub fn probe_config(cfg: &Config, id: &str) -> Option<Config> {
    let mut out = cfg.clone();
    let dev = out.devices.iter_mut().find(|d| d.id == id && d.enabled)?;
    dev.muted = false;
    dev.volume_db = 0.0;
    let src = match dev
        .sources
        .iter_mut()
        .find(|s| s.kind == SourceKind::PassThru)
    {
        Some(s) => s,
        None => {
            let id = unused_source_id(dev);
            dev.sources.push(Source {
                id,
                kind: SourceKind::PassThru,
                channels: dev.channels,
                volume_db: 0.0,
                muted: false,
                mute_original: false,
                name: String::new(),
            });
            dev.sources.last_mut()?
        }
    };
    src.volume_db = 0.0;
    src.muted = false;
    let source = src.id.clone();
    dev.wires = (0..dev.channels)
        .map(|ch| Wire {
            source: source.clone(),
            src_ch: ch,
            out_ch: ch,
        })
        .collect();
    for m in &mut dev.monitors {
        m.wires.clear();
    }
    Some(out)
}

fn unused_source_id(dev: &VirtualDevice) -> String {
    (0..)
        .map(|n| format!("selftest-{n}"))
        .find(|c| dev.sources.iter().all(|s| &s.id != c))
        .expect("endless iterator")
}

/// Puts `original` back when dropped, so a failed or panicking probe cannot leave the user's routing changed.
struct Restore<'a> {
    backend: &'a mut dyn Backend,
    original: &'a Config,
    id: &'a str,
    done: bool,
}

impl Restore<'_> {
    fn restore(&mut self) -> Result<()> {
        self.done = true;
        self.backend
            .apply(self.original)
            .context("the test finished but your routing could not be restored")?;
        // Plans a busy audio thread had not collected are flushed by a meter read; the tone's peaks
        // are in that reading, so the GUI's next one starts clean.
        let _ = self.backend.meters(self.id);
        Ok(())
    }
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.restore();
        }
    }
}

/// Run `probe` (tone in, measure the input) with an engine-compatible route.
///
/// `cfg` is the config the backend currently runs; `engine_running` says whether an engine holds
/// device `id`. Without one, `probe` runs on the driver's direct loop and nothing is applied.
/// Otherwise the probe route is applied first and `cfg` applied again afterwards, on every exit.
pub fn run_routed<T>(
    backend: &mut dyn Backend,
    cfg: &Config,
    id: &str,
    engine_running: bool,
    probe: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let tmp = engine_running.then(|| probe_config(cfg, id)).flatten();
    let Some(tmp) = tmp else {
        return probe();
    };
    let mut guard = Restore {
        backend,
        original: cfg,
        id,
        done: false,
    };
    guard
        .backend
        .apply(&tmp)
        .context("could not route the test tone through the engine")?;
    let result = probe();
    guard.restore()?;
    result
}

/// Turn a failed measurement into the diagnosis the user can act on. A pass or a partial result
/// (some audio, with dropouts or too quiet) goes through unchanged.
pub fn verdict(t: SelfTest, mic: TccStatus) -> Result<SelfTest> {
    if t.passed || t.rms_dbfs >= SILENT_DBFS {
        return Ok(t);
    }
    match mic {
        TccStatus::Denied => bail!(
            "Microphone access is off, so macOS hands SoundRouter silence when it reads the \
             device's input (the tone itself was played). Allow SoundRouter in System Settings → \
             Privacy & Security → Microphone, then try again."
        ),
        TccStatus::Unknown => bail!(
            "Nothing came back on the device's input ({:.0} dBFS). Microphone access has not been \
             granted yet, which gives silence; allow it when macOS asks, or in System Settings → \
             Privacy & Security → Microphone, then try again.",
            t.rms_dbfs
        ),
        TccStatus::Granted => bail!(
            "The tone was played but nothing came back on the device's input ({:.0} dBFS) \
             although microphone access is allowed, so the audio path itself is silent. Quit and \
             reopen SoundRouter; if that does not help, reinstall the driver.",
            t.rms_dbfs
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{AppInfo, HwDevice, MeterReading};
    use crate::config::{Monitor, MonitorWire};

    #[derive(Default)]
    struct Mock {
        applied: Vec<Config>,
        fail_apply_at: Option<usize>,
    }

    impl Backend for Mock {
        fn apply(&mut self, cfg: &Config) -> Result<()> {
            self.applied.push(cfg.clone());
            if self.fail_apply_at == Some(self.applied.len()) {
                bail!("boom");
            }
            Ok(())
        }
        fn hardware(&self) -> Vec<HwDevice> {
            Vec::new()
        }
        fn apps(&self) -> Vec<AppInfo> {
            Vec::new()
        }
        fn meters(&self, _id: &str) -> Option<MeterReading> {
            None
        }
    }

    fn dev(id: &str) -> VirtualDevice {
        VirtualDevice {
            id: id.into(),
            name: id.into(),
            enabled: true,
            channels: 2,
            volume_db: -6.0,
            muted: true,
            sources: vec![Source {
                id: "zoom".into(),
                kind: SourceKind::App {
                    bundle_id: "us.zoom.xos".into(),
                },
                channels: 2,
                volume_db: -3.0,
                muted: false,
                mute_original: true,
                name: String::new(),
            }],
            wires: vec![Wire {
                source: "zoom".into(),
                src_ch: 0,
                out_ch: 1,
            }],
            monitors: vec![Monitor {
                device_uid: "BuiltInSpeakerDevice".into(),
                enabled: true,
                volume_db: 0.0,
                wires: vec![MonitorWire {
                    out_ch: 0,
                    dst_ch: 0,
                }],
                name: String::new(),
            }],
        }
    }

    fn cfg() -> Config {
        Config {
            devices: vec![dev("v1"), dev("v2")],
            ..Config::default()
        }
    }

    fn st(rms_dbfs: f64, dropouts: u64) -> SelfTest {
        SelfTest {
            rms_dbfs,
            dropouts,
            rate: 48_000.0,
            passed: rms_dbfs > -20.0 && dropouts == 0,
        }
    }

    #[test]
    fn probe_config_is_a_valid_unit_passthru_with_silent_monitors() {
        let c = cfg();
        let p = probe_config(&c, "v1").unwrap();
        p.validate().unwrap();
        let d = &p.devices[0];
        assert!(!d.muted && d.volume_db == 0.0);
        let pt = d
            .sources
            .iter()
            .find(|s| s.kind == SourceKind::PassThru)
            .unwrap();
        assert_eq!(pt.id, "selftest-0");
        let wires: Vec<_> = d
            .wires
            .iter()
            .map(|w| (w.source.as_str(), w.src_ch, w.out_ch))
            .collect();
        assert_eq!(wires, [("selftest-0", 0, 0), ("selftest-0", 1, 1)]);
        assert!(d.monitors[0].enabled && d.monitors[0].wires.is_empty());
        // taps (app sources) and the other device are untouched
        assert_eq!(d.sources[0], c.devices[0].sources[0]);
        assert_eq!(p.devices[1], c.devices[1]);
        // the user's own Pass-Thru source is reused, not duplicated
        let mut c2 = c.clone();
        c2.devices[0].sources.push(Source {
            id: "mine".into(),
            kind: SourceKind::PassThru,
            channels: 2,
            volume_db: -20.0,
            muted: true,
            mute_original: false,
            name: String::new(),
        });
        let d2 = &probe_config(&c2, "v1").unwrap().devices[0];
        assert_eq!(d2.sources.len(), 2);
        assert_eq!(d2.wires[0].source, "mine");
        assert!(!d2.sources[1].muted && d2.sources[1].volume_db == 0.0);
    }

    #[test]
    fn no_probe_config_without_an_enabled_device() {
        let mut c = cfg();
        c.devices[0].enabled = false;
        assert!(probe_config(&c, "v1").is_none());
        assert!(probe_config(&c, "nope").is_none());
    }

    #[test]
    fn engine_running_routes_the_probe_then_restores_the_plan_exactly() {
        let c = cfg();
        let mut b = Mock::default();
        let r = run_routed(&mut b, &c, "v1", true, || Ok(st(-9.0, 0)));
        assert!(r.unwrap().passed);
        assert_eq!(b.applied, [probe_config(&c, "v1").unwrap(), c]);
    }

    #[test]
    fn probe_failure_still_restores() {
        let c = cfg();
        let mut b = Mock::default();
        let r = run_routed::<SelfTest>(&mut b, &c, "v1", true, || bail!("no device"));
        assert_eq!(r.unwrap_err().to_string(), "no device");
        assert_eq!(b.applied.last(), Some(&c));
        assert_eq!(b.applied.len(), 2);
    }

    #[test]
    fn probe_panic_still_restores() {
        let c = cfg();
        let mut b = Mock::default();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_routed::<SelfTest>(&mut b, &c, "v1", true, || panic!("probe died"))
        }));
        assert!(r.is_err());
        assert_eq!(b.applied.last(), Some(&c));
    }

    #[test]
    fn failed_route_setup_restores_and_says_so() {
        let c = cfg();
        let mut b = Mock {
            fail_apply_at: Some(1),
            ..Mock::default()
        };
        let e = run_routed::<SelfTest>(&mut b, &c, "v1", true, || panic!("probe must not run"))
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("could not route the test tone"),
            "{e:#}"
        );
        assert_eq!(b.applied.last(), Some(&c));
    }

    #[test]
    fn failed_restore_is_reported() {
        let c = cfg();
        let mut b = Mock {
            fail_apply_at: Some(2),
            ..Mock::default()
        };
        let e = run_routed(&mut b, &c, "v1", true, || Ok(st(-9.0, 0))).unwrap_err();
        assert!(format!("{e:#}").contains("could not be restored"), "{e:#}");
    }

    #[test]
    fn no_engine_means_no_apply() {
        let c = cfg();
        let mut b = Mock::default();
        run_routed(&mut b, &c, "v1", false, || Ok(st(-9.0, 0))).unwrap();
        run_routed(&mut b, &c, "nope", true, || Ok(st(-9.0, 0))).unwrap();
        assert!(b.applied.is_empty());
    }

    #[test]
    fn meters_are_drained_after_restore() {
        struct Counting(Mock, std::cell::Cell<usize>);
        impl Backend for Counting {
            fn apply(&mut self, cfg: &Config) -> Result<()> {
                self.0.apply(cfg)
            }
            fn hardware(&self) -> Vec<HwDevice> {
                Vec::new()
            }
            fn apps(&self) -> Vec<AppInfo> {
                Vec::new()
            }
            fn meters(&self, _id: &str) -> Option<MeterReading> {
                self.1.set(self.1.get() + 1);
                None
            }
        }
        let mut b = Counting(Mock::default(), Default::default());
        run_routed(&mut b, &cfg(), "v1", true, || Ok(st(-9.0, 0))).unwrap();
        assert_eq!(b.1.get(), 1);
    }

    #[test]
    fn silent_input_is_diagnosed_by_microphone_permission() {
        let silent = st(-200.0, 0);
        let denied = verdict(silent.clone(), TccStatus::Denied)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("Microphone access is off"), "{denied}");
        let unknown = verdict(silent.clone(), TccStatus::Unknown)
            .unwrap_err()
            .to_string();
        assert!(
            unknown.contains("not been granted yet") && unknown.contains("-200"),
            "{unknown}"
        );
        let path = verdict(silent, TccStatus::Granted).unwrap_err().to_string();
        assert!(path.contains("audio path itself is silent"), "{path}");
    }

    #[test]
    fn pass_and_partial_results_are_not_rewritten() {
        assert!(verdict(st(-9.0, 0), TccStatus::Denied).unwrap().passed);
        let glitchy = st(-9.0, 3);
        assert_eq!(
            verdict(glitchy.clone(), TccStatus::Denied).unwrap(),
            glitchy
        );
        let quiet = st(-40.0, 0);
        assert_eq!(verdict(quiet.clone(), TccStatus::Granted).unwrap(), quiet);
    }
}
