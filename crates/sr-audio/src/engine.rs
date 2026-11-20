//! The routing engine: one private aggregate + IOProc per enabled virtual device.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{
    AtomicU32, AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_audio::*;
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioTimeStamp, AudioTimeStampFlags, AudioValueRange,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSNumber, NSString};
use sr_core::backend::{
    AppInfo, Backend, Delay, DeviceState, EngineStatus, HwDevice, MeterReading, SelfTest, TccStatus,
};
use sr_core::config::{Config, MAX_CH, MAX_FRAMES, SourceKind, VirtualDevice};
use sr_core::driver_proto::{COMPANION_SUFFIX, DEVICE_UID_PREFIX, DriverDevice, companion_uid};
use sr_core::meters::Meters;
use sr_core::mixer::{IoLayout, MixPlan, Mixer, MixerHandle, mixer};
use sr_core::selftest;

use crate::activity::{self, Activity, Lease};
use crate::apps::{self, AppGroup};
use crate::driver;
use crate::hal::{self, SYSTEM};
use crate::health::{self, SilenceWatch};
use crate::layout::{self, LayoutSpec, SCRATCH_LEN, SubDesc};
use crate::power::PowerWatch;
use crate::tapring::{TapCounters, TapIn, TapInHandle, TapRx, TapStats, tap_in};
use sr_core::backend::HogInfo;
use tapagg::{TapAgg, TapAggSpec, retire_tap, tap_split_enabled};

/// If the hidden companion cannot be found or joined, run on the visible vdev instead.
/// shortcut: then the Pass-Thru source is off (see `LayoutInputs::passthru`); the companion path is verified live (docs/TESTING.md), this fallback is not.
pub const ALLOW_VISIBLE_FALLBACK: bool = true;
pub const AGG_UID_PREFIX: &str = "soundrouter.agg.";
static AGG_SEQ: AtomicU64 = AtomicU64::new(0);

/// State of a vdev's IOProc start (AudioDeviceStart blocks while the TCC prompt is open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoState {
    Starting,
    Running,
    Failed(i32),
}

fn io_state_of(shared: &Shared) -> IoState {
    match shared.state.load(Relaxed) {
        0 => IoState::Starting,
        1 => IoState::Running,
        s => IoState::Failed(s as u32 as i32),
    }
}

/// A start that is still blocked without TCC capture permission is the permission prompt.
fn device_state(io: IoState, tcc: TccStatus) -> DeviceState {
    match io {
        IoState::Starting if tcc != TccStatus::Granted => DeviceState::WaitingForPermission,
        IoState::Starting => DeviceState::Starting,
        IoState::Running => DeviceState::Running,
        IoState::Failed(c) => DeviceState::Failed(format!("IO start failed (OSStatus {c})")),
    }
}

/// The state of a split vdev: the main aggregate never waits on the permission prompt (it has no taps),
/// the tap aggregate's start does, and only that holds back the tapped apps.
fn split_state(main: IoState, tap: Option<IoState>, tcc: TccStatus) -> DeviceState {
    match (main, tap) {
        (m @ IoState::Failed(_), _) | (_, Some(m @ IoState::Failed(_))) => device_state(m, tcc),
        (IoState::Starting, _) => DeviceState::Starting,
        (IoState::Running, Some(IoState::Starting)) => device_state(IoState::Starting, tcc),
        (IoState::Running, _) => DeviceState::Running,
    }
}

fn vdev_state(v: &Vdev, tcc: TccStatus) -> DeviceState {
    let main = io_state_of(&v.shared);
    if v.spec.split {
        split_state(main, v.tapagg.as_ref().map(|t| io_state_of(&t.shared)), tcc)
    } else {
        device_state(main, tcc)
    }
}

struct Shared {
    /// Expected `layout::shape_key` of the buffer lists; 0 = never matches (silence).
    /// Release-stored after the matching plan is published and Acquire-loaded by the IOProc, so a new key
    /// is never paired with an older plan.
    expected: AtomicU64,
    /// 0 starting, 1 running, `1<<32 | status` failed.
    state: AtomicU64,
    /// In->out distance in frames of the latest cycle; [`NO_DELAY`] before the first.
    delay: AtomicU32,
    /// Output time minus the current time of the latest cycle: how far ahead of the clock the output
    /// lies; [`NO_DELAY`] before the first.
    lead: AtomicU32,
}

const NO_DELAY: u32 = u32::MAX;

/// Frames from an input sample's time to the output sample time of the same cycle; `None` for a
/// distance that is negative, not finite or too large to be a real one.
fn in_out_distance(input_t: f64, output_t: f64) -> Option<u32> {
    let d = output_t - input_t;
    (d.is_finite() && (0.0..f64::from(NO_DELAY)).contains(&d)).then(|| d.round() as u32)
}

/// Audio-thread state; owned via raw pointer handed to the IOProc.
struct Cb {
    mixer: Mixer,
    shared: Arc<Shared>,
    meters: Arc<Meters>,
    in_planar: Scratch,
    out_planar: Scratch,
    /// Sample time the next cycle should start at; NaN before the first cycle.
    next_t: f64,
    /// Tapped audio from the tap aggregate (split vdevs); no channels without one.
    tap: TapIn,
}

impl Cb {
    /// A cycle starting more than half a buffer away from where the last one ended means the HAL
    /// skipped (or repeated) audio. The first cycle (NaN) compares false. Audio thread: no allocation.
    fn track_time(&mut self, t: f64, frames: usize) {
        if (t - self.next_t).abs() > frames as f64 / 2.0 {
            self.meters.glitch_count.fetch_add(1, Relaxed);
        }
        self.next_t = t + frames as f64;
    }

    /// Publishes this cycle's in->out distance and the output's lead over the clock. Audio thread: relaxed stores.
    fn track_delay(&self, now: &AudioTimeStamp, input: &AudioTimeStamp, output: &AudioTimeStamp) {
        let valid = AudioTimeStampFlags::SampleTimeValid;
        if output.mFlags.contains(valid)
            && now.mFlags.contains(valid)
            && let Some(d) = in_out_distance(now.mSampleTime, output.mSampleTime)
        {
            self.shared.lead.store(d, Relaxed);
        }
        if input.mFlags.contains(valid)
            && output.mFlags.contains(valid)
            && let Some(d) = in_out_distance(input.mSampleTime, output.mSampleTime)
        {
            self.shared.delay.store(d, Relaxed);
        }
    }
}

/// `SCRATCH_LEN` zeroed floats mapped straight from the kernel and unmapped on drop. As a `Vec` (1 MiB,
/// malloc's medium zone) freed scratch stayed dirty in ever more malloc regions: RSS grew about
/// 0.4 MiB per vdev rebuild (tests/churn.rs).
struct Scratch(NonNull<f32>);
// SAFETY: owns its mapping exclusively, like a `Box<[f32]>`.
unsafe impl Send for Scratch {}

impl Scratch {
    const BYTES: usize = SCRATCH_LEN * size_of::<f32>();

    fn new() -> Scratch {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Self::BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            std::alloc::handle_alloc_error(std::alloc::Layout::array::<f32>(SCRATCH_LEN).unwrap());
        }
        Scratch(NonNull::new(p.cast()).expect("mmap never maps address 0"))
    }
}

impl std::ops::Deref for Scratch {
    type Target = [f32];
    fn deref(&self) -> &[f32] {
        unsafe { std::slice::from_raw_parts(self.0.as_ptr(), SCRATCH_LEN) }
    }
}

impl std::ops::DerefMut for Scratch {
    fn deref_mut(&mut self) -> &mut [f32] {
        unsafe { std::slice::from_raw_parts_mut(self.0.as_ptr(), SCRATCH_LEN) }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.0.as_ptr().cast(), Self::BYTES) };
    }
}

struct Tap {
    source_id: String,
    bundle: String,
    id: u32,
    uid: String,
    objs: Vec<u32>,
    /// Part of the reuse key: flipping it rebuilds the tap (`CATapMuteBehavior` is fixed at creation).
    mute: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct AggSpec {
    main: String,
    subs: Vec<String>,
    channels: u16,
    /// Requested IO buffer size (`Config::buffer_frames`); part of the reuse key.
    buffer_frames: u32,
    /// One aggregate for the process taps and another for everything else; part of the reuse key.
    split: bool,
    /// Devices whose input the IOProc reads (see [`input_readers`]); the input streams of every other
    /// sub-device are switched off. Part of the reuse key.
    readers: Vec<String>,
}

struct Vdev {
    dev: VirtualDevice,
    spec: AggSpec,
    /// Better main candidates that failed to build when this one was built; see [`keep_in_place`].
    passed_over: Vec<String>,
    agg: u32,
    proc_id: AudioDeviceIOProcID,
    cb: *mut Cb,
    shared: Arc<Shared>,
    handle: MixerHandle,
    meters: Arc<Meters>,
    layout: sr_core::mixer::IoLayout,
    subs: Vec<SubDesc>,
    taps: Vec<Tap>,
    /// The tap-only aggregate of a split vdev; `None` without taps (and unsplit, where `taps` sit in `agg`).
    tapagg: Option<TapAgg>,
    /// Publishes the tap ring's receiver to the IOProc.
    tap_in: TapInHandle,
    /// Tap path counters, cumulative over tap aggregate rebuilds.
    tap_stats: Arc<TapStats>,
    /// Where the tap aggregate reports its IOProc start; `None` in tests.
    events: Option<Sender<Event>>,
    activity: Arc<Activity>,
    /// Removed taps the aggregate may still reference; destroyed after its tap list is updated (F7).
    zombies: Vec<Tap>,
    /// The aggregate's tap list (or layout) may be out of date: the next sync must redo it (F7).
    tap_dirty: bool,
    /// Non-fatal config problems found by the last layout refresh, see [`Engine::warnings`].
    warnings: Vec<String>,
    /// The `AudioDeviceStart` thread; `None` once joined.
    starter: Option<JoinHandle<()>>,
    /// Build number (the aggregate uid suffix); ignores [`Event::IoStart`] of a retired build.
    build: u64,
    /// Nominal rate of the main and sub devices at build / start; a rate event that leaves these
    /// unchanged is an echo of our own aggregate.
    rates: Vec<(String, f64)>,
    /// Rate listeners on the main and sub devices; removed with the vdev.
    _listeners: Vec<Listener>,
    /// Listeners on V's volume and mute controls, feeding the mixer's `SysGain`; empty without controls.
    _sysvol: Vec<Listener>,
    /// Keeps the process out of App Nap while this vdev's IOProc exists.
    _lease: Lease,
    /// Processor overloads the HAL reported for this vdev's devices and aggregates (both of them).
    xruns: Arc<AtomicU64>,
    /// Zero-fill watch of the tapped apps, advanced by the health thread.
    silence: SilenceWatch,
    /// The tapped apps play but the taps deliver only zeros (see [`SilenceWatch`]).
    tap_silent: bool,
    /// Times a foreign change of the aggregates' buffer size was set back; capped so a size the HAL
    /// refuses cannot loop.
    buffer_resets: u32,
}
// SAFETY: `cb` is only dereferenced by the IOProc thread; control-side access is limited to the atomics in `Shared`.
unsafe impl Send for Vdev {}

impl Vdev {
    /// True while a starter thread is still inside `AudioDeviceStart` (TCC prompt open).
    fn starting(&self) -> bool {
        self.starter.as_ref().is_some_and(|h| !h.is_finished())
            || self.tapagg.as_ref().is_some_and(TapAgg::starting)
    }

