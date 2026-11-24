//! Backend worker thread: applies configs, polls meters at 30 Hz while the window is visible (and
//! wakes the UI only when something changed), refreshes hardware/apps every 2 s.
use crate::fake::FakeBackend;
use crate::icons::{self, IconResolver};
use eframe::egui::{ColorImage, Context};
use sr_core::backend::{
    AppInfo, Backend, EngineOptions, EngineStatus, HwDevice, MeterReading, SelfTest,
};
use sr_core::config::{Config, SourceKind};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const METER_PERIOD: Duration = Duration::from_millis(33);
/// Wake-up period while there are no meters to read (window hidden, or no device enabled): only
/// messages and refreshes.
const HIDDEN_PERIOD: Duration = Duration::from_millis(500);
const REFRESH_PERIOD: Duration = Duration::from_secs(2);
/// How long Cmd-Q waits for the worker thread; a HAL call that never returns must not keep the
/// app from quitting.
const STOP_WAIT: Duration = Duration::from_secs(2);

/// Starts the real engine; the worker calls it again while the first start failed.
pub type EngineStart = Box<dyn FnMut() -> anyhow::Result<Box<dyn Backend + Send>> + Send>;

/// The worker's time source, so tests step through the retry schedule without waiting.
type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Wait before the next start attempt after `failures` failed ones (the first start included):
/// 1 s, 2 s, 4 s, then every 10 s. At login coreaudiod may not be ready yet; it usually is within
/// seconds, and an engine that never comes back costs one cheap attempt every 10 s.
fn retry_delay(failures: u32) -> Duration {
    match failures {
        0 | 1 => Duration::from_secs(1),
        2 => Duration::from_secs(2),
        3 => Duration::from_secs(4),
        _ => Duration::from_secs(10),
    }
}

/// Give `backend` the window's options (before its first config). A failure is logged: the engine
/// keeps its previous options and the next change tries again.
fn push_options(backend: &mut dyn Backend, options: Option<&EngineOptions>) {
    if let Some(o) = options
        && let Err(e) = backend.set_options(o)
    {
        log::warn!("engine options not applied: {e:#}");
    }
}

/// An engine that has not started yet.
struct Retry {
    start: EngineStart,
    failures: u32,
    next: Instant,
}

/// A thread that can be waited for with a limit: `JoinHandle::join` has no timeout, so the thread's
/// closure holds a sender that nothing uses, and its drop (return or panic) ends the wait.
struct Joinable {
    handle: JoinHandle<()>,
    finished: Receiver<()>,
}

impl Joinable {
    fn spawn(body: impl FnOnce() + Send + 'static) -> Self {
        let (alive, finished) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let _alive = alive;
            body();
        });
        Self { handle, finished }
    }

    /// Joins the thread if it ends within `wait`. A thread still running then is dropped, not
    /// killed: process exit ends it. Returns whether it ended.
    fn join_within(self, wait: Duration) -> bool {
        if self.finished.recv_timeout(wait) == Err(RecvTimeoutError::Timeout) {
            log::warn!(
                "the audio worker did not stop within {} s (a CoreAudio call may be hung); quitting anyway",
                wait.as_secs_f32()
            );
            return false;
        }
        let _ = self.handle.join();
        true
    }
}

/// How long after an apply that starts or rebuilds a device its status is read again (the start takes
/// tens of milliseconds).
const AFTER_START_REFRESH: Duration = Duration::from_millis(300);

/// Progress of the loopback self-test the setup sheet asks for.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SelfTestState {
    #[default]
    Idle,
    Running,
    Done(Result<SelfTest, String>),
}

/// Progress of the delay measurement the "Measure delay" sheet asks for: milliseconds, or why not.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum MeasureState {
    #[default]
    Idle,
    Running,
    Done(Result<f64, String>),
}

#[derive(Default)]
pub struct Snapshot {
    pub meters: HashMap<String, MeterReading>,
    pub hardware: Vec<HwDevice>,
    pub apps: Vec<AppInfo>,
    /// App icons by source key (bundle id or `path:<exe>`); the least recently seen go past 64.
    pub icons: HashMap<String, Arc<ColorImage>>,
    /// Error of the most recent `apply`; cleared by the next success.
    pub apply_error: Option<String>,
    pub status: EngineStatus,
    /// False until the first status poll, so the GUI does not mistake "not asked yet" for "no driver".
    pub status_ready: bool,
    pub self_test: SelfTestState,
    pub measure: MeasureState,
    /// Why the real engine is not running while the worker retries starting it; `None` once it runs.
    pub engine_error: Option<String>,
}

enum Msg {
    Apply(Config),
    SelfTest(String),
    MeasureDelay(String),
    /// New engine options; the worker keeps the latest and gives them to every engine it starts.
    Options(EngineOptions),
    /// Replace the backend with the synthetic one.
    Demo,
    /// Only to end a wait: the window was shown or hidden.
    Wake,
}

pub struct Worker {
    tx: Option<Sender<Msg>>,
    handle: Option<Joinable>,
    pub shared: Arc<Mutex<Snapshot>>,
    visible: Arc<AtomicBool>,
    /// Tests: no thread. The loop body runs on the caller's thread as soon as a message is sent,
    /// so the reply is there before the next frame, whatever the scheduler does.
    #[cfg(test)]
    inline: Option<std::cell::RefCell<Core>>,
}

/// The worker loop's state; `iterate` is one wake-up.
struct Core {
    pump: Pump,
    rx: Receiver<Msg>,
    snap: Arc<Mutex<Snapshot>>,
    ctx: Context,
    visible: Arc<AtomicBool>,
    clock: Clock,
}

impl Core {
    fn period(&self) -> Duration {
        // A device that is off has no meters: 30 wake-ups a second would read nothing.
        if self.visible.load(Relaxed) && self.pump.cfg.devices.iter().any(|d| d.enabled) {
            METER_PERIOD
        } else {
            HIDDEN_PERIOD
        }
    }

