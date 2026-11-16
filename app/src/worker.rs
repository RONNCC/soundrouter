//! Backend worker thread: applies configs, polls meters at 30 Hz, refreshes hardware/apps every 2 s.
use crate::icons::{self, IconResolver};
use eframe::egui::{ColorImage, Context};
use sr_core::backend::{AppInfo, Backend, EngineStatus, HwDevice, MeterReading};
use sr_core::config::{Config, SourceKind};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const METER_PERIOD: Duration = Duration::from_millis(33);
const REFRESH_PERIOD: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Snapshot {
    pub meters: HashMap<String, MeterReading>,
    pub hardware: Vec<HwDevice>,
    pub apps: Vec<AppInfo>,
    /// App icons by source key (bundle id or `path:<exe>`); only ever grows.
    pub icons: HashMap<String, Arc<ColorImage>>,
    /// Error of the most recent `apply`; cleared by the next success.
    pub apply_error: Option<String>,
    pub status: EngineStatus,
}

pub struct Worker {
    tx: Option<Sender<Config>>,
    handle: Option<JoinHandle<()>>,
    pub shared: Arc<Mutex<Snapshot>>,
}

/// Newest queued config, if any: bursts (slider drags) collapse to the latest.
fn drain_latest(rx: &Receiver<Config>, first: Option<Config>) -> Option<Config> {
    let mut newest = first;
    while let Ok(c) = rx.try_recv() {
        newest = Some(c);
    }
    newest
}

/// One worker iteration. Backend calls run with the snapshot mutex released, so the UI never
/// blocks behind slow CoreAudio work.
struct Pump {
    backend: Box<dyn Backend + Send>,
    cfg: Config,
    next_refresh: Instant,
    resolve_icon: IconResolver,
    /// Keys already looked up (hit or miss), so a refresh never repeats an AppKit call.
    icons_tried: HashSet<String>,
}

impl Pump {
    fn tick(&mut self, newest: Option<Config>, now: Instant, snap: &Mutex<Snapshot>) {
        let mut apply_result = None;
        if let Some(c) = newest {
            apply_result = Some(self.backend.apply(&c).err().map(|e| format!("{e:#}")));
            self.cfg = c;
        }
        let meters: HashMap<String, MeterReading> = self
            .cfg
            .devices
            .iter()
            .filter(|d| d.enabled)
            .filter_map(|d| Some((d.id.clone(), self.backend.meters(&d.id)?)))
            .collect();
        let refresh = (now >= self.next_refresh).then(|| {
            self.next_refresh = now + REFRESH_PERIOD;
            let apps = self.backend.apps();
            let icons = self.new_icons(&apps);
            (self.backend.hardware(), apps, self.backend.status(), icons)
        });
        let mut s = snap.lock().unwrap();
        if let Some(r) = apply_result {
            s.apply_error = r;
        }
        s.meters = meters;
        if let Some((hw, apps, status, icons)) = refresh {
            s.status = status;
            s.hardware = hw;
            s.apps = apps;
            s.icons.extend(icons);
        }
    }

    /// Icons for running apps and configured App sources not looked up before.
    fn new_icons(&mut self, apps: &[AppInfo]) -> Vec<(String, Arc<ColorImage>)> {
        let configured = self
            .cfg
            .devices
            .iter()
            .flat_map(|d| &d.sources)
            .filter_map(|s| match &s.kind {
                SourceKind::App { bundle_id } => Some(bundle_id.as_str()),
                _ => None,
            });
        let keys: Vec<String> = apps
            .iter()
            .map(|a| a.bundle_id.as_str())
            .chain(configured)
            .filter(|k| self.icons_tried.insert((*k).to_string()))
            .map(str::to_string)
            .collect();
        keys.into_iter()
            .filter_map(|k| Some((k.clone(), Arc::new((self.resolve_icon)(&k)?))))
            .collect()
    }
}

impl Worker {
    pub fn spawn(backend: Box<dyn Backend + Send>, ctx: Context) -> Worker {
        Self::spawn_with(backend, ctx, Box::new(icons::resolve))
    }

    /// `spawn` with a custom icon source (tests use none, so snapshots keep letter tiles).
    pub fn spawn_with(
        backend: Box<dyn Backend + Send>,
        ctx: Context,
        resolve_icon: IconResolver,
    ) -> Worker {
        let (tx, rx) = mpsc::channel::<Config>();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let snap = shared.clone();
        let handle = std::thread::spawn(move || {
            let mut pump = Pump {
                backend,
                cfg: Config::default(),
                next_refresh: Instant::now(),
                resolve_icon,
                icons_tried: HashSet::new(),
            };
            loop {
                let first = match rx.recv_timeout(METER_PERIOD) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                pump.tick(drain_latest(&rx, first), Instant::now(), &snap);
                ctx.request_repaint();
            }
        });
        Worker {
            tx: Some(tx),
            handle: Some(handle),
            shared,
        }
    }

