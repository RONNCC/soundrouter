//! Backend worker thread: applies configs, polls meters at 30 Hz, refreshes hardware/apps every 2 s.
use eframe::egui::Context;
use sr_core::backend::{AppInfo, Backend, HwDevice, MeterReading};
use sr_core::config::Config;
use std::collections::HashMap;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const METER_PERIOD: Duration = Duration::from_millis(33);
const REFRESH_PERIOD: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Snapshot {
    pub meters: HashMap<String, MeterReading>,
    pub hardware: Vec<HwDevice>,
    pub apps: Vec<AppInfo>,
    /// Error of the most recent `apply`; cleared by the next success.
    pub apply_error: Option<String>,
}

pub struct Worker {
    tx: Sender<Config>,
    pub shared: Arc<Mutex<Snapshot>>,
}

impl Worker {
    pub fn spawn(mut backend: Box<dyn Backend + Send>, ctx: Context) -> Worker {
        let (tx, rx) = mpsc::channel::<Config>();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let snap = shared.clone();
        std::thread::spawn(move || {
            let mut cfg = Config::default();
            let mut next_refresh = Instant::now();
            loop {
                let mut newest = match rx.recv_timeout(METER_PERIOD) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                while let Ok(c) = rx.try_recv() {
                    newest = Some(c); // coalesce slider drags
                }
                if let Some(c) = newest {
                    let res = backend.apply(&c);
                    snap.lock().unwrap().apply_error = res.err().map(|e| format!("{e:#}"));
                    cfg = c;
                }
                let meters: HashMap<String, MeterReading> = cfg
                    .devices
                    .iter()
                    .filter(|d| d.enabled)
                    .filter_map(|d| Some((d.id.clone(), backend.meters(&d.id)?)))
                    .collect();
                let refresh = (Instant::now() >= next_refresh).then(|| {
                    next_refresh = Instant::now() + REFRESH_PERIOD;
                    (backend.hardware(), backend.apps())
                });
                {
                    let mut s = snap.lock().unwrap();
                    s.meters = meters;
                    if let Some((hw, apps)) = refresh {
                        s.hardware = hw;
                        s.apps = apps;
                    }
                }
                ctx.request_repaint();
            }
        });
        Worker { tx, shared }
    }

    pub fn apply(&self, cfg: &Config) {
        let _ = self.tx.send(cfg.clone());
    }
}