    /// One wake-up; the UI is only asked to repaint when the snapshot changed. A panic in the
    /// backend must not end the thread: nothing would apply configs any more and the window would
    /// look fine. It is logged by the panic hook and shown as the apply error instead.
    fn iterate(&mut self, first: Option<Msg>) {
        let batch = drain_latest(&self.rx, first);
        self.pump.visible = self.visible.load(Relaxed);
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.pump.run(batch, (self.clock)(), &self.snap)
        }));
        let changed = run.unwrap_or_else(|payload| {
            let why = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            let msg = format!("The audio engine hit an internal error ({why}). See the log.");
            let mut s = self.snap.lock().unwrap_or_else(PoisonError::into_inner);
            s.apply_error = Some(msg.clone());
            // A test that panicked would otherwise stay "Running" for ever.
            if s.self_test == SelfTestState::Running {
                s.self_test = SelfTestState::Done(Err(msg.clone()));
            }
            if s.measure == MeasureState::Running {
                s.measure = MeasureState::Done(Err(msg));
            }
            true
        });
        if changed {
            self.ctx.request_repaint();
        }
    }
}

/// Everything queued since the last iteration. Bursts of configs (slider drags) collapse to the
/// latest; self-test requests for the same device collapse to one.
#[derive(Default)]
struct Batch {
    cfg: Option<Config>,
    options: Option<EngineOptions>,
    tests: Vec<String>,
    measures: Vec<String>,
    demo: bool,
}

fn drain_latest(rx: &Receiver<Msg>, first: Option<Msg>) -> Batch {
    let mut b = Batch::default();
    for m in first
        .into_iter()
        .chain(std::iter::from_fn(|| rx.try_recv().ok()))
    {
        match m {
            Msg::Apply(c) => b.cfg = Some(c),
            Msg::Options(o) => b.options = Some(o),
            Msg::SelfTest(id) => {
                if !b.tests.contains(&id) {
                    b.tests.push(id);
                }
            }
            Msg::MeasureDelay(id) => {
                if !b.measures.contains(&id) {
                    b.measures.push(id);
                }
            }
            Msg::Demo => b.demo = true,
            Msg::Wake => {}
        }
    }
    b
}

/// Whether applying `new` over `old` switches on or rebuilds a device (a mute, volume or wire edit
/// does not): the edits after which a device's status goes through "Starting".
fn starts_something(old: &Config, new: &Config) -> bool {
    old.buffer_frames != new.buffer_frames
        || new.devices.iter().filter(|d| d.enabled).any(|d| {
            old.devices.iter().find(|o| o.id == d.id).is_none_or(|o| {
                !o.enabled
                    || o.channels != d.channels
                    || o.sources.len() != d.sources.len()
                    || o.sources
                        .iter()
                        .zip(&d.sources)
                        .any(|(a, b)| (&a.kind, a.mute_original) != (&b.kind, b.mute_original))
                    || o.monitors.len() != d.monitors.len()
                    || o.monitors
                        .iter()
                        .zip(&d.monitors)
                        .any(|(a, b)| (&a.device_uid, a.enabled) != (&b.device_uid, b.enabled))
            })
        })
}

/// Icons kept in the snapshot: every executable ever seen adds one, and a long session sees many.
const ICON_CAP: usize = 64;

/// One worker iteration. Backend calls run with the snapshot mutex released, so the UI never
/// blocks behind slow CoreAudio work.
struct Pump {
    backend: Box<dyn Backend + Send>,
    cfg: Config,
    next_refresh: Instant,
    resolve_icon: IconResolver,
    /// Keys already looked up (hit or miss), so a refresh never repeats an AppKit call, with the
    /// refresh (`icon_clock`) that last found them running or configured.
    icon_used: HashMap<String, u64>,
    icon_clock: u64,
    /// The window can be seen: only then are meters read.
    visible: bool,
    /// The engine is still to be started again (see [`Retry`]).
    retry: Option<Retry>,
    /// A config reached the backend: only then is there one to give an engine that starts late.
    applied: bool,
    /// The latest options from the window; `None` until it sent any (the engine keeps its defaults).
    options: Option<EngineOptions>,
}

impl Pump {
    /// Demo swap, then config, then self-tests: the tests see the devices the config creates.
    /// Returns whether the snapshot changed.
    fn run(&mut self, b: Batch, now: Instant, snap: &Mutex<Snapshot>) -> bool {
        if b.demo {
            self.backend = Box::new(FakeBackend::new());
            self.next_refresh = now;
            self.retry = None;
            snap.lock().unwrap().engine_error = None;
        }
        // Before the config: a device the config builds then starts with the new options.
        let new_options = b.options.is_some();
        if b.options.is_some() {
            self.options = b.options;
        }
        if new_options || b.demo {
            push_options(&mut *self.backend, self.options.as_ref());
        }
        let mut changed = self.tick(b.cfg, now, snap) || b.demo;
        for id in b.tests {
            changed = true;
            snap.lock().unwrap().self_test = SelfTestState::Running;
            let r = self.backend.self_test(&id).map_err(|e| format!("{e:#}"));
            snap.lock().unwrap().self_test = SelfTestState::Done(r);
        }
        for id in b.measures {
            changed = true;
            snap.lock().unwrap().measure = MeasureState::Running;
            let r = self
                .backend
                .measure_delay(&id)
                .map_err(|e| format!("{e:#}"));
            snap.lock().unwrap().measure = MeasureState::Done(r);
        }
        changed
    }