    /// Start state of the vdev: the main aggregate's, or `Failed` when the tap aggregate's start failed.
    fn io(&self) -> IoState {
        combine_io(
            io_state_of(&self.shared),
            self.tapagg.as_ref().map(|t| io_state_of(&t.shared)),
        )
    }
}

/// Failed if either start failed; else the main aggregate's state (the tap aggregate may still be
/// waiting on the permission prompt while everything else routes).
fn combine_io(main: IoState, tap: Option<IoState>) -> IoState {
    match tap {
        Some(f @ IoState::Failed(_)) if !matches!(main, IoState::Failed(_)) => f,
        _ => main,
    }
}

/// Teardown first waits for the starter threads: `AudioDeviceStart` may still be running on the
/// IOProc/aggregate we are about to destroy (F3). So **dropping a `Vdev` can block for as long as the TCC
/// prompt is open**; code on a caller's thread uses [`retire`] instead.
///
/// Order: tap IOProc stopped, main IOProc stopped, tap aggregate destroyed, main aggregate destroyed, then
/// the taps (nothing lists them any more).
impl Drop for Vdev {
    fn drop(&mut self) {
        if let Some(t) = self.tapagg.as_mut() {
            t.stop();
        }
        if let Some(h) = self.starter.take() {
            let _ = h.join();
        }
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceStop(self.agg, self.proc_id);
            }
        }
        self.tapagg = None;
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceDestroyIOProcID(self.agg, self.proc_id);
            }
            AudioHardwareDestroyAggregateDevice(self.agg);
            for t in self.taps.iter().chain(&self.zombies) {
                AudioHardwareDestroyProcessTap(t.id);
            }
            drop(Box::from_raw(self.cb));
        }
    }
}

/// Drop a vdev without blocking the caller: one still starting is torn down on a short-lived thread.
fn retire(v: Option<Vdev>) {
    let Some(v) = v else { return };
    if v.starting() {
        let _ = std::thread::Builder::new()
            .name("sr-audio-reap".into())
            .spawn(move || drop(v));
    }
}

#[derive(Default)]
struct Inner {
    cfg: Option<Config>,
    vdevs: HashMap<String, Vdev>,
    /// [`current_device_set`] as of the last apply; device events that leave it unchanged are echoes.
    dev_set: Vec<String>,
    /// App Nap insurance shared by all vdevs.
    activity: Arc<Activity>,
    bg_errors: BTreeMap<Op, String>,
    /// Where vdevs report their IOProc start; `None` in tests that never build one.
    tx: Option<Sender<Event>>,
    /// Vdevs torn down by a recovery and not yet rebuilt; shown as [`DeviceState::Starting`].
    recovering: BTreeSet<String>,
    /// Consecutive failed IOProc starts per vdev id, see [`io_retry`].
    io_fails: HashMap<String, u32>,
}

enum Event {
    Devices,
    Processes,
    /// The system woke from sleep.
    Wake,
    /// The nominal sample rate or the stream layout of the device with this uid changed.
    Rate(String),
    /// A vdev's `AudioDeviceStart` returned; `build` tells a stale build from the current one.
    IoStart {
        id: String,
        build: u64,
        ok: bool,
    },
    /// A device (uid) reported a change of `kAudioDevicePropertyDeviceIsAlive`.
    Dead(String),
    /// An aggregate of this vdev id changed its IO buffer size.
    Buffer(String),
    /// coreaudiod restarted (`kAudioHardwarePropertyServiceRestarted`): every object id is invalid.
    ServiceRestarted,
    /// The system default output device changed.
    DefaultOutput,
    Shutdown,
}

struct Listener {
    obj: u32,
    addr: AudioObjectPropertyAddress,
    block: RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)>,
}
// SAFETY: the block only sends on a channel; HAL calls it from its own threads anyway.
unsafe impl Send for Listener {}

impl Listener {
    /// Runs `f` on the HAL's thread whenever the property changes: keep it to a channel send or an
    /// atomic update.
    fn with(
        obj: u32,
        sel: u32,
        scope: u32,
        f: impl Fn() + Send + Sync + 'static,
    ) -> Result<Listener> {
        let addr = hal::addr(sel, scope);
        let block: RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)> =
            RcBlock::new(move |_n: u32, _a: NonNull<AudioObjectPropertyAddress>| f());
        let st = unsafe {
            AudioObjectAddPropertyListenerBlock(
                obj,
                NonNull::from(&addr),
                None,
                &*block as *const _ as *mut _,
            )
        };
        hal::check("AudioObjectAddPropertyListenerBlock", st)?;
        Ok(Listener { obj, addr, block })
    }

    /// Sends `ev()` to the worker on every change.
    fn new(
        obj: u32,
        sel: u32,
        scope: u32,
        tx: Sender<Event>,
        ev: impl Fn() -> Event + Send + Sync + 'static,
    ) -> Result<Listener> {
        let tx = Mutex::new(tx);
        Listener::with(obj, sel, scope, move || {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(ev());
            }
        })
    }

    /// Counts every change in `n` (a relaxed atomic add: safe wherever the HAL calls from).
    fn counter(obj: u32, sel: u32, n: Arc<AtomicU64>) -> Result<Listener> {
        Listener::with(obj, sel, kAudioObjectPropertyScopeGlobal, move || {
            n.fetch_add(1, Relaxed);
        })
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        unsafe {
            AudioObjectRemovePropertyListenerBlock(
                self.obj,
                NonNull::from(&self.addr),
                None,
                &*self.block as *const _ as *mut _,
            )
        };
    }
}

pub struct Engine {
    inner: Arc<Mutex<Inner>>,
    tx: Sender<Event>,
    worker: Option<JoinHandle<()>>,
    listeners: Vec<Listener>,
    /// `None` when the IOKit registration failed (logged); wake recovery is then off.
    power: Option<PowerWatch>,
    health: Option<HealthThread>,
}

/// How often the health thread looks at the vdevs.
const HEALTH_PERIOD: Duration = Duration::from_millis(500);