    pub fn apply(&self, cfg: &Config) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(cfg.clone());
        }
    }

    /// Stop the thread and wait for it, so the backend (and its CoreAudio devices) is dropped
    /// before the process exits. Cmd-Q never drops `App`, so this is also called from `on_exit`.
    pub fn stop(&mut self) {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::default_config;
    use eframe::egui::Color32;
    use sr_core::config::Config;

    #[derive(Default)]
    struct Log {
        applied: Vec<Config>,
        fail: bool,
        /// Snapshot lock was free during every backend call.
        lock_free: bool,
    }

    struct Rec {
        log: Arc<Mutex<Log>>,
        snap: Arc<Mutex<Snapshot>>,
    }

    impl Rec {
        fn probe(&self) {
            if self.snap.try_lock().is_err() {
                self.log.lock().unwrap().lock_free = false;
            }
        }
    }

    impl Backend for Rec {
        fn apply(&mut self, cfg: &Config) -> anyhow::Result<()> {
            self.probe();
            let mut l = self.log.lock().unwrap();
            l.applied.push(cfg.clone());
            if l.fail {
                anyhow::bail!("tap denied");
            }
            Ok(())
        }
        fn hardware(&self) -> Vec<HwDevice> {
            self.probe();
            Vec::new()
        }
        fn apps(&self) -> Vec<AppInfo> {
            self.probe();
            vec![AppInfo {
                bundle_id: "com.fake.player".into(),
                name: "Player".into(),
                running_output: true,
            }]
        }
        fn meters(&self, id: &str) -> Option<MeterReading> {
            self.probe();
            (id == "v1").then(MeterReading::default)
        }
    }

    fn setup() -> (Pump, Arc<Mutex<Log>>, Arc<Mutex<Snapshot>>) {
        let log = Arc::new(Mutex::new(Log {
            lock_free: true,
            ..Log::default()
        }));
        let snap = Arc::new(Mutex::new(Snapshot::default()));
        let backend = Rec {
            log: log.clone(),
            snap: snap.clone(),
        };
        let pump = Pump {
            backend: Box::new(backend),
            cfg: Config::default(),
            next_refresh: Instant::now(),
            resolve_icon: Box::new(|_| None),
            icons_tried: HashSet::new(),
        };
        (pump, log, snap)
    }

    #[test]
    fn burst_of_applies_reaches_backend_once_with_latest() {
        let (tx, rx) = mpsc::channel();
        let mut cfgs = Vec::new();
        for i in 0..100 {
            let mut c = default_config();
            c.devices[0].volume_db = -(i as f32);
            cfgs.push(c.clone());
            tx.send(c).unwrap();
        }
        let first = rx.recv().ok();
        let newest = drain_latest(&rx, first);
        assert_eq!(newest.as_ref(), cfgs.last());
        let (mut pump, log, snap) = setup();
        pump.tick(newest, Instant::now(), &snap);
        let l = log.lock().unwrap();
        assert_eq!(l.applied.len(), 1);
        assert_eq!(l.applied[0].devices[0].volume_db, -99.0);
    }

    #[test]
    fn apply_error_surfaces_persists_across_meter_ticks_then_clears() {
        let (mut pump, log, snap) = setup();
        let now = Instant::now();
        log.lock().unwrap().fail = true;
        pump.tick(Some(default_config()), now, &snap);
        assert!(
            snap.lock()
                .unwrap()
                .apply_error
                .as_deref()
                .unwrap()
                .contains("tap denied")
        );
        pump.tick(None, now, &snap); // pure meter tick must not clear it
        assert!(snap.lock().unwrap().apply_error.is_some());
        log.lock().unwrap().fail = false;
        pump.tick(Some(default_config()), now, &snap);
        assert!(snap.lock().unwrap().apply_error.is_none());
    }

    #[test]
    fn meters_only_for_enabled_devices_and_lock_never_held_over_backend() {
        let (mut pump, log, snap) = setup();
        let mut c = default_config();
        let mut d2 = c.devices[0].clone();
        d2.id = "v2".into();
        d2.enabled = false;
        c.devices.push(d2);
        pump.tick(Some(c), Instant::now(), &snap);
        let s = snap.lock().unwrap();
        assert_eq!(s.meters.keys().collect::<Vec<_>>(), ["v1"]);
        assert!(log.lock().unwrap().lock_free);
    }

    #[test]
    fn hardware_refresh_is_throttled_to_period() {
        let (mut pump, _log, snap) = setup();
        let t0 = Instant::now();
        pump.tick(None, t0, &snap);
        let after_first = pump.next_refresh;
        assert_eq!(after_first, t0 + REFRESH_PERIOD);
        pump.tick(None, t0 + REFRESH_PERIOD / 2, &snap);
        assert_eq!(pump.next_refresh, after_first); // not refreshed yet
        pump.tick(None, t0 + REFRESH_PERIOD, &snap);
        assert_eq!(pump.next_refresh, t0 + REFRESH_PERIOD * 2);
    }

    #[test]
    fn icons_for_running_and_configured_apps_resolve_once_on_refresh() {
        use crate::model::{Edit, apply};
        let (mut pump, _log, snap) = setup();
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = asked.clone();
        pump.resolve_icon = Box::new(move |k| {
            log.lock().unwrap().push(k.to_string());
            (k != "com.fake.missing").then(|| ColorImage::filled([2, 2], Color32::RED))
        });
        let mut c = default_config();
        for id in ["com.fake.configured", "com.fake.missing"] {
            let kind = SourceKind::App {
                bundle_id: id.into(),
            };
            apply(&mut c, Edit::AddSource(0, kind, 2));
        }
        let t0 = Instant::now();
        pump.tick(Some(c), t0, &snap);
        pump.tick(None, t0 + REFRESH_PERIOD, &snap); // second refresh must not look up again
        let s = snap.lock().unwrap();
        let mut got: Vec<_> = s.icons.keys().cloned().collect();
        got.sort();
        assert_eq!(got, ["com.fake.configured", "com.fake.player"]);
        assert_eq!(s.icons["com.fake.player"].size, [2, 2]);
        let mut asked = asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(
            asked,
            ["com.fake.configured", "com.fake.missing", "com.fake.player"]
        );
    }
}