    /// Returns whether the snapshot changed. Meters are read only while visible; the hidden
    /// snapshot keeps the last ones until the window is shown again.
    fn tick(&mut self, newest: Option<Config>, now: Instant, snap: &Mutex<Snapshot>) -> bool {
        let engine = self.retry_engine(now);
        let newest = match newest {
            None if matches!(engine, Some(None)) && self.applied => Some(self.cfg.clone()),
            n => n,
        };
        let mut apply_result = None;
        let mut starts = false;
        if let Some(c) = newest {
            sr_core::trace::stage("worker: apply begins");
            let r = self.backend.apply(&c);
            sr_core::trace::stage("worker: apply returned");
            // The status bar shows it too, but only the log file survives for a bug report.
            apply_result = Some(r.err().map(|e| {
                let msg = format!("{e:#}");
                log::warn!("apply failed: {msg}");
                msg
            }));
            starts = starts_something(&self.cfg, &c);
            self.cfg = c;
            self.applied = true;
        }
        let meters: Option<HashMap<String, MeterReading>> = self.visible.then(|| {
            self.cfg
                .devices
                .iter()
                .filter(|d| d.enabled)
                .filter_map(|d| Some((d.id.clone(), self.backend.meters(&d.id)?)))
                .collect()
        });
        if self.visible
            && sr_core::trace::enabled()
            && let Some(m) = &meters
            && m.values()
                .any(|r| r.sources.iter().any(|(_, c)| c.iter().any(|p| *p > 0.0)))
        {
            sr_core::trace::once("worker: first non-silent source meter read");
        }
        let refresh = (now >= self.next_refresh).then(|| {
            self.next_refresh = now + REFRESH_PERIOD;
            let apps = self.backend.apps();
            let icons = self.new_icons(&apps);
            (self.backend.hardware(), apps, self.backend.status(), icons)
        });
        // A device that was just switched on or rebuilt shows "Starting" until the next status refresh:
        // pull it in to a moment after the start instead of leaving the dot amber for up to 2 s.
        if starts {
            self.next_refresh = self.next_refresh.min(Instant::now() + AFTER_START_REFRESH);
        }
        let mut s = snap.lock().unwrap();
        let mut changed = apply_result.is_some() || refresh.is_some();
        if let Some(r) = apply_result {
            s.apply_error = r;
        }
        if let Some(e) = engine
            && s.engine_error != e
        {
            s.engine_error = e;
            changed = true;
        }
        if let Some(m) = meters
            && s.meters != m
        {
            s.meters = m;
            changed = true;
        }
        if let Some((hw, apps, status, icons)) = refresh {
            s.status = status;
            s.status_ready = true;
            s.hardware = hw;
            s.apps = apps;
            s.icons.extend(icons);
            self.trim_icons(&mut s.icons);
        }
        changed
    }

    /// Starts the engine again when a retry is due. `Some(None)`: it runs now (the stand-in is
    /// replaced and the next refresh is due at once); `Some(Some(why))`: it failed again.
    fn retry_engine(&mut self, now: Instant) -> Option<Option<String>> {
        let r = self.retry.as_mut().filter(|r| now >= r.next)?;
        // Scheduled before the attempt: a start that panics must not be retried at every wake-up.
        r.failures = r.failures.saturating_add(1);
        r.next = now + retry_delay(r.failures);
        match (r.start)() {
            Ok(mut engine) => {
                log::info!(
                    "audio engine started after {} failed attempt(s)",
                    r.failures
                );
                // Before the config `tick` gives it: a restarted engine must not fall back to defaults.
                push_options(&mut *engine, self.options.as_ref());
                self.retry = None;
                self.backend = engine;
                self.next_refresh = now;
                Some(None)
            }
            Err(e) => {
                let why = format!("{e:#}");
                log::warn!("audio engine still not ready: {why}");
                Some(Some(why))
            }
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
        self.icon_clock += 1;
        let clock = self.icon_clock;
        let keys: Vec<String> = apps
            .iter()
            .map(|a| a.bundle_id.as_str())
            .chain(configured)
            .filter(|k| self.icon_used.insert((*k).to_string(), clock).is_none())
            .map(str::to_string)
            .collect();
        keys.into_iter()
            .filter_map(|k| Some((k.clone(), Arc::new((self.resolve_icon)(&k)?))))
            .collect()
    }

    /// Drops the icons least recently seen running or configured beyond [`ICON_CAP`] (they resolve
    /// again if the app comes back), and forgets lookups that missed for keys no longer in use.
    fn trim_icons(&mut self, icons: &mut HashMap<String, Arc<ColorImage>>) {
        while icons.len() > ICON_CAP {
            let oldest = icons
                .keys()
                .min_by_key(|k| self.icon_used.get(*k).copied().unwrap_or(0))
                .cloned();
            let Some(k) = oldest else { break };
            icons.remove(&k);
            self.icon_used.remove(&k);
        }
        let clock = self.icon_clock;
        self.icon_used
            .retain(|k, &mut t| t == clock || icons.contains_key(k));
    }
}

impl Worker {
    pub fn spawn(backend: Box<dyn Backend + Send>, ctx: Context) -> Worker {
        Self::spawn_with(backend, ctx, Box::new(icons::resolve))
    }

    /// The first start failed (`failed` says why): the worker runs on a silent stand-in, and calls
    /// `start` after 1 s, 2 s, 4 s, then every 10 s until the engine runs. The window's banner
    /// follows `Snapshot::engine_error`.
    pub fn spawn_retrying(failed: String, start: EngineStart, ctx: Context) -> Worker {
        log::warn!("audio engine did not start: {failed}");
        let retry = (failed, start);
        let clock: Clock = Arc::new(Instant::now);
        Self::spawn_on(
            Box::new(FakeBackend::silent()),
            ctx,
            Box::new(icons::resolve),
            Some(retry),
            clock,
        )
    }

    fn core(
        backend: Box<dyn Backend + Send>,
        ctx: Context,
        resolve_icon: IconResolver,
        retry: Option<(String, EngineStart)>,
        clock: Clock,
    ) -> (Sender<Msg>, Arc<Mutex<Snapshot>>, Core) {
        let (tx, rx) = mpsc::channel::<Msg>();
        let now = clock();
        let snap = Arc::new(Mutex::new(Snapshot {
            engine_error: retry.as_ref().map(|(why, _)| why.clone()),
            ..Snapshot::default()
        }));
        let pump = Pump {
            backend,
            cfg: Config::default(),
            next_refresh: now,
            resolve_icon,
            icon_used: HashMap::new(),
            icon_clock: 0,
            visible: true,
            retry: retry.map(|(_, start)| Retry {
                start,
                failures: 1,
                next: now + retry_delay(1),
            }),
            applied: false,
            options: None,
        };
        let core = Core {
            pump,
            rx,
            snap: snap.clone(),
            ctx,
            visible: Arc::new(AtomicBool::new(true)),
            clock,
        };
        (tx, snap, core)
    }