/// Advances the per-vdev silence watch twice a second (it needs history, so it cannot live in `status`).
struct HealthThread {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl HealthThread {
    fn start(inner: Arc<Mutex<Inner>>) -> Result<HealthThread> {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let s = stop.clone();
        let thread = std::thread::Builder::new()
            .name("sr-audio-health".into())
            .spawn(move || {
                let t0 = Instant::now();
                loop {
                    {
                        let (flag, cv) = &*s;
                        let g = flag.lock().unwrap_or_else(|e| e.into_inner());
                        let (g, _) = cv
                            .wait_timeout_while(g, HEALTH_PERIOD, |stop| !*stop)
                            .unwrap_or_else(|e| e.into_inner());
                        if *g {
                            return;
                        }
                    }
                    health_pass(&mut lock(&inner), t0.elapsed());
                }
            })?;
        Ok(HealthThread {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for HealthThread {
    fn drop(&mut self) {
        *self.stop.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.stop.1.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Whether a vdev's taps can deliver audio: its main aggregate runs and, when split, so does the tap
/// aggregate (before both run, silence is a start, not a denied tap).
fn taps_running(main: IoState, tap: Option<IoState>) -> bool {
    main == IoState::Running && tap.is_none_or(|t| t == IoState::Running)
}

/// One observation of every vdev's taps: is a tapped app playing, and did the taps deliver any signal
/// since the last pass (the mixer's input meters, drained apart from the window's).
fn health_pass(g: &mut Inner, now: Duration) {
    let tcc = crate::tcc::audio_capture_status().into();
    for (id, v) in g.vdevs.iter_mut() {
        let signal = v
            .taps
            .iter()
            .map(|t| v.meters.drain_source_peak(&v.layout, &t.source_id))
            .any(|p| p > 0.0);
        let running = taps_running(
            io_state_of(&v.shared),
            v.tapagg.as_ref().map(|t| io_state_of(&t.shared)),
        );
        let playing = v
            .taps
            .iter()
            .any(|t| t.objs.iter().any(|&o| health::process_running_output(o)));
        let silent = v
            .silence
            .observe(now, health::watch_taps(running && playing, tcc), signal);
        if silent != v.tap_silent {
            log::warn!(
                "device '{id}': tapped apps {} (zero-filled taps look like a denied System Audio Recording permission)",
                if silent {
                    "play but deliver only silence"
                } else {
                    "deliver audio again"
                }
            );
            v.tap_silent = silent;
        }
    }
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    pub fn new() -> Result<Engine> {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let (tx, rx) = channel();
        lock(&inner).tx = Some(tx.clone());
        let listeners = vec![
            Listener::new(
                SYSTEM,
                kAudioHardwarePropertyDevices,
                kAudioObjectPropertyScopeGlobal,
                tx.clone(),
                || Event::Devices,
            )?,
            Listener::new(
                SYSTEM,
                kAudioHardwarePropertyProcessObjectList,
                kAudioObjectPropertyScopeGlobal,
                tx.clone(),
                || Event::Processes,
            )?,
            Listener::new(
                SYSTEM,
                kAudioHardwarePropertyServiceRestarted,
                kAudioObjectPropertyScopeGlobal,
                tx.clone(),
                || Event::ServiceRestarted,
            )?,
            Listener::new(
                SYSTEM,
                kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal,
                tx.clone(),
                || Event::DefaultOutput,
            )?,
        ];
        let wake_tx = tx.clone();
        let power = PowerWatch::start(move || {
            let _ = wake_tx.send(Event::Wake);
        })
        .map_err(|e| log::warn!("no sleep/wake recovery: {e:#}"))
        .ok();
        let w = inner.clone();
        let worker = std::thread::Builder::new()
            .name("sr-audio-events".into())
            .spawn(move || worker(w, rx))?;
        let health = HealthThread::start(inner.clone())
            .map_err(|e| log::warn!("no health watch: {e:#}"))
            .ok();
        Ok(Engine {
            inner,
            tx,
            worker: Some(worker),
            listeners,
            power,
            health,
        })
    }

    pub fn io_state(&self, device_id: &str) -> Option<IoState> {
        Some(lock(&self.inner).vdevs.get(device_id)?.io())
    }

    /// Start state of a split vdev's tap aggregate; `None` without one (no taps running, or unsplit).
    pub fn tap_io_state(&self, device_id: &str) -> Option<IoState> {
        Some(io_state_of(
            &lock(&self.inner)
                .vdevs
                .get(device_id)?
                .tapagg
                .as_ref()?
                .shared,
        ))
    }

    /// Counters of a vdev's tap path: underruns, trims, dropped blocks and shape mismatches.
    pub fn tap_counters(&self, device_id: &str) -> Option<TapCounters> {
        Some(lock(&self.inner).vdevs.get(device_id)?.tap_stats.counters())
    }

    /// Blocks silenced by a buffer-shape mismatch (diagnostic).
    pub fn mismatches(&self, device_id: &str) -> u64 {
        lock(&self.inner)
            .vdevs
            .get(device_id)
            .map_or(0, |v| v.meters.mismatch_count())
    }

    /// IOProc cycles that arrived after the HAL skipped audio (overload, stalled thread; diagnostic).
    pub fn glitches(&self, device_id: &str) -> u64 {
        lock(&self.inner)
            .vdevs
            .get(device_id)
            .map_or(0, |v| v.meters.glitch_count())
    }

    /// The last background (event-driven) update failure, until a later update succeeds.
    /// Logged once at error level; this lets a UI show it as well.
    pub fn background_error(&self) -> Option<String> {
        background_message(&lock(&self.inner).bg_errors)
    }

    /// Configuration problems that do not stop routing, e.g. monitor wires beyond the hardware's
    /// channel count. Updated by every apply; each is also logged once when it first appears.
    pub fn warnings(&self) -> Vec<String> {
        let g = lock(&self.inner);
        let mut w: Vec<String> = g
            .vdevs
            .values()
            .flat_map(|v| v.warnings.iter().cloned())
            .collect();
        w.sort();
        w
    }
}

/// Nominal rate of `v`'s main device; `None` before it is known.
fn main_rate(v: &Vdev) -> Option<u32> {
    let rate = v.rates.iter().find(|(u, _)| *u == v.spec.main)?.1;
    (rate >= 1.0).then(|| rate.round() as u32)
}

/// The distance through `v`'s main aggregate (mic, pass-thru, monitors), once its IOProc has run a cycle.
/// In an unsplit vdev this is also the tapped apps' delay.
fn main_delay(v: &Vdev) -> Option<Delay> {
    let frames = v.shared.delay.load(Relaxed);
    (frames != NO_DELAY).then_some(Delay {
        frames,
        rate_hz: main_rate(v)?,
    })
}

/// The path of a split vdev's tapped apps, from the app's output to the mixer's: how stale the audio is
/// when the tap IOProc gets it, plus what waits in the ring, plus how far the main output lies ahead.
/// Measured per cycle; the wait for the main IOProc's phase (up to one buffer) is not in it.
fn tap_delay(v: &Vdev) -> Option<Delay> {
    let t = v.tapagg.as_ref()?;
    let (lag, lead) = (t.shared.delay.load(Relaxed), v.shared.lead.load(Relaxed));
    (lag != NO_DELAY && lead != NO_DELAY).then_some(Delay {
        frames: lag
            .saturating_add(v.tap_stats.level.load(Relaxed))
            .saturating_add(lead),
        rate_hz: main_rate(v)?,
    })
}

/// The longest path of `v`: what a user hears as the device's delay.
fn vdev_delay(v: &Vdev) -> Option<Delay> {
    [main_delay(v), tap_delay(v)]
        .into_iter()
        .flatten()
        .max_by_key(|d| d.frames)
}

impl Engine {
    /// The longest path through `id`'s engine: its main aggregate's in->out distance (the IOProc's
    /// `output - input` time) or, when apps are tapped, the app path; see [`Delay`].
    pub fn delay(&self, id: &str) -> Option<Delay> {
        vdev_delay(lock(&self.inner).vdevs.get(id)?)
    }

    /// The app (tap) path alone; `None` without a running tap aggregate.
    pub fn tap_delay(&self, id: &str) -> Option<Delay> {
        tap_delay(lock(&self.inner).vdevs.get(id)?)
    }

    /// The main aggregate's distance alone: mic, pass-thru and monitor paths.
    pub fn main_delay(&self, id: &str) -> Option<Delay> {
        main_delay(lock(&self.inner).vdevs.get(id)?)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.power = None;
        self.health = None;
        self.listeners.clear();
        let _ = self.tx.send(Event::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        for (_, v) in lock(&self.inner).vdevs.drain() {
            retire(Some(v));
        }
    }
}

/// Quiet period that batches HAL events before the worker reacts.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(200);
/// Upper bound of the debounce growth after repeated background failures.
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Quiet period after a system wake: displays, USB and Bluetooth audio come back late.
const WAKE_SETTLE: Duration = Duration::from_secs(1);

/// Debounce window of a batch: after a wake, wait for the hardware to reappear.
fn settle(wake: bool, fails: u32) -> Duration {
    if wake {
        WAKE_SETTLE.max(backoff(fails))
    } else {
        backoff(fails)
    }
}

/// Debounce window after `fails` consecutive background failures: doubles each time, capped.
fn backoff(fails: u32) -> Duration {
    (EVENT_DEBOUNCE * 2u32.saturating_pow(fails.min(8))).min(MAX_BACKOFF)
}

/// Sorted uids of the devices that are not our own private aggregates. Creating or destroying an
/// aggregate fires `kAudioHardwarePropertyDevices` in our own process; comparing this set tells such
/// echoes from real hot-plugs (F4).
fn uid_set(uids: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut v: Vec<String> = uids
        .into_iter()
        .filter(|u| !u.starts_with(AGG_UID_PREFIX))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn current_device_set() -> Vec<String> {
    uid_set(hal::all_devices().into_iter().map(hal::device_uid))
}

/// Background retries after a failure before the error is surfaced.
const MAX_RETRIES: u32 = 5;

/// Wait before retrying after the `fails`-th consecutive background failure; `None` = give up.
fn retry_delay(fails: u32) -> Option<Duration> {
    (1..=MAX_RETRIES)
        .contains(&fails)
        .then(|| backoff(fails - 1))
}

/// Which background operation failed; errors are tracked per operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Op {
    Apply,
    Retap,
}

fn background_message(errs: &BTreeMap<Op, String>) -> Option<String> {
    (!errs.is_empty()).then(|| errs.values().cloned().collect::<Vec<_>>().join("; "))
}

/// The worker's failure bookkeeping between passes.
#[derive(Default, Debug, PartialEq)]
struct Retry {
    /// Consecutive failed passes.
    fails: u32,
    /// Operation to rerun unconditionally (no dev_set filter) after a failure.
    op: Option<Op>,
}

impl Retry {
    /// How long to wait for an event before rerunning `op`; `None` = block until an event.
    fn wait(&self) -> Option<Duration> {
        self.op.and(retry_delay(self.fails))
    }

    /// What a pass runs for this news: device list changed, wake, vdevs to rebuild, processes
    /// changed. A full apply covers everything a retap does.
    fn choose(&self, devs: bool, wake: bool, rebuild: bool, procs: bool) -> Option<Op> {
        if devs || wake || rebuild || self.op == Some(Op::Apply) {
            Some(Op::Apply)
        } else if procs || self.op == Some(Op::Retap) {
            Some(Op::Retap)
        } else {
            None
        }
    }

    /// Books the outcome of `op`. Success clears its errors (an apply resolves everything). A failure
    /// schedules a retry; once the retries are spent the error is surfaced, once per distinct message,
    /// and nothing reruns until a new event.
    fn record(&mut self, op: Op, res: Result<()>, errs: &mut BTreeMap<Op, String>) {
        let e = match res {
            Ok(()) => {
                *self = Retry::default();
                if op == Op::Apply {
                    errs.clear();
                } else {
                    errs.remove(&Op::Retap);
                }
                return;
            }
            Err(e) => e,
        };
        self.fails = self.fails.saturating_add(1);
        let msg = format!("{e:#}");
        if retry_delay(self.fails).is_some() {
            self.op = Some(op);
            log::debug!(
                "background update failed (x{}), retrying: {msg}",
                self.fails
            );
            return;
        }
        self.op = None;
        if errs.get(&op) != Some(&msg) {
            log::error!("background update failed: {msg}");
            errs.insert(op, msg);
        }
    }
}

fn run_op(g: &mut Inner, op: Op) -> Result<()> {
    match op {
        Op::Apply => match g.cfg.clone() {
            Some(cfg) => apply_locked(g, &cfg, sync_driver),
            None => Ok(()),
        },
        Op::Retap => retap(g),
    }
}

/// What the worker has heard since its last pass.
#[derive(Default, Debug, PartialEq)]
struct Pending {
    devs: bool,
    procs: bool,
    wake: bool,
    /// Uids of devices that reported an IsAlive change.
    dead: BTreeSet<String>,
    /// Ids of vdevs whose aggregate changed its buffer size.
    buffers: BTreeSet<String>,
    /// Uids of devices whose nominal rate changed.
    rates: BTreeSet<String>,
    /// `(vdev id, build, started ok)` per finished `AudioDeviceStart`.
    io: Vec<(String, u64, bool)>,
}

impl Pending {
    /// Records `ev`; false for `Shutdown`.
    fn absorb(&mut self, ev: Event) -> bool {
        match ev {
            Event::Shutdown => return false,
            Event::Devices => self.devs = true,
            Event::Processes => self.procs = true,
            Event::Wake => self.wake = true,
            // Every object id went stale: rebuild everything, as after a wake.
            Event::ServiceRestarted => self.wake = true,
            // Re-reads the device list, which only acts when it really changed (status is read live).
            Event::DefaultOutput => self.devs = true,
            Event::Dead(uid) => {
                self.dead.insert(uid);
            }
            Event::Buffer(id) => {
                self.buffers.insert(id);
            }
            Event::Rate(uid) => {
                self.rates.insert(uid);
            }
            Event::IoStart { id, build, ok } => self.io.push((id, build, ok)),
        }
        true
    }
}

/// Nominal rate of the main and sub devices of `spec`, as the HAL reports them now.
fn read_rates(spec: &AggSpec) -> Vec<(String, f64)> {
    std::iter::once(&spec.main)
        .chain(&spec.subs)
        .filter_map(|u| Some((u.clone(), hal::nominal_rate(hal::device_by_uid(u)?))))
        .collect()
}

/// Baseline after a good IOProc start: the rates `now`, since starting may settle them (an echo, not a
/// change). A device with a rate event in the same batch (`changed`) keeps its `built` rate so the event is
/// still compared: else a change landing between build and start became the baseline and the vdev
/// (e.g. one monitoring that device) kept running on an aggregate built for the old rate.
fn start_baseline(
    built: &[(String, f64)],
    now: Vec<(String, f64)>,
    changed: &BTreeSet<String>,
) -> Vec<(String, f64)> {
    now.into_iter()
        .map(|(uid, r)| match built.iter().find(|(u, _)| *u == uid) {
            Some(&(_, was)) if changed.contains(&uid) => (uid, was),
            _ => (uid, r),
        })
        .collect()
}

/// Updates the consecutive start-failure counts (`ok` resets, each `failed` adds one) and returns
/// the failed ids that are still worth a rebuild. Without the cap a start that always fails would
/// rebuild forever, because every successful rebuild resets the background failure count.
fn io_retry(
    fails: &mut HashMap<String, u32>,
    failed: &[String],
    ok: &[String],
) -> BTreeSet<String> {
    for id in ok {
        fails.remove(id);
    }
    failed
        .iter()
        .filter(|id| {
            let n = fails.entry((*id).clone()).or_insert(0);
            *n += 1;
            retry_delay(*n).is_some()
        })
        .cloned()
        .collect()
}

/// Vdev ids to tear down and rebuild. `vdevs` is `(id, rates at build)`; `forced` ids always are (failed
/// start, new stream layout). A wake rebuilds all (the HAL may have replaced devices and streams); a rate
/// event rebuilds a vdev only when a device's rate really differs from the one it was built against, so
/// our own aggregate's echoes do not loop.
fn rebuild_targets(
    wake: bool,
    changed: &BTreeSet<String>,
    forced: &BTreeSet<String>,
    vdevs: &[(&str, &[(String, f64)])],
    rate_now: &dyn Fn(&str) -> f64,
) -> BTreeSet<String> {
    vdevs
        .iter()
        .filter(|(id, built)| {
            wake || forced.contains(*id)
                || built.iter().any(|(uid, was)| {
                    let now = rate_now(uid);
                    changed.contains(uid) && now > 0.0 && now != *was
                })
        })
        .map(|(id, _)| (*id).to_owned())
        .collect()
}

/// Vdev ids built on a device that reported a change of `IsAlive` and is no longer alive (or gone).
/// `vdevs` is `(id, rates at build)`: the uids the vdev's aggregate was built from.
fn dead_targets(
    dead: &BTreeSet<String>,
    vdevs: &[(&str, &[(String, f64)])],
    alive: &dyn Fn(&str) -> bool,
) -> BTreeSet<String> {
    vdevs
        .iter()
        .filter(|(_, built)| {
            built
                .iter()
                .any(|(uid, _)| dead.contains(uid) && !alive(uid))
        })
        .map(|(id, _)| (*id).to_owned())
        .collect()
}

/// Foreign buffer-size changes are set back at most this often per vdev.
const MAX_BUFFER_RESETS: u32 = 3;

/// Whether a changed aggregate buffer size (`now`) should be set back to `want`: it differs and the
/// vdev has not used up its resets.
fn should_reset_buffer(now: Option<u32>, want: u32, resets: u32) -> bool {
    resets < MAX_BUFFER_RESETS && now.is_some_and(|n| n != want)
}

/// Puts back the IO buffer size of the aggregates of the vdevs in `ids` where something else changed it.
fn reassert_buffers(g: &mut Inner, ids: &BTreeSet<String>) {
    for id in ids {
        let Some(v) = g.vdevs.get_mut(id) else {
            continue;
        };
        let want = v.spec.buffer_frames;
        for agg in std::iter::once(v.agg).chain(v.tapagg.as_ref().map(|t| t.agg)) {
            let now = hal::get_global::<u32>(agg, kAudioDevicePropertyBufferFrameSize).ok();
            if should_reset_buffer(now, want, v.buffer_resets) {
                v.buffer_resets += 1;
                log::info!(
                    "device '{id}': aggregate buffer changed to {now:?}, setting {want} again"
                );
                set_buffer_frames(agg, want);
            }
        }
    }
}

fn worker(inner: Arc<Mutex<Inner>>, rx: Receiver<Event>) {
    let mut retry = Retry::default();
    loop {
        let mut pending = Pending::default();
        let first = match retry.wait() {
            Some(d) => match rx.recv_timeout(d) {
                Ok(e) => Some(e),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(e) => Some(e),
                Err(_) => return,
            },
        };
        if let Some(mut ev) = first {
            let level = retry
                .fails
                .max(lock(&inner).io_fails.values().copied().max().unwrap_or(0));
            loop {
                if !pending.absorb(ev) {
                    return;
                }
                match rx.recv_timeout(settle(pending.wake, level)) {
                    Ok(e) => ev = e,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
        let mut g = lock(&inner);
        let targets = recovery_targets(&mut g, &mut pending);
        let devs = pending.devs && current_device_set() != g.dev_set;
        let Some(op) = retry.choose(devs, pending.wake, !targets.is_empty(), pending.procs) else {
            continue;
        };
        let res = run_op(&mut g, op);
        retry.record(op, res, &mut g.bg_errors);
    }
}

/// A device's `(input, output)` channels per stream buffer.
type StreamBufs = (Vec<u16>, Vec<u16>);

/// True when a device in `changed` now has other stream buffers than `subs` recorded at build: the
/// aggregate's buffer lists no longer match its layout, so the IOProc outputs silence until a rebuild.
/// `now(uid)` is `None` when the device is gone (a device event handles that).
fn reshaped(
    subs: &[SubDesc],
    changed: &BTreeSet<String>,
    now: &dyn Fn(&str) -> Option<StreamBufs>,
) -> bool {
    subs.iter().any(|s| {
        changed.contains(&s.uid)
            && now(&s.uid).is_some_and(|(i, o)| i != s.in_bufs || o != s.out_bufs)
    })
}

/// Applies the IOProc-start and rate news in `p`: tears down the vdevs that need a rebuild (the
/// following apply rebuilds them, with backoff while it fails) and returns their ids.
fn recovery_targets(g: &mut Inner, p: &mut Pending) -> BTreeSet<String> {
    let (mut failed, mut ok) = (Vec::new(), Vec::new());
    for (id, build, good) in p.io.drain(..) {
        // A stale build (already retired) says nothing about the current one.
        let Some(v) = g
            .vdevs
            .get_mut(&id)
            .filter(|v| v.build == build || v.tapagg.as_ref().is_some_and(|t| t.build == build))
        else {
            continue;
        };
        if good {
            v.rates = start_baseline(&v.rates, read_rates(&v.spec), &p.rates);
            ok.push(id);
        } else {
            failed.push(id);
        }
    }
    let mut forced = io_retry(&mut g.io_fails, &failed, &ok);
    let streams = |uid: &str| {
        let d = hal::device_by_uid(uid)?;
        Some((
            hal::stream_channels(d, kAudioObjectPropertyScopeInput),
            hal::stream_channels(d, kAudioObjectPropertyScopeOutput),
        ))
    };
    forced.extend(
        g.vdevs
            .iter()
            .filter(|(_, v)| reshaped(&v.subs, &p.rates, &streams))
            .map(|(id, _)| id.clone()),
    );
    let watched: Vec<(&str, &[(String, f64)])> = g
        .vdevs
        .iter()
        .map(|(id, v)| (id.as_str(), v.rates.as_slice()))
        .collect();
    forced.extend(dead_targets(&p.dead, &watched, &|uid| {
        hal::device_by_uid(uid).is_some_and(hal::is_alive)
    }));
    let targets = rebuild_targets(p.wake, &p.rates, &forced, &watched, &|uid| {
        hal::device_by_uid(uid).map_or(0.0, hal::nominal_rate)
    });
    if !targets.is_empty() {
        log::info!(
            "rebuilding {targets:?} (wake: {}, format change: {:?}, io failed or reshaped: {forced:?})",
            p.wake,
            p.rates
        );
    }
    for id in &targets {
        retire(g.vdevs.remove(id));
        g.recovering.insert(id.clone());
    }
    reassert_buffers(g, &std::mem::take(&mut p.buffers));
    targets
}

/// Uids of the physical devices the enabled virtual devices use: device sources and monitors.
fn used_device_uids(cfg: &Config) -> Vec<String> {
    let mut uids: Vec<String> = cfg
        .devices
        .iter()
        .filter(|d| d.enabled)
        .flat_map(|d| plan_subs(d, &|_| true))
        .filter(|u| !is_own_uid(u))
        .collect();
    uids.sort();
    uids.dedup();
    uids
}

/// One device used by several vdevs: the widest output any of them was built with, and whether any
/// of them reads its input. `None` when no vdev uses it. Independent of the vdev order.
fn combine_builds(builds: impl Iterator<Item = (u32, bool)>) -> Option<(u32, bool)> {
    builds.reduce(|(out, open), (o, i)| (out.max(o), open || i))
}

/// Devices of the config held in hog mode by another process, and Bluetooth devices that look switched
/// to the headset profile. Read from the HAL on each call (both are cheap property reads).
fn device_trouble(g: &Inner) -> (Vec<HogInfo>, Vec<String>) {
    let (mut hogged, mut hfp) = (Vec::new(), Vec::new());
    for uid in g.cfg.as_ref().map(used_device_uids).unwrap_or_default() {
        let Some(dev) = hal::device_by_uid(&uid) else {
            continue;
        };
        let name = hal::device_name(dev);
        hogged.extend(health::hog_info(dev));
        let builds = g
            .vdevs
            .values()
            .flat_map(|v| v.subs.iter().map(move |s| (v, s)))
            .filter(|(_, s)| s.uid == uid)
            .map(|(v, s)| {
                (
                    s.out_bufs.iter().map(|&c| u32::from(c)).sum(),
                    !s.in_bufs.is_empty() && v.spec.readers.contains(&uid),
                )
            });
        if let Some((built_out, input_open)) = combine_builds(builds)
            && health::looks_hfp(
                health::is_bluetooth(health::transport_type(dev)),
                hal::nominal_rate(dev),
                u32::from(hal::total_channels(dev, kAudioObjectPropertyScopeOutput)),
                built_out,
                input_open,
            )
        {
            hfp.push(name);
        }
    }
    hfp.sort();
    hfp.dedup();
    (hogged, hfp)
}

impl Backend for Engine {
    fn apply(&mut self, cfg: &Config) -> Result<()> {
        apply_locked(&mut lock(&self.inner), cfg, sync_driver)
    }

    fn hardware(&self) -> Vec<HwDevice> {
        hal::all_devices()
            .into_iter()
            .filter_map(|d| {
                let uid = hal::device_uid(d);
                if is_own_uid(&uid) {
                    return None;
                }
                Some(HwDevice {
                    uid,
                    name: hal::device_name(d),
                    in_ch: hal::total_channels(d, kAudioObjectPropertyScopeInput),
                    out_ch: hal::total_channels(d, kAudioObjectPropertyScopeOutput),
                })
            })
            .collect()
    }

    fn apps(&self) -> Vec<AppInfo> {
        apps::app_infos(&apps::list_groups())
    }

    fn status(&self) -> EngineStatus {
        let tcc = crate::tcc::audio_capture_status().into();
        let mic = mic_status();
        let driver_version =
            driver::find_plugin().map(|p| driver::version(p).unwrap_or_else(|| "?".into()));
        let g = lock(&self.inner);
        let waiting = g
            .bg_errors
            .get(&Op::Apply)
            .map_or(DeviceState::Starting, |e| DeviceState::Failed(e.clone()));
        let mut per_device: Vec<(String, DeviceState)> = g
            .vdevs
            .iter()
            .map(|(id, v)| (id.clone(), vdev_state(v, tcc)))
            .chain(
                g.recovering
                    .iter()
                    .filter(|id| !g.vdevs.contains_key(*id))
                    .map(|id| (id.clone(), waiting.clone())),
            )
            .collect();
        let mut delays: Vec<(String, Delay)> = g
            .vdevs
            .iter()
            .filter_map(|(id, v)| Some((id.clone(), main_delay(v)?)))
            .collect();
        delays.sort_by(|a, b| a.0.cmp(&b.0));
        let mut tap_delays: Vec<(String, Delay)> = g
            .vdevs
            .iter()
            .filter_map(|(id, v)| Some((id.clone(), tap_delay(v)?)))
            .collect();
        tap_delays.sort_by(|a, b| a.0.cmp(&b.0));
        let mut xruns: Vec<(String, u64)> = g
            .vdevs
            .iter()
            .map(|(id, v)| (id.clone(), v.xruns.load(Relaxed)))
            .collect();
        xruns.sort();
        let tap_silent: Vec<String> = g
            .vdevs
            .iter()
            .filter(|(_, v)| v.tap_silent)
            .map(|(id, _)| id.clone())
            .collect();
        let (hogged, bluetooth_hfp) = device_trouble(&g);
        drop(g);
        per_device.sort_by(|a, b| a.0.cmp(&b.0));
        let mut tap_silent = tap_silent;
        tap_silent.sort();
        EngineStatus {
            per_device,
            warnings: self.warnings(),
            background_error: self.background_error(),
            tcc,
            mic,
            driver_version,
            delays,
            tap_delays,
            low_power_mode: activity::low_power_mode(),
            xruns,
            tap_silent,
            hogged,
            bluetooth_hfp,
        }
    }

    fn self_test(&mut self, id: &str) -> Result<SelfTest> {
        let uid = format!("{DEVICE_UID_PREFIX}{id}");
        let cfg = lock(&self.inner).cfg.clone();
        // A virtual device's input reads silence until the microphone question is answered: ask now.
        if mic_status() == TccStatus::Unknown {
            crate::tcc::request_microphone(MIC_PROMPT_WAIT);
        }
        let probe = || Ok(crate::loopback::loopback_test(&uid, 2)?.to_self_test());
        let t = match cfg {
            // The engine owns every enabled device's input, or takes it back as soon as a recovery or
            // retry rebuilds it, which can land mid-probe with the user's monitors still wired: so route
            // the tone through the engine (probe route, no monitor wires) whenever the device is enabled.
            Some(cfg) => selftest::run_routed(self, &cfg, id, true, probe)?,
            None => probe()?,
        };
        selftest::verdict(t, mic_status())
    }

    fn measure_delay(&mut self, id: &str) -> Result<f64> {
        let uid = format!("{DEVICE_UID_PREFIX}{id}");
        let cfg = lock(&self.inner).cfg.clone();
        if mic_status() == TccStatus::Unknown {
            crate::tcc::request_microphone(MIC_PROMPT_WAIT);
        }
        let probe = || crate::loopback::measure_delay(&uid);
        let r = match cfg {
            Some(cfg) => selftest::run_routed(self, &cfg, id, true, probe),
            None => probe(),
        };
        r.map_err(|e| match mic_status() {
            TccStatus::Granted => e,
            _ => e.context("Microphone access is not allowed, which hands SoundRouter silence"),
        })
    }

    fn meters(&self, device_id: &str) -> Option<MeterReading> {
        let mut g = lock(&self.inner);
        let v = g.vdevs.get_mut(device_id)?;
        // Deliver a plan the full queue refused while the IOProc was not consuming (F5): a stale plan
        // can otherwise outlive the user's newer volume until the next apply.
        v.handle.flush();
        v.tap_in.flush();
        Some(v.meters.take(&v.layout, &v.dev))
    }
}

/// How long a self-test waits for the user to answer the microphone prompt.
const MIC_PROMPT_WAIT: Duration = Duration::from_secs(120);

/// Microphone permission, which reading a virtual device's input needs.
pub fn mic_status() -> TccStatus {
    match crate::tcc::microphone_status() {
        crate::tcc::CaptureStatus::Granted => TccStatus::Granted,
        crate::tcc::CaptureStatus::Denied => TccStatus::Denied,
        // -1: private API missing, so we cannot tell; do not let onboarding wait on it forever.
        crate::tcc::CaptureStatus::Unknown(-1) => TccStatus::Granted,
        crate::tcc::CaptureStatus::Unknown(_) => TccStatus::Unknown,
    }
}

/// Our own virtual devices (visible or companion) and the aggregates the engine builds. Picking one
/// as a source or monitor would feed a device back into itself.
pub fn is_own_uid(uid: &str) -> bool {
    uid.starts_with(DEVICE_UID_PREFIX)
        || uid.starts_with(AGG_UID_PREFIX)
        || uid.ends_with(COMPANION_SUFFIX)
}

/// Subdevice uids (besides the main): Device sources then enabled monitors, deduped, present only.
fn plan_subs(dev: &VirtualDevice, present: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let srcs = dev.sources.iter().filter_map(|s| match &s.kind {
        SourceKind::Device { uid } => Some(uid.as_str()),
        _ => None,
    });
    let mons = dev
        .monitors
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.device_uid.as_str());
    for uid in srcs.chain(mons) {
        if present(uid) && !out.iter().any(|u| u == uid) {
            out.push(uid.to_owned());
        }
    }
    out
}

/// Test/advanced hook: when `SR_MAIN_UID_OVERRIDE` names a device uid, the engine routes into that
/// existing loopback device (e.g. BlackHole) instead of our driver's devices, and never touches the driver.
const MAIN_UID_OVERRIDE: &str = "SR_MAIN_UID_OVERRIDE";

fn main_candidates(dev: &VirtualDevice) -> Vec<String> {
    let ov = std::env::var(MAIN_UID_OVERRIDE).ok();
    main_candidates_in(dev, ov.as_deref(), &|u| hal::device_by_uid(u).is_some())
}

/// Main-device uids to try, best first, present only. A non-empty `override_uid` is the only candidate.
fn main_candidates_in(
    dev: &VirtualDevice,
    override_uid: Option<&str>,
    present: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    if let Some(uid) = override_uid.filter(|u| !u.is_empty()) {
        return if present(uid) {
            vec![uid.to_owned()]
        } else {
            vec![]
        };
    }
    let vis = format!("{DEVICE_UID_PREFIX}{}", dev.id);
    let mut c = vec![companion_uid(&vis)];
    if ALLOW_VISIBLE_FALLBACK {
        c.push(vis);
    }
    c.retain(|u| present(u));
    c
}

fn sync_driver(cfg: &Config) -> Result<()> {
    if std::env::var_os(MAIN_UID_OVERRIDE).is_some_and(|v| !v.is_empty()) {
        return Ok(());
    }
    let want: Vec<DriverDevice> = cfg
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| DriverDevice {
            uid: format!("{DEVICE_UID_PREFIX}{}", d.id),
            name: d.name.clone(),
            channels: d.channels,
        })
        .collect();
    let plugin = match driver::find_plugin() {
        Some(p) => p,
        None if want.is_empty() => return Ok(()),
        // Looks again: the plug-in may have appeared since, so this must not assume an error.
        None => driver::require_plugin()?,
    };
    let old = driver::get_devices(plugin).unwrap_or_default();
    let removed = push_devices(&want, old, |l| driver::set_devices(plugin, l))?;
    wait_driver_settled(&want, &removed)
}

/// Sets `want` even when the driver already has it: an identical list stores and announces nothing, but the
/// driver re-asks a device side whose format change the host aborted (that pair stays silent until then).
/// Returns the uids in `old` that `want` drops.
fn push_devices(
    want: &[DriverDevice],
    old: Vec<DriverDevice>,
    set: impl FnOnce(&[DriverDevice]) -> Result<()>,
) -> Result<Vec<String>> {
    set(want)?;
    Ok(old
        .into_iter()
        .filter(|o| !want.iter().any(|w| w.uid == o.uid))
        .map(|o| o.uid)
        .collect())
}

/// How long after `set_devices` the driver's devices may take to appear and take their channel counts.
/// Adding 15 devices at once takes about 3 s on an M-series Mac (coreaudiod publishes them one by one).
const DRIVER_SETTLE: Duration = Duration::from_secs(6);
/// Poll step while settling.
const SETTLE_STEP: Duration = Duration::from_millis(50);
/// Hidden companions are best-effort: after this long without one, `main_candidates` falls back to the visible device.
const COMPANION_GRACE: Duration = Duration::from_millis(500);

/// Devices from `want` that are not ready: visible device and companion present, and each reporting
/// exactly the configured channels on both scopes (the HAL applies a channel change asynchronously, F1b).
/// `totals(uid)` = `(input, output)` channels, `None` when the device does not exist. A `removed` device
/// (or its companion) must be gone too: the HAL drops it a moment after the driver does, and a disable
/// then enable that found the dying device would take it as settled and lose it right after.
fn unsettled(
    want: &[DriverDevice],
    totals: &dyn Fn(&str) -> Option<(u16, u16)>,
    companion_optional: bool,
    removed: &[String],
) -> Vec<String> {
    let check = |uid: &str, ch: u16| match totals(uid) {
        None => Some(format!("{uid}: not present")),
        Some((i, o)) if i != ch || o != ch => Some(format!("{uid}: {i} in / {o} out, want {ch}")),
        Some(_) => None,
    };
    let lingering = removed
        .iter()
        .flat_map(|u| [u.clone(), companion_uid(u)])
        .filter(|u| totals(u).is_some())
        .map(|u| format!("{u}: removed but still present"));
    want.iter()
        .flat_map(|d| {
            let comp = companion_uid(&d.uid);
            let comp_bad = match totals(&comp) {
                None if companion_optional => None,
                _ => check(&comp, d.channels),
            };
            [check(&d.uid, d.channels), comp_bad]
        })
        .flatten()
        .chain(lingering)
        .collect()
}

fn wait_driver_settled(want: &[DriverDevice], removed: &[String]) -> Result<()> {
    let totals = |uid: &str| {
        hal::device_by_uid(uid).map(|d| {
            (
                hal::total_channels(d, kAudioObjectPropertyScopeInput),
                hal::total_channels(d, kAudioObjectPropertyScopeOutput),
            )
        })
    };
    let t0 = Instant::now();
    loop {
        let elapsed = t0.elapsed();
        let bad = unsettled(want, &totals, elapsed >= COMPANION_GRACE, removed);
        if bad.is_empty() {
            return Ok(());
        }
        if elapsed >= DRIVER_SETTLE {
            bail!(
                "driver devices not ready {} s after set_devices: {}",
                DRIVER_SETTLE.as_secs(),
                bad.join("; ")
            );
        }
        std::thread::sleep(SETTLE_STEP);
    }
}

/// `sync` brings the driver's device list in line with `cfg` (a parameter so tests can fail it without a driver).
fn apply_locked(g: &mut Inner, cfg: &Config, sync: fn(&Config) -> Result<()>) -> Result<()> {
    cfg.validate().map_err(|e| anyhow!("invalid config: {e}"))?;
    // Kept even if the driver does not settle in time: the device events that follow rebuild this
    // config. Keeping the previous one instead let the next background apply undo the user's change.
    g.cfg = Some(cfg.clone());
    let enabled: HashMap<&str, u16> = cfg
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| (d.id.as_str(), d.channels))
        .collect();
    // Removed devices and those changing channel count go before the driver changes them: a companion
    // that a running aggregate still uses takes its channel change late (several seconds with many devices).
    let stale: Vec<String> = g
        .vdevs
        .iter()
        .filter(|(id, v)| enabled.get(id.as_str()) != Some(&v.spec.channels))
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        retire(g.vdevs.remove(&id));
    }
    // A removed vdev's start failures must not keep stretching the worker's debounce.
    g.io_fails.retain(|id, _| enabled.contains_key(id.as_str()));
    sync(cfg)?;
    // Taken before any aggregate is built: our own create/destroy events must compare equal (F4).
    g.dev_set = current_device_set();
    let groups = apps::list_groups();
    let mut errors = Vec::new();
    for dev in cfg.devices.iter().filter(|d| d.enabled) {
        if let Err(e) = apply_vdev(g, dev, cfg.buffer_frames, &groups) {
            errors.push(format!("device '{}': {e:#}", dev.id));
        }
    }
    // A device disabled or removed while it was waiting to be rebuilt is not recovering any more.
    g.recovering
        .retain(|id| enabled.contains_key(id.as_str()) && !g.vdevs.contains_key(id));
    if errors.is_empty() {
        // Everything a background update does is now in place: its old failure no longer holds.
        g.bg_errors.clear();
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

/// Whether the live vdev (`live`, built after `passed_over` failed as its main) can take the new config
/// in place instead of being rebuilt. Not when its IOProc failed to start: only a new build retries that
/// (`io_retry` bounds the background ones). Not when a better main is present that was not tried at its
/// build: a fallback built before the companion was published then moves to it.
#[allow(clippy::too_many_arguments)]
fn keep_in_place(
    live: &AggSpec,
    passed_over: &[String],
    io: IoState,
    cands: &[String],
    subs: &[String],
    channels: u16,
    buffer_frames: u32,
    split: bool,
) -> bool {
    !matches!(io, IoState::Failed(_))
        && live.subs == subs
        && live.channels == channels
        && live.buffer_frames == buffer_frames
        && live.split == split
        && cands
            .iter()
            .find(|c| **c == live.main || !passed_over.contains(c))
            == Some(&live.main)
}

fn apply_vdev(
    g: &mut Inner,
    dev: &VirtualDevice,
    buffer_frames: u32,
    groups: &[AppGroup],
) -> Result<()> {
    let cands = main_candidates(dev);
    if cands.is_empty() {
        retire(g.vdevs.remove(&dev.id));
        bail!(
            "neither the hidden companion nor the visible device is present (driver not loaded?)"
        );
    }
    let subs = plan_subs(dev, &|u| hal::device_by_uid(u).is_some());
    let split = tap_split_enabled();
    if let Some(v) = g.vdevs.get_mut(&dev.id) {
        let io = v.io();
        if v.spec.readers == input_readers(dev, &v.spec.main)
            && keep_in_place(
                &v.spec,
                &v.passed_over,
                io,
                &cands,
                &subs,
                dev.channels,
                buffer_frames,
                split,
            )
        {
            v.dev = dev.clone();
            match sync_taps(v, groups).and_then(|()| refresh_layout(v)) {
                Ok(()) => return Ok(()),
                // F8: the live aggregate is out of step (e.g. a subdevice's streams changed): rebuild it.
                Err(e) => log::warn!(
                    "device '{}': in-place update failed, rebuilding: {e:#}",
                    dev.id
                ),
            }
        }
        retire(g.vdevs.remove(&dev.id));
    }
    let mut last = anyhow!("no candidate main device");
    let mut passed_over = Vec::new();
    for main in cands {
        match build_vdev(
            dev,
            AggSpec {
                main: main.clone(),
                subs: subs.clone(),
                channels: dev.channels,
                buffer_frames,
                split,
                readers: input_readers(dev, &main),
            },
            groups,
            g.tx.as_ref(),
            &g.activity,
        ) {
            Ok(mut v) => {
                log::info!("device '{}': aggregate main device '{main}'", dev.id);
                v.passed_over = passed_over;
                g.vdevs.insert(dev.id.clone(), v);
                return Ok(());
            }
            Err(e) => {
                log::warn!("device '{}': main '{main}' failed: {e:#}", dev.id);
                passed_over.push(main);
                last = e;
            }
        }
    }
    Err(last)
}

/// Re-sync the taps of every vdev; one vdev's failure does not stop the others.
fn retap(g: &mut Inner) -> Result<()> {
    let groups = apps::list_groups();
    let errors: Vec<String> = g
        .vdevs
        .iter_mut()
        .filter_map(|(id, v)| {
            sync_taps(v, &groups)
                .err()
                .map(|e| format!("device '{id}': {e:#}"))
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

fn make_desc(name: &str, objs: &[u32], mute: bool) -> Retained<CATapDescription> {
    let ids: Vec<Retained<NSNumber>> = objs.iter().map(|&o| NSNumber::new_u32(o)).collect();
    let desc = unsafe {
        CATapDescription::initStereoMixdownOfProcesses(
            CATapDescription::alloc(),
            &NSArray::from_retained_slice(&ids),
        )
    };
    unsafe {
        desc.setName(&NSString::from_str(name));
        desc.setPrivate(true);
        desc.setMuteBehavior(if mute {
            CATapMuteBehavior::MutedWhenTapped
        } else {
            CATapMuteBehavior::Unmuted
        });
    }
    desc
}

fn create_tap(source_id: &str, bundle: &str, objs: Vec<u32>, mute: bool) -> Result<Tap> {
    let desc = make_desc(&format!("sr-{source_id}"), &objs, mute);
    let mut id = 0u32;
    hal::check("AudioHardwareCreateProcessTap", unsafe {
        AudioHardwareCreateProcessTap(Some(&desc), &mut id)
    })?;
    let uid = match hal::get_string(id, kAudioTapPropertyUID) {
        Ok(u) => u,
        Err(s) => {
            unsafe { AudioHardwareDestroyProcessTap(id) };
            bail!("tap uid: {}", hal::fourcc(s));
        }
    };
    Ok(Tap {
        source_id: source_id.into(),
        bundle: bundle.into(),
        id,
        uid,
        objs,
        mute,
    })
}

fn app_objs(groups: &[AppGroup], bundle: &str) -> Vec<u32> {
    groups
        .iter()
        .find(|g| g.bundle_id == bundle)
        .map(|g| g.objs.clone())
        .unwrap_or_default()
}

/// `(source id, bundle id, mute original output)` of every app source.
fn app_sources(dev: &VirtualDevice) -> impl Iterator<Item = (&str, &str, bool)> {
    dev.sources.iter().filter_map(|s| match &s.kind {
        SourceKind::App { bundle_id } => Some((s.id.as_str(), bundle_id.as_str(), s.mute_original)),
        _ => None,
    })
}

/// What `sync_taps` must do to move `current` to the desired state. Indices refer to `current`.
#[derive(Debug, Default, PartialEq)]
struct TapPlan {
    keep: Vec<usize>,
    /// Same source, new process objects: rewrite the tap description.
    update: Vec<(usize, Vec<u32>)>,
    remove: Vec<usize>,
    /// `(source id, bundle id, process objects, mute)` of taps to create, in source order.
    create: Vec<(String, String, Vec<u32>, bool)>,
}

/// Pure diff of live taps against the config + running processes. A tap whose app has no
/// processes is removed (app quit); it comes back as `create` once the app runs again. A source
/// whose app is not running has no tap at all: idle, not an error. A flipped mute flag removes the tap
/// and creates it anew.
fn tap_plan(current: &[Tap], dev: &VirtualDevice, groups: &[AppGroup]) -> TapPlan {
    let mut p = TapPlan::default();
    for (i, t) in current.iter().enumerate() {
        let objs = app_sources(dev)
            .find(|(id, b, m)| *id == t.source_id && *b == t.bundle && *m == t.mute)
            .map(|(_, b, _)| app_objs(groups, b))
            .unwrap_or_default();
        if objs.is_empty() {
            p.remove.push(i);
        } else if objs != t.objs {
            p.update.push((i, objs));
        } else {
            p.keep.push(i);
        }
    }
    for (id, b, mute) in app_sources(dev) {
        let live = current
            .iter()
            .enumerate()
            .any(|(i, t)| t.source_id == id && !p.remove.contains(&i));
        let objs = app_objs(groups, b);
        if !live && !objs.is_empty() {
            p.create.push((id.to_owned(), b.to_owned(), objs, mute));
        }
    }
    p
}

/// Bring the live taps in line with the config + running processes (live tap-list edit, I/O keeps running).
///
/// Failure handling (F7): no step may lose a tap that is still referenced by the aggregate. Removed taps
/// wait in `v.zombies` and are destroyed only once the aggregate's tap list no longer names them; a failed
/// list update leaves `v.tap_dirty` so the next sync repeats it instead of seeing "no diff" and staying silent.
fn sync_taps(v: &mut Vdev, groups: &[AppGroup]) -> Result<()> {
    let plan = tap_plan(&v.taps, &v.dev, groups);
    log::debug!(
        "taps for '{}': keep {} update {} remove {} create {}",
        v.dev.id,
        plan.keep.len(),
        plan.update.len(),
        plan.remove.len(),
        plan.create.len()
    );
    let mut changed = v.tap_dirty || !plan.remove.is_empty();
    for (i, objs) in plan.update {
        let t = &v.taps[i];
        let desc = make_desc(&format!("sr-{}", t.source_id), &objs, t.mute);
        match hal::set_object(t.id, kAudioTapPropertyDescription, &desc) {
            Ok(()) => v.taps[i].objs = objs,
            // Keep the old description (and `objs`) so the next sync retries.
            Err(s) => log::warn!(
                "set description of tap '{}' failed: {}",
                t.source_id,
                hal::fourcc(s)
            ),
        }
    }
    for &i in plan.remove.iter().rev() {
        v.zombies.push(v.taps.remove(i));
    }
    for (id, b, objs, mute) in plan.create {
        match create_tap(&id, &b, objs, mute) {
            Ok(t) => {
                v.taps.push(t);
                changed = true;
            }
            Err(e) => log::warn!("tap for '{b}' failed: {e:#}"),
        }
    }
    if changed {
        v.tap_dirty = true;
        if v.spec.split {
            rebuild_tap_agg(v)?;
            v.tap_dirty = false;
            return Ok(());
        }
        v.shared.expected.store(0, Release); // silence until the new layout is in
        let uids: Vec<String> = v.taps.iter().map(|t| t.uid.clone()).collect();
        hal::set_object(
            v.agg,
            kAudioAggregateDevicePropertyTapList,
            &hal::string_array(&uids),
        )
        .map_err(|s| anyhow!("set aggregate tap list: {}", hal::fourcc(s)))?;
        // The aggregate no longer references the removed taps.
        for t in v.zombies.drain(..) {
            unsafe { AudioHardwareDestroyProcessTap(t.id) };
        }
        let want = v.subs.iter().map(|s| s.in_bufs.len()).sum::<usize>() + v.taps.len();
        for _ in 0..50 {
            if hal::stream_channels(v.agg, kAudioObjectPropertyScopeInput).len() == want {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        refresh_layout(v)?;
        restrict_inputs(v.agg, v.proc_id, &v.subs, &v.spec.readers, v.taps.len());
        v.tap_dirty = false;
    }
    Ok(())
}

/// Replaces a split vdev's tap aggregate with one listing `v.taps` (none: no aggregate), hands its ring
/// to the main IOProc and republishes the layout. The main aggregate keeps mixing every other path
/// throughout; the tapped apps are silent from the old aggregate's stop until the new one runs.
///
/// Fails without touching what runs when the new aggregate cannot be built or does not fit the mixer;
/// `v.tap_dirty` makes the next sync try again.
fn rebuild_tap_agg(v: &mut Vdev) -> Result<()> {
    let (new, rx) = if v.taps.is_empty() {
        (None, TapRx::none(v.tap_stats.clone()))
    } else {
        let (agg, rx) = tapagg::build(&TapAggSpec {
            dev_id: &v.dev.id,
            main: &v.spec.main,
            taps: &v.taps,
            buffer_frames: v.spec.buffer_frames,
            stats: &v.tap_stats,
            events: v.events.as_ref(),
            activity: &v.activity,
            xruns: &v.xruns,
        })?;
        (Some(agg), rx)
    };
    let old = std::mem::replace(&mut v.tapagg, new);
    if let Err(e) = refresh_layout(v) {
        retire_tap(std::mem::replace(&mut v.tapagg, old));
        return Err(e);
    }
    v.tap_in.publish(rx);
    // The old aggregate may still list the taps removed since: it destroys them once it is gone.
    let mut old = old;
    match old.as_mut() {
        Some(o) => o.zombies.append(&mut v.zombies),
        None => v
            .zombies
            .drain(..)
            .for_each(|t| unsafe { _ = AudioHardwareDestroyProcessTap(t.id) }),
    }
    retire_tap(old);
    Ok(())
}

/// The config-derived inputs of the layout spec.
#[derive(Debug, Default, PartialEq)]
struct LayoutInputs {
    /// Source id fed by the main device's input. `None` when the main is the visible device itself
    /// (fallback without the companion): its input is the driver's V.out -> V.in loop, which also
    /// carries the engine's own output, so reading it as Pass-Thru would feed the mix back into itself.
    passthru: Option<String>,
    /// Device uid -> Device source id (first source wins).
    device_sources: HashMap<String, String>,
    /// Uids of enabled monitors.
    monitors: HashSet<String>,
}

/// `main` is the uid of the aggregate's main device.
fn layout_inputs(dev: &VirtualDevice, main: &str) -> LayoutInputs {
    let fallback = main == format!("{DEVICE_UID_PREFIX}{}", dev.id);
    LayoutInputs {
        passthru: dev
            .sources
            .iter()
            .find(|s| s.kind == SourceKind::PassThru && !fallback)
            .map(|s| s.id.clone()),
        device_sources: dev
            .sources
            .iter()
            .rev()
            .filter_map(|s| match &s.kind {
                SourceKind::Device { uid } => Some((uid.clone(), s.id.clone())),
                _ => None,
            })
            .collect(),
        monitors: dev
            .monitors
            .iter()
            .filter(|m| m.enabled)
            .map(|m| m.device_uid.clone())
            .collect(),
    }
}

/// Enabled monitors' wires whose `dst_ch` is beyond the hardware's output channels: the mixer cannot
/// route them, so say so instead of dropping them silently (bug 7). Absent devices are not reported.
fn monitor_warnings(dev: &VirtualDevice, subs: &[SubDesc]) -> Vec<String> {
    let mut out = Vec::new();
    for m in dev.monitors.iter().filter(|m| m.enabled) {
        let Some(sub) = subs.iter().find(|s| s.uid == m.device_uid) else {
            continue;
        };
        let n = sub.out_bufs.iter().sum::<u16>();
        for w in m.wires.iter().filter(|w| w.dst_ch >= n) {
            out.push(format!(
                "device '{}': monitor '{}' wire from channel {} to output channel {} is not routed: \
                 the hardware has {n} output channels",
                dev.id, m.device_uid, w.out_ch, w.dst_ch
            ));
        }
    }
    out
}

/// The mixer handles at most `MAX_CH` channels per direction. Fail loudly rather than drop channels:
/// the aggregate's inputs are the device's own channels + hardware sources + 2 per app tap, its
/// outputs the device's channels + enabled monitors' channels.
fn check_width(lay: &IoLayout) -> Result<()> {
    let (n_in, n_out) = (lay.inputs.len(), lay.outputs.len());
    if n_in > MAX_CH || n_out > MAX_CH {
        bail!(
            "aggregate needs {n_in} input / {n_out} output channels but the engine mixes at most {MAX_CH} per direction \
             (inputs = device channels + hardware sources + 2 per app source; outputs = device channels + monitor channels); \
             reduce the device's channel count or remove sources/monitors"
        );
    }
    Ok(())
}

/// Recompute the layout from the aggregate's current streams, compile the plan, publish both.
fn refresh_layout(v: &mut Vdev) -> Result<()> {
    let main_in = hal::stream_channels(v.agg, kAudioObjectPropertyScopeInput);
    // A split vdev's tap buffers come from the tap aggregate, behind the main aggregate's own.
    let ain: Vec<u16> = main_in
        .iter()
        .copied()
        .chain(v.tapagg.iter().flat_map(|t| t.bufs.iter().copied()))
        .collect();
    let aout = hal::stream_channels(v.agg, kAudioObjectPropertyScopeOutput);
    let li = layout_inputs(&v.dev, &v.spec.main);
    let tap_ids: Vec<String> = v.taps.iter().map(|t| t.source_id.clone()).collect();
    let spec = LayoutSpec {
        subs: &v.subs,
        passthru: li.passthru.as_deref(),
        device_sources: &li.device_sources,
        monitors: &li.monitors,
        taps: &tap_ids,
    };
    let lay = layout::build_layout(&spec, &ain, &aout)?;
    check_width(&lay).with_context(|| {
        format!(
            "device '{}' ({} channels, {} app taps)",
            v.dev.id,
            v.dev.channels,
            v.taps.len()
        )
    })?;
    let warnings = monitor_warnings(&v.dev, &v.subs);
    for w in warnings.iter().filter(|w| !v.warnings.contains(w)) {
        log::warn!("{w}");
    }
    v.warnings = warnings;
    v.handle.set_plan(MixPlan::compile(&v.dev, &lay));
    v.layout = lay;
    v.shared
        .expected
        .store(layout::shape_key_of(&main_in, &aout), Release);
    Ok(())
}

/// Asks the aggregate for `frames`-frame IO buffers; outside the device's range or on any HAL
/// error the aggregate keeps its default size (logged, not fatal).
fn set_buffer_frames(agg: u32, frames: u32) {
    let range = hal::get_global::<AudioValueRange>(agg, kAudioDevicePropertyBufferFrameSizeRange);
    let result = match range {
        Ok(r) if (r.mMinimum..=r.mMaximum).contains(&f64::from(frames)) => {
            hal::set_global(agg, kAudioDevicePropertyBufferFrameSize, &frames)
                .map_err(|st| format!("set failed: OSStatus {}", hal::fourcc(st)))
        }
        Ok(r) => Err(format!("outside range {}..={}", r.mMinimum, r.mMaximum)),
        Err(st) => Err(format!("range unreadable: OSStatus {}", hal::fourcc(st))),
    };
    let now = hal::get_global::<u32>(agg, kAudioDevicePropertyBufferFrameSize).unwrap_or(0);
    match result {
        Ok(()) => log::info!("aggregate {agg}: buffer {frames} frames (now {now})"),
        Err(e) => log::info!("aggregate {agg}: buffer {frames} frames {e}; keeping {now}"),
    }
}

/// Starts `proc_id` on `agg` off the caller's thread (`AudioDeviceStart` blocks until the TCC prompt is
/// answered), records the outcome in `sh.state` and reports it as an [`Event::IoStart`] of `build`.
fn spawn_starter(
    agg: u32,
    proc_id: AudioDeviceIOProcID,
    sh: Arc<Shared>,
    events: Option<Sender<Event>>,
    id: String,
    build: u64,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("sr-audio-start".into())
        .spawn(move || {
            let st = unsafe { AudioDeviceStart(agg, proc_id) };
            sh.state.store(
                if st == 0 {
                    1
                } else {
                    (1 << 32) | u64::from(st as u32)
                },
                Relaxed,
            );
            if let Some(tx) = events {
                let _ = tx.send(Event::IoStart {
                    id,
                    build,
                    ok: st == 0,
                });
            }
        })
}

fn build_vdev(
    dev: &VirtualDevice,
    spec: AggSpec,
    groups: &[AppGroup],
    events: Option<&Sender<Event>>,
    activity: &Arc<Activity>,
) -> Result<Vdev> {
    // Format listeners first, then the baseline, then the aggregate: a rate change at any point after the
    // read has an event that compares unequal, one before it is in both the baseline and the aggregate.
    // A stream layout change (an interface switched to another channel mode) reuses the rate event.
    let xruns = Arc::new(AtomicU64::new(0));
    let mut listeners: Vec<Listener> = events.map_or_else(Vec::new, |tx| {
        std::iter::once(&spec.main)
            .chain(&spec.subs)
            .filter_map(|uid| Some((uid, hal::device_by_uid(uid)?)))
            .flat_map(|(uid, obj)| {
                let (u1, u2) = (uid.clone(), uid.clone());
                [
                    Listener::new(
                        obj,
                        kAudioDevicePropertyNominalSampleRate,
                        kAudioObjectPropertyScopeGlobal,
                        tx.clone(),
                        move || Event::Rate(u1.clone()),
                    ),
                    Listener::new(
                        obj,
                        kAudioDevicePropertyStreamConfiguration,
                        kAudioObjectPropertyScopeWildcard,
                        tx.clone(),
                        move || Event::Rate(u2.clone()),
                    ),
                    Listener::new(
                        obj,
                        kAudioDevicePropertyDeviceIsAlive,
                        kAudioObjectPropertyScopeGlobal,
                        tx.clone(),
                        {
                            let u = uid.clone();
                            move || Event::Dead(u.clone())
                        },
                    ),
                    Listener::counter(obj, kAudioDeviceProcessorOverload, xruns.clone()),
                ]
                .map(|l| {
                    l.map_err(|e| log::warn!("no device listener on '{uid}': {e:#}"))
                        .ok()
                })
            })
            .flatten()
            .collect()
    });
    let rates = read_rates(&spec);
    let mut taps = Vec::new();
    for (id, b, mute) in app_sources(dev) {
        let objs = app_objs(groups, b);
        if objs.is_empty() {
            continue;
        }
        match create_tap(id, b, objs, mute) {
            Ok(t) => taps.push(t),
            Err(e) => log::warn!("tap for '{b}' failed: {e:#}"),
        }
    }
    let destroy_taps = |taps: &[Tap]| {
        taps.iter()
            .for_each(|t| unsafe { _ = AudioHardwareDestroyProcessTap(t.id) })
    };

    // `drift` must be a CFNumber: the HAL reads a CFBoolean (`NSNumber::new_bool`) as 0, i.e. no drift
    // compensation at all (checked via the aggregate's composition). Low quality then halves the
    // resampler's fixed delay; the main device is the clock and has none.
    let low = NSNumber::new_u32(kAudioAggregateDriftCompensationLowQuality);
    let sub_dicts: Vec<Retained<_>> = std::iter::once((&spec.main, false))
        .chain(spec.subs.iter().map(|s| (s, true)))
        .map(|(uid, drift)| {
            let (u, d) = (NSString::from_str(uid), NSNumber::new_i32(i32::from(drift)));
            let pairs: [(&CStr, &AnyObject); 3] = [
                (kAudioSubDeviceUIDKey, &u),
                (kAudioSubDeviceDriftCompensationKey, &d),
                (kAudioSubDeviceDriftCompensationQualityKey, &low),
            ];
            hal::cdict(&pairs[..if drift { 3 } else { 2 }])
        })
        .collect();
    let tap_dicts: Vec<Retained<_>> = taps
        .iter()
        .filter(|_| !spec.split)
        .map(|t| {
            let (u, d) = (NSString::from_str(&t.uid), hal::bool_obj(true));
            hal::cdict(&[
                (kAudioSubTapUIDKey, &u),
                (kAudioSubTapDriftCompensationKey, &d),
            ])
        })
        .collect();
    let (subs_arr, taps_arr) = (
        NSArray::from_retained_slice(&sub_dicts),
        NSArray::from_retained_slice(&tap_dicts),
    );
    // A retired vdev may still be tearing down on a reaper thread, so each build gets its own uid.
    let seq = AGG_SEQ.fetch_add(1, Relaxed);
    let agg_uid = NSString::from_str(&format!("{AGG_UID_PREFIX}{}.{seq}", dev.id));
    let name = NSString::from_str(&format!("SoundRouter {}", dev.id));
    let main = NSString::from_str(&spec.main);
    let (yes, no) = (hal::bool_obj(true), hal::bool_obj(false));
    let d = hal::cdict(&[
        (kAudioAggregateDeviceUIDKey, &agg_uid),
        (kAudioAggregateDeviceNameKey, &name),
        (kAudioAggregateDeviceIsPrivateKey, &yes),
        (kAudioAggregateDeviceMainSubDeviceKey, &main),
        (kAudioAggregateDeviceClockDeviceKey, &main),
        (kAudioAggregateDeviceIsStackedKey, &no),
        (kAudioAggregateDeviceSubDeviceListKey, &subs_arr),
        (kAudioAggregateDeviceTapListKey, &taps_arr),
        (kAudioAggregateDeviceTapAutoStartKey, &no),
    ]);
    let mut agg = 0u32;
    let st = unsafe {
        AudioHardwareCreateAggregateDevice(
            &*(Retained::as_ptr(&d) as *const CFDictionary),
            NonNull::from(&mut agg),
        )
    };
    if st != 0 {
        destroy_taps(&taps);
        bail!(
            "create aggregate with main '{}' failed: OSStatus {}",
            spec.main,
            hal::fourcc(st)
        );
    }
    if let Some(tx) = events {
        let id = dev.id.clone();
        listeners.extend(
            [
                Listener::counter(agg, kAudioDeviceProcessorOverload, xruns.clone()),
                Listener::new(
                    agg,
                    kAudioDevicePropertyBufferFrameSize,
                    kAudioObjectPropertyScopeGlobal,
                    tx.clone(),
                    move || Event::Buffer(id.clone()),
                ),
            ]
            .into_iter()
            .filter_map(|l| {
                l.map_err(|e| log::warn!("no aggregate listener: {e:#}"))
                    .ok()
            }),
        );
    }
    let fail = |taps: &[Tap], agg: u32| {
        unsafe { AudioHardwareDestroyAggregateDevice(agg) };
        destroy_taps(taps);
    };

    let mut descs = Vec::new();
    for uid in std::iter::once(&spec.main).chain(&spec.subs) {
        let Some(id) = hal::device_by_uid(uid) else {
            fail(&taps, agg);
            bail!("subdevice '{uid}' vanished");
        };
        descs.push(SubDesc {
            uid: uid.clone(),
            is_main: *uid == spec.main,
            in_bufs: hal::stream_channels(id, kAudioObjectPropertyScopeInput),
            out_bufs: hal::stream_channels(id, kAudioObjectPropertyScopeOutput),
        });
    }
    let meters = Arc::new(Meters::new());
    let (mx, handle) = mixer(meters.clone());
    let sysvol = sysvol::follow(&format!("{DEVICE_UID_PREFIX}{}", dev.id), handle.sys_gain());
    let split = spec.split;
    let tap_stats = Arc::new(TapStats::default());
    let (tap_cb, tap_handle) = tap_in(tap_stats.clone());
    let shared = Arc::new(Shared {
        expected: AtomicU64::new(0),
        state: AtomicU64::new(0),
        delay: AtomicU32::new(NO_DELAY),
        lead: AtomicU32::new(NO_DELAY),
    });
    let cb = Box::into_raw(Box::new(Cb {
        mixer: mx,
        shared: shared.clone(),
        meters: meters.clone(),
        in_planar: Scratch::new(),
        out_planar: Scratch::new(),
        next_t: f64::NAN,
        tap: tap_cb,
    }));
    // A split vdev's taps join once the main aggregate runs: until then they only need cleaning up.
    let (live_taps, parked) = if split {
        (Vec::new(), taps)
    } else {
        (taps, Vec::new())
    };
    let mut v = Vdev {
        dev: dev.clone(),
        spec,
        agg,
        proc_id: None,
        cb,
        shared,
        handle,
        meters,
        layout: sr_core::mixer::IoLayout {
            inputs: vec![],
            outputs: vec![],
        },
        subs: descs,
        taps: live_taps,
        tapagg: None,
        tap_in: tap_handle,
        tap_stats,
        events: events.cloned(),
        activity: activity.clone(),
        starter: None,
        build: seq,
        rates,
        _listeners: listeners,
        _sysvol: sysvol,
        _lease: activity.lease(),
        xruns,
        silence: SilenceWatch::default(),
        tap_silent: false,
        buffer_resets: 0,
        zombies: parked,
        tap_dirty: false,
        warnings: Vec::new(),
        passed_over: Vec::new(),
    };
    // From here `Drop` cleans up the aggregate, taps and callback state.
    refresh_layout(&mut v)?;
    // Only the aggregate is asked: a size requested for V or C from this process (no IO of ours on V)
    // changed neither a client's cycle nor the loop delay when measured; a client's size is its own.
    set_buffer_frames(agg, v.spec.buffer_frames);
    let st = unsafe {
        AudioDeviceCreateIOProcID(
            agg,
            Some(ioproc),
            cb as *mut c_void,
            NonNull::from(&mut v.proc_id),
        )
    };
    hal::check("AudioDeviceCreateIOProcID", st).context("creating IOProc")?;
    restrict_inputs(
        agg,
        v.proc_id,
        &v.subs,
        &v.spec.readers,
        if split { 0 } else { v.taps.len() },
    );
    let (proc_id, sh) = (v.proc_id, v.shared.clone());
    // AudioDeviceStart blocks until the TCC prompt is answered: keep it off the caller's thread.
    // `Drop` joins this thread before stopping or destroying anything it uses (F3).
    v.starter = Some(spawn_starter(
        agg,
        proc_id,
        sh,
        events.cloned(),
        dev.id.clone(),
        seq,
    )?);
    v.handle.flush();
    if split {
        // The main aggregate is already running every other path: the taps follow, and a failure here
        // (or a permission prompt holding the tap aggregate's start) never takes them down.
        v.taps = std::mem::take(&mut v.zombies);
        if let Err(e) = rebuild_tap_agg(&mut v) {
            log::warn!("device '{}': tap aggregate failed: {e:#}", dev.id);
            v.zombies = std::mem::take(&mut v.taps);
        }
    }
    Ok(v)
}

unsafe extern "C-unwind" fn ioproc(
    _dev: AudioObjectID,
    now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    it: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    ot: NonNull<AudioTimeStamp>,
    ud: *mut c_void,
) -> i32 {
    let r = catch_unwind(AssertUnwindSafe(|| unsafe {
        let cb = &mut *(ud as *mut Cb);
        let frames = bufs(output.as_ptr()).first().map_or(0, |b| {
            b.mDataByteSize as usize / 4 / (b.mNumberChannels as usize).max(1)
        });
        cb.track_time(ot.as_ref().mSampleTime, frames);
        cb.track_delay(now.as_ref(), it.as_ref(), ot.as_ref());
        run(cb, input.as_ptr(), output.as_ptr())
    }));
    if r.is_err() {
        // A panic may have left half-written output: never hand that to the hardware.
        unsafe { zero_buffers(bufs(output.as_ptr())) };
    }
    0
}

/// Silence every output buffer that has data.
unsafe fn zero_buffers(list: &mut [AudioBuffer]) {
    for b in list {
        if !b.mData.is_null() {
            unsafe { std::ptr::write_bytes(b.mData as *mut u8, 0, b.mDataByteSize as usize) };
        }
    }
}

unsafe fn bufs<'a>(l: *mut AudioBufferList) -> &'a mut [AudioBuffer] {
    unsafe {
        std::slice::from_raw_parts_mut((*l).mBuffers.as_mut_ptr(), (*l).mNumberBuffers as usize)
    }
}

/// Realtime body: no allocation, locks, logging or ObjC.
unsafe fn run(cb: &mut Cb, input: *mut AudioBufferList, output: *mut AudioBufferList) {
    let ins: &mut [AudioBuffer] = if input.is_null() {
        &mut []
    } else {
        unsafe { bufs(input) }
    };
    let outs = unsafe { bufs(output) };
    let key = layout::shape_key(
        ins.iter().map(|b| b.mNumberChannels),
        outs.iter().map(|b| b.mNumberChannels),
    );
    let frames_of =
        |b: &AudioBuffer| b.mDataByteSize as usize / 4 / (b.mNumberChannels as usize).max(1);
    let frames = outs.first().map_or(0, frames_of);
    let n_in: usize = ins.iter().map(|b| b.mNumberChannels as usize).sum();
    let n_out: usize = outs.iter().map(|b| b.mNumberChannels as usize).sum();
    let expected = cb.shared.expected.load(Acquire);
    let ok = key == expected
        && n_in <= MAX_CH
        && n_out <= MAX_CH
        && ins
            .iter()
            .chain(outs.iter())
            .all(|b| b.mNumberChannels > 0 && frames_of(b) == frames);
    if !ok {
        // expected == 0 is deliberate silence while the layout is rebuilt, not a mismatch.
        if expected != 0 {
            cb.meters.mismatch_count.fetch_add(1, Relaxed);
        }
        unsafe { zero_buffers(outs) };
        return;
    }
    let Cb {
        mixer,
        in_planar,
        out_planar,
        tap,
        ..
    } = cb;
    tap.poll();
    // The plan says how many tap channels follow the aggregate's own inputs: they come from the tap
    // ring, or are silent while a new ring or plan is still on its way. An unsplit plan wants none.
    let tap_ch = mixer
        .in_len()
        .map_or(0, |p| p.saturating_sub(n_in))
        .min(MAX_CH - n_in);
    let mut start = 0;
    while start < frames {
        let n = (frames - start).min(MAX_FRAMES);
        let mut c0 = 0;
        for b in ins.iter() {
            let ch = b.mNumberChannels as usize;
            if b.mData.is_null() {
                in_planar[c0 * MAX_FRAMES..(c0 + ch) * MAX_FRAMES].fill(0.0);
            } else {
                let src = unsafe { std::slice::from_raw_parts(b.mData as *const f32, frames * ch) };
                layout::deinterleave(src, ch, start, n, in_planar, c0);
            }
            c0 += ch;
        }
        if tap_ch > 0 {
            tap.pull(tap_ch, n, in_planar, n_in);
        }
        let mut in_refs: [&[f32]; MAX_CH] = [&[]; MAX_CH];
        for (c, r) in in_refs.iter_mut().enumerate().take(n_in + tap_ch) {
            *r = &in_planar[c * MAX_FRAMES..][..n];
        }
        let mut out_refs: [&mut [f32]; MAX_CH] = std::array::from_fn(|_| &mut [][..]);
        for (r, row) in out_refs
            .iter_mut()
            .zip(out_planar.chunks_mut(MAX_FRAMES))
            .take(n_out)
        {
            *r = &mut row[..n];
        }
        mixer.process(&in_refs[..n_in + tap_ch], &mut out_refs[..n_out], n);
        let mut c0 = 0;
        for b in outs.iter() {
            let ch = b.mNumberChannels as usize;
            if !b.mData.is_null() {
                let dst =
                    unsafe { std::slice::from_raw_parts_mut(b.mData as *mut f32, frames * ch) };
                layout::interleave(out_planar, c0, start, n, dst, ch);
            }
            c0 += ch;
        }
        start += n;
    }
}

mod sysvol;
mod tapagg;

#[cfg(test)]
mod tests;

/// Uids whose input streams the vdev's IOProc reads: the main device (Pass-Thru) and Device sources.
/// Monitors are only written to.
fn input_readers(dev: &VirtualDevice, main: &str) -> Vec<String> {
    let mut r: Vec<String> = std::iter::once(main.to_owned())
        .chain(dev.sources.iter().filter_map(|s| match &s.kind {
            SourceKind::Device { uid } => Some(uid.clone()),
            _ => None,
        }))
        .collect();
    r.sort();
    r.dedup();
    r
}

/// One flag per input stream of the aggregate, in stream order (sub-devices in list order): on for the
/// streams of a device that is read. An output-only path must not open a Bluetooth headset's microphone:
/// that would switch it from A2DP to the 16 kHz mono headset profile. An unsplit aggregate carries one
/// input stream per tap after the sub-devices' streams (`n_taps`, 0 when split); the property covers
/// every stream, so those stay on.
fn stream_flags(subs: &[SubDesc], readers: &[String], n_taps: usize) -> Vec<bool> {
    subs.iter()
        .flat_map(|d| {
            let on = readers.contains(&d.uid);
            d.in_bufs.iter().map(move |_| on)
        })
        .chain(std::iter::repeat_n(true, n_taps))
        .collect()
}

/// Switches off the aggregate's input streams nobody reads
/// (`kAudioDevicePropertyIOProcStreamUsage`, input scope, for `proc`). The buffers keep their shape, so
/// the layout is unchanged. Not fatal: the aggregate then simply opens every input as before. Changing
/// an unsplit aggregate's tap list changes its stream count, so call it again then.
fn restrict_inputs(
    agg: u32,
    proc: AudioDeviceIOProcID,
    subs: &[SubDesc],
    readers: &[String],
    n_taps: usize,
) {
    let on = stream_flags(subs, readers, n_taps);
    if on.iter().all(|&b| b) {
        return;
    }
    // struct { void* mIOProc; UInt32 mNumberStreams; UInt32 mStreamIsOn[n]; }
    let mut buf = vec![0u8; 12 + 4 * on.len()];
    let id = proc.map_or(0usize, |f| f as usize);
    buf[..8].copy_from_slice(&id.to_ne_bytes());
    buf[8..12].copy_from_slice(&(on.len() as u32).to_ne_bytes());
    for (i, &b) in on.iter().enumerate() {
        buf[12 + 4 * i..16 + 4 * i].copy_from_slice(&u32::from(b).to_ne_bytes());
    }
    let a = hal::addr(
        kAudioDevicePropertyIOProcStreamUsage,
        kAudioObjectPropertyScopeInput,
    );
    // SAFETY: `buf` holds the struct above and outlives the call.
    let st = unsafe {
        AudioObjectSetPropertyData(
            agg,
            NonNull::from(&a),
            0,
            std::ptr::null(),
            buf.len() as u32,
            NonNull::from(&mut buf[0]).cast(),
        )
    };
    if st != 0 {
        log::warn!(
            "could not switch off unread input streams {on:?}: OSStatus {}",
            hal::fourcc(st)
        );
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn io_state_maps_to_device_state() {
        use TccStatus::*;
        for t in [Denied, Unknown] {
            assert_eq!(
                device_state(IoState::Starting, t),
                DeviceState::WaitingForPermission
            );
        }
        assert_eq!(
            device_state(IoState::Starting, Granted),
            DeviceState::Starting
        );
        assert_eq!(device_state(IoState::Running, Denied), DeviceState::Running);
        assert_eq!(
            device_state(IoState::Failed(-50), Granted),
            DeviceState::Failed("IO start failed (OSStatus -50)".into())
        );
    }
}