    /// `spawn` with a custom icon source (tests use none, so snapshots keep letter tiles).
    pub fn spawn_with(
        backend: Box<dyn Backend + Send>,
        ctx: Context,
        resolve_icon: IconResolver,
    ) -> Worker {
        Self::spawn_on(backend, ctx, resolve_icon, None, Arc::new(Instant::now))
    }

    fn spawn_on(
        backend: Box<dyn Backend + Send>,
        ctx: Context,
        resolve_icon: IconResolver,
        retry: Option<(String, EngineStart)>,
        clock: Clock,
    ) -> Worker {
        let (tx, shared, mut core) = Self::core(backend, ctx, resolve_icon, retry, clock);
        let visible = core.visible.clone();
        let handle = Joinable::spawn(move || {
            loop {
                let first = match core.rx.recv_timeout(core.period()) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                core.iterate(first);
            }
        });
        Worker {
            tx: Some(tx),
            handle: Some(handle),
            shared,
            visible,
            #[cfg(test)]
            inline: None,
        }
    }

    /// `spawn_with` minus the thread (see `Worker::inline`). The first refresh is published before
    /// this returns; later ones repeat it, the fake backends being constant.
    #[cfg(test)]
    pub fn spawn_inline(
        backend: Box<dyn Backend + Send>,
        ctx: Context,
        resolve_icon: IconResolver,
    ) -> Worker {
        Self::inline_on(backend, ctx, resolve_icon, None, Arc::new(Instant::now))
    }

    /// `spawn_retrying` without the thread and with the clock the test moves.
    #[cfg(test)]
    pub fn spawn_inline_retrying(
        failed: String,
        start: EngineStart,
        ctx: Context,
        clock: Clock,
    ) -> Worker {
        let backend = Box::new(FakeBackend::silent());
        Self::inline_on(
            backend,
            ctx,
            Box::new(|_| None),
            Some((failed, start)),
            clock,
        )
    }

    #[cfg(test)]
    fn inline_on(
        backend: Box<dyn Backend + Send>,
        ctx: Context,
        resolve_icon: IconResolver,
        retry: Option<(String, EngineStart)>,
        clock: Clock,
    ) -> Worker {
        let (tx, shared, mut core) = Self::core(backend, ctx, resolve_icon, retry, clock);
        core.iterate(None);
        Worker {
            tx: Some(tx),
            handle: None,
            shared,
            visible: core.visible.clone(),
            inline: Some(std::cell::RefCell::new(core)),
        }
    }

    /// Tests: one worker wake-up now, as the thread's timeout would do.
    #[cfg(test)]
    pub fn wake(&self) {
        self.send(Msg::Wake);
    }

    /// New engine options (Settings). They reach the backend before the next config, and every
    /// engine the worker starts later.
    pub fn set_options(&self, o: &EngineOptions) {
        self.send(Msg::Options(o.clone()));
    }

    pub fn apply(&self, cfg: &Config) {
        sr_core::trace::stage("worker: apply message sent");
        self.send(Msg::Apply(cfg.clone()));
    }

    fn send(&self, m: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(m);
        }
        #[cfg(test)]
        if let Some(core) = &self.inline {
            core.borrow_mut().iterate(None);
        }
    }

    /// Loopback-test virtual device `id` off the UI thread; the result lands in `Snapshot::self_test`.
    pub fn self_test(&self, id: &str) {
        self.shared.lock().unwrap().self_test = SelfTestState::Running;
        self.send(Msg::SelfTest(id.to_string()));
    }

    /// Report a self-test that cannot even start.
    pub fn fail_self_test(&self, why: &str) {
        self.shared.lock().unwrap().self_test = SelfTestState::Done(Err(why.to_string()));
    }

    /// Time a click through virtual device `id` off the UI thread; the result lands in `Snapshot::measure`.
    pub fn measure_delay(&self, id: &str) {
        self.shared.lock().unwrap().measure = MeasureState::Running;
        self.send(Msg::MeasureDelay(id.to_string()));
    }

    /// Report a measurement that cannot even start.
    pub fn fail_measure_delay(&self, why: &str) {
        self.shared.lock().unwrap().measure = MeasureState::Done(Err(why.to_string()));
    }

    /// Tell the worker whether the window can be seen; hidden, it stops reading meters and sleeps
    /// longer between wake-ups. Cheap to call every frame.
    pub fn set_visible(&self, visible: bool) {
        if self.visible.swap(visible, Relaxed) != visible {
            self.send(Msg::Wake);
        }
    }

    /// Swap the real engine for the synthetic one (the setup sheet's "Try Demo").
    pub fn use_demo(&self) {
        self.send(Msg::Demo);
    }

    /// Stop the thread and wait for it (at most [`STOP_WAIT`]), so the backend (and its CoreAudio
    /// devices) is dropped before the process exits. Cmd-Q never drops `App`, so this is also
    /// called from `on_exit`.
    pub fn stop(&mut self) {
        self.tx = None;
        #[cfg(test)]
        {
            self.inline = None;
        }
        if let Some(j) = self.handle.take() {
            j.join_within(STOP_WAIT);
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
    use sr_core::backend::ReadAheadState;
    use sr_core::config::Config;

    #[derive(Default)]
    struct Log {
        applied: Vec<Config>,
        fail: bool,
        /// `apply` panics.
        panic: bool,
        /// Snapshot lock was free during every backend call.
        lock_free: bool,
        /// What the backend was asked, in order: "options" (with the read-ahead flag) and "apply".
        order: Vec<String>,
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
            let (fail, boom) = {
                let mut l = self.log.lock().unwrap();
                l.applied.push(cfg.clone());
                l.order.push("apply".into());
                (l.fail, l.panic)
            };
            // After the guard is gone: a panic under it would poison the log for the test.
            assert!(!boom, "engine bug");
            anyhow::ensure!(!fail, "tap denied");
            Ok(())
        }
        fn set_options(&mut self, o: &EngineOptions) -> anyhow::Result<()> {
            self.log
                .lock()
                .unwrap()
                .order
                .push(format!("options read_ahead={}", o.read_ahead));
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
                regular: true,
            }]
        }
        fn meters(&self, id: &str) -> Option<MeterReading> {
            self.probe();
            (id == "v1").then(MeterReading::default)
        }
        fn self_test(&mut self, id: &str) -> anyhow::Result<SelfTest> {
            self.probe();
            anyhow::ensure!(id == "v1", "no device {id}");
            Ok(SelfTest {
                rms_dbfs: -6.0,
                dropouts: 0,
                rate: 48_000.0,
                passed: true,
            })
        }
        fn measure_delay(&mut self, id: &str) -> anyhow::Result<f64> {
            self.probe();
            anyhow::ensure!(id == "v1", "no device {id}");
            Ok(5.4)
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
            icon_used: HashMap::new(),
            icon_clock: 0,
            visible: true,
            retry: None,
            applied: false,
            options: None,
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
            tx.send(Msg::Apply(c)).unwrap();
        }
        let first = rx.recv().ok();
        let batch = drain_latest(&rx, first);
        assert_eq!(batch.cfg.as_ref(), cfgs.last());
        let (mut pump, log, snap) = setup();
        pump.run(batch, Instant::now(), &snap);
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
    fn a_panicking_backend_is_reported_and_the_worker_keeps_serving() {
        // Before, the panic ended the thread: later configs went nowhere and the UI never knew.
        let log = Arc::new(Mutex::new(Log {
            lock_free: true,
            panic: true,
            ..Log::default()
        }));
        let rec_snap = Arc::new(Mutex::new(Snapshot::default()));
        let backend = Rec {
            log: log.clone(),
            snap: rec_snap,
        };
        let (tx, snap, mut core) = Worker::core(
            Box::new(backend),
            Context::default(),
            Box::new(|_| None),
            None,
            Arc::new(Instant::now),
        );
        snap.lock().unwrap().self_test = SelfTestState::Running;
        tx.send(Msg::Apply(default_config())).unwrap();
        core.iterate(None);
        let s = snap.lock().unwrap();
        let err = s.apply_error.clone().unwrap();
        assert!(err.contains("engine bug"), "{err}");
        assert!(
            matches!(s.self_test, SelfTestState::Done(Err(_))),
            "not stuck on Running"
        );
        drop(s);
        log.lock().unwrap().panic = false;
        tx.send(Msg::Apply(default_config())).unwrap();
        core.iterate(None);
        assert!(snap.lock().unwrap().apply_error.is_none());
        assert_eq!(log.lock().unwrap().applied.len(), 2);
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
    fn unchanged_meters_do_not_wake_the_ui_and_hidden_ticks_read_none() {
        let (mut pump, _log, snap) = setup();
        let t0 = Instant::now();
        assert!(pump.tick(Some(default_config()), t0, &snap)); // apply + first refresh + new meters
        assert!(!pump.tick(None, t0, &snap), "same silent meters");
        pump.visible = false;
        snap.lock().unwrap().meters.clear();
        assert!(!pump.tick(None, t0, &snap));
        assert!(snap.lock().unwrap().meters.is_empty(), "hidden: not read");
        pump.visible = true;
        assert!(pump.tick(None, t0, &snap), "shown again: meters are back");
        assert!(
            pump.tick(None, t0 + REFRESH_PERIOD, &snap),
            "a refresh always counts"
        );
    }

    #[test]
    fn the_worker_wakes_at_30_hz_only_with_a_window_and_a_device_to_meter() {
        let (_tx, _snap, mut core) = Worker::core(
            Box::new(FakeBackend::new()),
            Context::default(),
            Box::new(|_| None),
            None,
            Arc::new(Instant::now),
        );
        assert_eq!(core.period(), HIDDEN_PERIOD, "no device: nothing to meter");
        core.pump.cfg = default_config();
        assert!(core.pump.cfg.devices.iter().any(|d| d.enabled));
        assert_eq!(core.period(), METER_PERIOD);
        core.visible.store(false, Relaxed);
        assert_eq!(core.period(), HIDDEN_PERIOD, "hidden window");
        core.visible.store(true, Relaxed);
        core.pump
            .cfg
            .devices
            .iter_mut()
            .for_each(|d| d.enabled = false);
        assert_eq!(core.period(), HIDDEN_PERIOD, "every device off");
    }

    /// The click that switches a device on arrives while the worker waits out the slow hidden period:
    /// the message ends the wait, the apply and the first meter read share that wake-up, and the next
    /// wait is already the 30 Hz one (nothing sleeps through a 500 ms period after the click).
    #[test]
    fn switching_a_device_on_is_applied_and_metered_in_one_wakeup_and_the_next_is_fast() {
        let (_tx, snap, mut core) = Worker::core(
            Box::new(FakeBackend::new()),
            Context::default(),
            Box::new(|_| None),
            None,
            Arc::new(Instant::now),
        );
        let mut off = default_config();
        off.devices.iter_mut().for_each(|d| d.enabled = false);
        core.iterate(Some(Msg::Apply(off)));
        assert_eq!(core.period(), HIDDEN_PERIOD);
        assert!(snap.lock().unwrap().meters.is_empty());
        core.iterate(Some(Msg::Apply(default_config())));
        assert_eq!(core.period(), METER_PERIOD);
        assert!(!snap.lock().unwrap().meters.is_empty(), "metered at once");
    }

    #[test]
    fn only_edits_that_start_a_device_pull_the_status_refresh_in() {
        let on = default_config();
        let mut muted = on.clone();
        muted.devices[0].sources[0].muted = true;
        muted.devices[0].volume_db = -6.0;
        assert!(
            !starts_something(&on, &muted),
            "a mute or volume edit starts nothing"
        );
        let mut off = on.clone();
        off.devices[0].enabled = false;
        assert!(starts_something(&off, &on), "switched on");
        assert!(!starts_something(&on, &off), "switched off");
        let mut wider = on.clone();
        wider.devices[0].channels = 4;
        assert!(starts_something(&on, &wider));
        let mut less = on.clone();
        less.devices[0].sources.clear();
        assert!(starts_something(&on, &less));
        assert!(starts_something(&Config::default(), &on), "a new device");
        let (mut pump, _log, snap) = setup();
        let t0 = Instant::now();
        pump.tick(Some(on.clone()), t0, &snap);
        assert!(pump.next_refresh <= Instant::now() + AFTER_START_REFRESH);
        pump.tick(None, t0, &snap);
        pump.next_refresh = t0 + REFRESH_PERIOD;
        pump.tick(Some(muted), t0, &snap);
        assert_eq!(
            pump.next_refresh,
            t0 + REFRESH_PERIOD,
            "a mute leaves it alone"
        );
    }

    #[test]
    fn set_visible_wakes_the_thread_only_on_change() {
        let (tx, rx) = mpsc::channel();
        let w = Worker {
            tx: Some(tx),
            handle: None,
            shared: Arc::default(),
            visible: Arc::new(AtomicBool::new(true)),
            inline: None,
        };
        w.set_visible(true);
        assert!(rx.try_recv().is_err());
        w.set_visible(false);
        assert!(matches!(rx.try_recv(), Ok(Msg::Wake)));
        w.set_visible(false);
        assert!(rx.try_recv().is_err());
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

    #[test]
    fn the_snapshot_keeps_at_most_64_icons_and_drops_the_least_recently_seen() {
        let (mut pump, _log, snap) = setup();
        let asked = Arc::new(Mutex::new(0usize));
        let count = asked.clone();
        pump.resolve_icon = Box::new(move |_| {
            *count.lock().unwrap() += 1;
            Some(ColorImage::filled([1, 1], Color32::RED))
        });
        let seen = |pump: &mut Pump, range: std::ops::Range<usize>| {
            let apps: Vec<AppInfo> = range
                .map(|i| AppInfo {
                    bundle_id: format!("path:/bin/x{i}"),
                    name: String::new(),
                    running_output: true,
                    regular: false,
                })
                .collect();
            let icons = pump.new_icons(&apps);
            let mut s = snap.lock().unwrap();
            s.icons.extend(icons);
            pump.trim_icons(&mut s.icons);
            s.icons.len()
        };
        let has = |i: usize| {
            snap.lock()
                .unwrap()
                .icons
                .contains_key(&format!("path:/bin/x{i}"))
        };
        assert_eq!(seen(&mut pump, 0..30), 30);
        assert_eq!(seen(&mut pump, 30..64), 64);
        // 36 more executables: the 30 seen longest ago go, then 6 of the next oldest.
        assert_eq!(seen(&mut pump, 64..100), 64);
        assert!((0..30).all(|i| !has(i)));
        assert!((64..100).all(has), "what is running now stays");
        assert_eq!(pump.icon_used.len(), 64, "lookups go with their icons");
        // A dropped one resolves again when its app returns; a kept one does not.
        let before = *asked.lock().unwrap();
        assert_eq!(seen(&mut pump, 0..1), 64);
        assert_eq!(*asked.lock().unwrap(), before + 1);
        assert!(has(0));
        assert_eq!(seen(&mut pump, 99..100), 64);
        assert_eq!(*asked.lock().unwrap(), before + 1);
    }

    #[test]
    fn self_test_result_reaches_the_snapshot_and_requests_collapse() {
        let (tx, rx) = mpsc::channel();
        for id in ["v1", "v1", "nope"] {
            tx.send(Msg::SelfTest(id.into())).unwrap();
        }
        let batch = drain_latest(&rx, None);
        assert_eq!(batch.tests, ["v1", "nope"]);
        let (mut pump, log, snap) = setup();
        pump.run(batch, Instant::now(), &snap);
        // the last request (unknown device) is the one left behind
        assert_eq!(
            snap.lock().unwrap().self_test,
            SelfTestState::Done(Err("no device nope".into()))
        );
        pump.run(
            Batch {
                tests: vec!["v1".into()],
                ..Batch::default()
            },
            Instant::now(),
            &snap,
        );
        let SelfTestState::Done(Ok(t)) = snap.lock().unwrap().self_test.clone() else {
            panic!("no result");
        };
        assert!(t.passed);
        assert!(log.lock().unwrap().lock_free);
    }

    #[test]
    fn measured_delay_reaches_the_snapshot_and_requests_collapse() {
        let (tx, rx) = mpsc::channel();
        for id in ["v1", "v1", "nope"] {
            tx.send(Msg::MeasureDelay(id.into())).unwrap();
        }
        let batch = drain_latest(&rx, None);
        assert_eq!(batch.measures, ["v1", "nope"]);
        let (mut pump, log, snap) = setup();
        pump.run(batch, Instant::now(), &snap);
        assert_eq!(
            snap.lock().unwrap().measure,
            MeasureState::Done(Err("no device nope".into()))
        );
        pump.run(
            Batch {
                measures: vec!["v1".into()],
                ..Batch::default()
            },
            Instant::now(),
            &snap,
        );
        assert_eq!(snap.lock().unwrap().measure, MeasureState::Done(Ok(5.4)));
        assert!(log.lock().unwrap().lock_free);
    }

    #[test]
    fn demo_message_swaps_in_the_synthetic_backend() {
        let (mut pump, _log, snap) = setup();
        let now = Instant::now();
        pump.run(
            Batch {
                demo: true,
                ..Batch::default()
            },
            now,
            &snap,
        );
        let s = snap.lock().unwrap();
        assert!(s.status_ready);
        assert_eq!(
            s.status.driver_version.as_deref(),
            Some(crate::fake::DEMO_DRIVER)
        );
    }

    fn read_ahead(on: bool) -> EngineOptions {
        EngineOptions {
            read_ahead: on,
            ..EngineOptions::default()
        }
    }

    #[test]
    fn a_burst_of_options_collapses_to_the_latest() {
        let (tx, rx) = mpsc::channel();
        for on in [true, false, true, false] {
            tx.send(Msg::Options(read_ahead(on))).unwrap();
        }
        tx.send(Msg::Apply(default_config())).unwrap();
        let first = rx.recv().ok();
        let batch = drain_latest(&rx, first);
        assert_eq!(batch.options, Some(read_ahead(false)));
        assert!(batch.cfg.is_some());
        let (mut pump, log, snap) = setup();
        pump.run(batch, Instant::now(), &snap);
        assert_eq!(
            log.lock().unwrap().order,
            ["options read_ahead=false", "apply"]
        );
    }

    #[test]
    fn options_reach_the_backend_before_the_config_of_the_same_batch() {
        let (mut pump, log, snap) = setup();
        pump.run(
            Batch {
                cfg: Some(default_config()),
                options: Some(read_ahead(true)),
                ..Batch::default()
            },
            Instant::now(),
            &snap,
        );
        assert_eq!(
            log.lock().unwrap().order,
            ["options read_ahead=true", "apply"]
        );
        // A later config alone does not repeat them.
        pump.run(
            Batch {
                cfg: Some(default_config()),
                ..Batch::default()
            },
            Instant::now(),
            &snap,
        );
        assert_eq!(log.lock().unwrap().order.len(), 3);
    }

    #[test]
    fn an_engine_that_starts_late_gets_the_options_before_its_first_config() {
        let (mut pump, first, snap) = setup();
        let second = Arc::new(Mutex::new(Log {
            lock_free: true,
            ..Log::default()
        }));
        let engine = Rec {
            log: second.clone(),
            snap: snap.clone(),
        };
        let (start, _) = fails_then_starts(1, engine);
        let t0 = Instant::now();
        pump.retry = Some(Retry {
            start,
            failures: 1,
            next: t0 + secs(1),
        });
        pump.run(
            Batch {
                cfg: Some(default_config()),
                options: Some(read_ahead(true)),
                ..Batch::default()
            },
            t0,
            &snap,
        );
        assert_eq!(
            first.lock().unwrap().order,
            ["options read_ahead=true", "apply"]
        );
        // The first attempt fails, the second starts the engine: options, then the config.
        pump.tick(None, t0 + secs(1), &snap);
        pump.tick(None, t0 + secs(3), &snap);
        assert!(pump.retry.is_none());
        assert_eq!(
            second.lock().unwrap().order,
            ["options read_ahead=true", "apply"]
        );
    }

    #[test]
    fn the_demo_backend_gets_the_options_too() {
        let (mut pump, _log, snap) = setup();
        let now = Instant::now();
        pump.run(
            Batch {
                options: Some(read_ahead(true)),
                ..Batch::default()
            },
            now,
            &snap,
        );
        pump.run(
            Batch {
                demo: true,
                cfg: Some(crate::fake::demo_config()),
                ..Batch::default()
            },
            now,
            &snap,
        );
        // `FakeBackend` reports what `set_options` gave it.
        assert_eq!(snap.lock().unwrap().status.read_ahead, ReadAheadState::On);
    }

    /// The one test of the real thread. `stop` joins it, and a hung-up channel still yields what
    /// was queued, so no wait for a reply is needed.
    #[test]
    fn the_thread_answers_everything_queued_before_it_is_stopped() {
        let backend = Box::new(FakeBackend::frozen(1.5));
        let mut w = Worker::spawn_with(backend, Context::default(), Box::new(|_| None));
        w.apply(&crate::fake::demo_config());
        w.self_test("v1");
        w.stop();
        let s = w.shared.lock().unwrap();
        assert!(s.status_ready);
        assert_eq!(s.meters.len(), 2); // the demo config's two enabled devices
        assert!(matches!(s.self_test, SelfTestState::Done(Ok(_))));
    }

    /// `Rec` for an engine that starts on attempt `n + 1`, and how many times it was asked.
    fn fails_then_starts(n: u32, engine: Rec) -> (EngineStart, Arc<Mutex<u32>>) {
        let tries = Arc::new(Mutex::new(0));
        let count = tries.clone();
        let mut engine = Some(engine);
        let start: EngineStart = Box::new(move || {
            let mut c = count.lock().unwrap();
            *c += 1;
            anyhow::ensure!(*c > n, "coreaudiod not ready ({c})");
            Ok(Box::new(engine.take().expect("started twice")) as Box<dyn Backend + Send>)
        });
        (start, tries)
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn start_attempts_wait_1_2_4_then_10_seconds() {
        let waits: Vec<_> = (0..8).map(retry_delay).collect();
        assert_eq!(
            waits,
            [
                secs(1),
                secs(1),
                secs(2),
                secs(4),
                secs(10),
                secs(10),
                secs(10),
                secs(10)
            ]
        );
        assert_eq!(retry_delay(u32::MAX), secs(10));
    }

    #[test]
    fn a_late_engine_is_retried_on_the_schedule_and_gets_the_config() {
        let (mut pump, first, snap) = setup();
        let second = Arc::new(Mutex::new(Log {
            lock_free: true,
            ..Log::default()
        }));
        let engine = Rec {
            log: second.clone(),
            snap: snap.clone(),
        };
        let (start, tries) = fails_then_starts(3, engine);
        let t0 = Instant::now();
        pump.retry = Some(Retry {
            start,
            failures: 1,
            next: t0 + secs(1),
        });
        snap.lock().unwrap().engine_error = Some("no CoreAudio".into());
        // The window's first config lands on the stand-in (the first `setup` backend).
        pump.tick(Some(default_config()), t0, &snap);
        assert_eq!(first.lock().unwrap().applied.len(), 1);

        // (time of the tick, attempts made so far, banner afterwards)
        let steps = [
            (900, 0, Some("no CoreAudio")),
            (1000, 1, Some("coreaudiod not ready (1)")), // next in 2 s
            (2999, 1, Some("coreaudiod not ready (1)")),
            (3000, 2, Some("coreaudiod not ready (2)")), // next in 4 s
            (6999, 2, Some("coreaudiod not ready (2)")),
            (7000, 3, Some("coreaudiod not ready (3)")), // next in 10 s
            (16_999, 3, Some("coreaudiod not ready (3)")),
            (17_000, 4, None),
            (60_000, 4, None), // running: never asked again
        ];
        for (ms, attempts, banner) in steps {
            pump.tick(None, t0 + Duration::from_millis(ms), &snap);
            assert_eq!(*tries.lock().unwrap(), attempts, "at {ms} ms");
            assert_eq!(
                snap.lock().unwrap().engine_error.as_deref(),
                banner,
                "at {ms} ms"
            );
        }
        assert!(pump.retry.is_none());
        // The new engine got the last config at once; the stand-in saw no more.
        assert_eq!(second.lock().unwrap().applied, [default_config()]);
        assert_eq!(first.lock().unwrap().applied.len(), 1);
        assert!(snap.lock().unwrap().apply_error.is_none());
    }

    #[test]
    fn a_failed_retry_with_the_same_reason_does_not_wake_the_ui_but_a_start_does() {
        let (mut pump, _log, snap) = setup();
        let engine = Rec {
            log: Arc::default(),
            snap: snap.clone(),
        };
        let (start, _) = fails_then_starts(1, engine);
        let t0 = Instant::now();
        pump.retry = Some(Retry {
            start,
            failures: 1,
            next: t0 + secs(1),
        });
        snap.lock().unwrap().engine_error = Some("coreaudiod not ready (1)".into());
        pump.visible = false; // no meters, so only the retry can change anything
        assert!(pump.tick(None, t0, &snap)); // first refresh
        pump.next_refresh = t0 + secs(3600); // refreshes always count; keep them out of this
        assert!(
            !pump.tick(None, t0 + secs(1), &snap),
            "same reason as the banner"
        );
        assert!(pump.tick(None, t0 + secs(3), &snap), "the engine started");
        assert!(snap.lock().unwrap().engine_error.is_none());
    }

    #[test]
    fn an_engine_that_starts_before_any_config_is_not_given_the_empty_default() {
        let (mut pump, _log, snap) = setup();
        let log = Arc::new(Mutex::new(Log::default()));
        let engine = Rec {
            log: log.clone(),
            snap: snap.clone(),
        };
        let (start, _) = fails_then_starts(0, engine);
        let t0 = Instant::now();
        pump.retry = Some(Retry {
            start,
            failures: 1,
            next: t0,
        });
        pump.tick(None, t0, &snap);
        assert!(pump.retry.is_none());
        assert!(log.lock().unwrap().applied.is_empty());
    }

    #[test]
    fn the_demo_ends_the_retries_and_the_banner() {
        let (mut pump, _log, snap) = setup();
        let (start, tries) = fails_then_starts(
            u32::MAX,
            Rec {
                log: Arc::default(),
                snap: snap.clone(),
            },
        );
        let t0 = Instant::now();
        pump.retry = Some(Retry {
            start,
            failures: 1,
            next: t0 + secs(1),
        });
        snap.lock().unwrap().engine_error = Some("no CoreAudio".into());
        let demo = Batch {
            demo: true,
            ..Batch::default()
        };
        pump.run(demo, t0, &snap);
        assert!(snap.lock().unwrap().engine_error.is_none());
        pump.tick(None, t0 + secs(60), &snap);
        assert_eq!(*tries.lock().unwrap(), 0, "the fake engine is not replaced");
    }

    #[test]
    fn the_worker_retries_on_its_own_clock_and_clears_the_banner() {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock: Clock = {
            let now = now.clone();
            Arc::new(move || *now.lock().unwrap())
        };
        let engine = Rec {
            log: Arc::default(),
            snap: Arc::default(),
        };
        let (start, tries) = fails_then_starts(1, engine);
        let (_tx, snap, mut core) = Worker::core(
            Box::new(FakeBackend::silent()),
            Context::default(),
            Box::new(|_| None),
            Some(("no CoreAudio".into(), start)),
            clock,
        );
        assert_eq!(
            snap.lock().unwrap().engine_error.as_deref(),
            Some("no CoreAudio")
        );
        core.iterate(None);
        assert_eq!(*tries.lock().unwrap(), 0, "not due at once");
        for (advance, attempts, running) in [(1, 1, false), (1, 1, false), (1, 2, true)] {
            *now.lock().unwrap() += secs(advance);
            core.iterate(None);
            assert_eq!(*tries.lock().unwrap(), attempts);
            assert_eq!(snap.lock().unwrap().engine_error.is_none(), running);
        }
    }

    #[test]
    fn a_start_that_panics_is_not_retried_at_every_wake_up() {
        let t0 = Instant::now();
        let calls = Arc::new(Mutex::new(0));
        let count = calls.clone();
        let start: EngineStart = Box::new(move || {
            *count.lock().unwrap() += 1;
            panic!("HAL aborted");
        });
        let (mut pump, _log, snap) = setup();
        pump.retry = Some(Retry {
            start,
            failures: 1,
            next: t0,
        });
        let run = |pump: &mut Pump, at: Instant| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pump.tick(None, at, &snap);
            }))
        };
        assert!(run(&mut pump, t0).is_err());
        assert!(
            run(&mut pump, t0 + Duration::from_millis(33)).is_ok(),
            "waits"
        );
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn a_worker_that_ends_is_joined() {
        assert!(Joinable::spawn(|| {}).join_within(secs(3600)));
        // A panic ends the thread as well; it is not mistaken for a hang.
        assert!(Joinable::spawn(|| panic!("worker bug")).join_within(secs(3600)));
    }

    #[test]
    fn a_worker_stuck_in_a_call_does_not_hold_up_quitting() {
        let (release, blocked) = mpsc::channel::<()>();
        let (ended, ended_rx) = mpsc::channel::<()>();
        let j = Joinable::spawn(move || {
            let _ = blocked.recv(); // the hung HAL call
            let _ = ended.send(());
        });
        // Zero wait: the thread cannot have ended, so this is the timeout path, with no sleeping.
        assert!(!j.join_within(Duration::ZERO));
        // Let it go: the thread was only detached, and still finishes.
        release.send(()).unwrap();
        ended_rx.recv().unwrap();
    }
}
