//! The routing engine: one private aggregate + IOProc per enabled virtual device.
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSNumber, NSString};
use sr_core::backend::{AppInfo, Backend, HwDevice, MeterReading};
use sr_core::config::{Config, MAX_CH, MAX_FRAMES, SourceKind, VirtualDevice};
use sr_core::driver_proto::{COMPANION_SUFFIX, DEVICE_UID_PREFIX, DriverDevice, companion_uid};
use sr_core::meters::Meters;
use sr_core::mixer::{MixPlan, Mixer, MixerHandle, mixer};

use crate::apps::{self, AppGroup};
use crate::driver;
use crate::hal::{self, SYSTEM};
use crate::layout::{self, LayoutSpec, SCRATCH_LEN, SubDesc};

/// If the hidden companion cannot be found or joined, run on the visible vdev instead.
/// shortcut: then pass-thru audio mixes with the engine output in the HAL; unverified until the driver is installed.
pub const ALLOW_VISIBLE_FALLBACK: bool = true;
pub const AGG_UID_PREFIX: &str = "soundrouter.agg.";

/// State of a vdev's IOProc start (AudioDeviceStart blocks while the TCC prompt is open).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoState {
    Starting,
    Running,
    Failed(i32),
}

struct Shared {
    /// Expected `layout::shape_key` of the buffer lists; 0 = never matches (silence).
    expected: AtomicU64,
    /// 0 starting, 1 running, `1<<32 | status` failed.
    state: AtomicU64,
}

/// Audio-thread state; owned via raw pointer handed to the IOProc.
struct Cb {
    mixer: Mixer,
    shared: Arc<Shared>,
    in_planar: Vec<f32>,
    out_planar: Vec<f32>,
}

struct Tap {
    source_id: String,
    bundle: String,
    id: u32,
    uid: String,
    objs: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
struct AggSpec {
    main: String,
    subs: Vec<String>,
    channels: u16,
}

struct Vdev {
    dev: VirtualDevice,
    spec: AggSpec,
    agg: u32,
    proc_id: AudioDeviceIOProcID,
    cb: *mut Cb,
    shared: Arc<Shared>,
    handle: MixerHandle,
    meters: Arc<Meters>,
    layout: sr_core::mixer::IoLayout,
    subs: Vec<SubDesc>,
    taps: Vec<Tap>,
}
// SAFETY: `cb` is only dereferenced by the IOProc thread; control-side access is limited to the atomics in `Shared`.
unsafe impl Send for Vdev {}

impl Drop for Vdev {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.agg, self.proc_id);
            AudioDeviceDestroyIOProcID(self.agg, self.proc_id);
            AudioHardwareDestroyAggregateDevice(self.agg);
            for t in &self.taps {
                AudioHardwareDestroyProcessTap(t.id);
            }
            drop(Box::from_raw(self.cb));
        }
    }
}

#[derive(Default)]
struct Inner {
    cfg: Option<Config>,
    vdevs: HashMap<String, Vdev>,
}

enum Event {
    Devices,
    Processes,
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
    fn new(obj: u32, sel: u32, tx: Sender<Event>, ev: fn() -> Event) -> Result<Listener> {
        let addr = hal::addr(sel, kAudioObjectPropertyScopeGlobal);
        let tx = Mutex::new(tx);
        let block: RcBlock<dyn Fn(u32, NonNull<AudioObjectPropertyAddress>)> =
            RcBlock::new(move |_n: u32, _a: NonNull<AudioObjectPropertyAddress>| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send(ev());
                }
            });
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
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Engine {
    pub fn new() -> Result<Engine> {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let (tx, rx) = channel();
        let listeners = vec![
            Listener::new(SYSTEM, kAudioHardwarePropertyDevices, tx.clone(), || {
                Event::Devices
            })?,
            Listener::new(
                SYSTEM,
                kAudioHardwarePropertyProcessObjectList,
                tx.clone(),
                || Event::Processes,
            )?,
        ];
        let w = inner.clone();
        let worker = std::thread::Builder::new()
            .name("sr-audio-events".into())
            .spawn(move || worker(w, rx))?;
        Ok(Engine {
            inner,
            tx,
            worker: Some(worker),
            listeners,
        })
    }

    pub fn io_state(&self, device_id: &str) -> Option<IoState> {
        let g = lock(&self.inner);
        let v = g.vdevs.get(device_id)?;
        Some(match v.shared.state.load(Relaxed) {
            0 => IoState::Starting,
            1 => IoState::Running,
            s => IoState::Failed(s as u32 as i32),
        })
    }

    /// Blocks silenced by a buffer-shape mismatch (diagnostic).
    pub fn mismatches(&self, device_id: &str) -> u64 {
        lock(&self.inner)
            .vdevs
            .get(device_id)
            .map_or(0, |v| v.meters.mismatch_count())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.listeners.clear();
        let _ = self.tx.send(Event::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        lock(&self.inner).vdevs.clear();
    }
}

fn worker(inner: Arc<Mutex<Inner>>, rx: Receiver<Event>) {
    while let Ok(first) = rx.recv() {
        let (mut devs, mut procs) = (false, false);
        let mut ev = first;
        loop {
            match ev {
                Event::Shutdown => return,
                Event::Devices => devs = true,
                Event::Processes => procs = true,
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(e) => ev = e,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        let mut g = lock(&inner);
        let r = if devs {
            match g.cfg.clone() {
                Some(cfg) => apply_locked(&mut g, &cfg),
                None => Ok(()),
            }
        } else if procs {
            retap(&mut g)
        } else {
            Ok(())
        };
        if let Err(e) = r {
            eprintln!("sr-audio: background update failed: {e:#}");
        }
    }
}

impl Backend for Engine {
    fn apply(&mut self, cfg: &Config) -> Result<()> {
        apply_locked(&mut lock(&self.inner), cfg)
    }

    fn hardware(&self) -> Vec<HwDevice> {
        hal::all_devices()
            .into_iter()
            .filter_map(|d| {
                let uid = hal::device_uid(d);
                if uid.starts_with(DEVICE_UID_PREFIX)
                    || uid.starts_with(AGG_UID_PREFIX)
                    || uid.ends_with(COMPANION_SUFFIX)
                {
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

    fn meters(&self, device_id: &str) -> Option<MeterReading> {
        let g = lock(&self.inner);
        let v = g.vdevs.get(device_id)?;
        Some(v.meters.take(&v.layout, &v.dev))
    }
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
    if let Some(uid) = std::env::var(MAIN_UID_OVERRIDE)
        .ok()
        .filter(|u| !u.is_empty())
    {
        return if hal::device_by_uid(&uid).is_some() {
            vec![uid]
        } else {
            vec![]
        };
    }
    let vis = format!("{DEVICE_UID_PREFIX}{}", dev.id);
    let mut c = vec![companion_uid(&vis)];
    if ALLOW_VISIBLE_FALLBACK {
        c.push(vis);
    }
    c.retain(|u| hal::device_by_uid(u).is_some());
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
    let Some(plugin) = driver::find_plugin() else {
        if want.is_empty() {
            return Ok(());
        }
        driver::require_plugin()?;
        unreachable!()
    };
    if driver::get_devices(plugin).ok().as_ref() != Some(&want) {
        driver::set_devices(plugin, &want)?;
        let uids: Vec<String> = want.iter().map(|d| d.uid.clone()).collect();
        let missing = driver::wait_for_uids(&uids, Duration::from_secs(2));
        if !missing.is_empty() {
            bail!("driver devices did not appear within 2 s: {missing:?}");
        }
        // Hidden companions are best-effort here: `main_candidates` falls back if absent.
        let comps: Vec<String> = uids.iter().map(|u| companion_uid(u)).collect();
        driver::wait_for_uids(&comps, Duration::from_millis(500));
    }
    Ok(())
}

fn apply_locked(g: &mut Inner, cfg: &Config) -> Result<()> {
    cfg.validate().map_err(|e| anyhow!("invalid config: {e}"))?;
    sync_driver(cfg)?;
    let groups = apps::list_groups();
    let enabled: HashSet<&str> = cfg
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| d.id.as_str())
        .collect();
    g.vdevs.retain(|id, _| enabled.contains(id.as_str()));
    let mut errors = Vec::new();
    for dev in cfg.devices.iter().filter(|d| d.enabled) {
        if let Err(e) = apply_vdev(g, dev, &groups) {
            errors.push(format!("device '{}': {e:#}", dev.id));
        }
    }
    g.cfg = Some(cfg.clone());
    if errors.is_empty() {
        Ok(())
    } else {
        bail!("{}", errors.join("; "))
    }
}

fn apply_vdev(g: &mut Inner, dev: &VirtualDevice, groups: &[AppGroup]) -> Result<()> {
    let cands = main_candidates(dev);
    if cands.is_empty() {
        g.vdevs.remove(&dev.id);
        bail!(
            "neither the hidden companion nor the visible device is present (driver not loaded?)"
        );
    }
    let subs = plan_subs(dev, &|u| hal::device_by_uid(u).is_some());
    if let Some(v) = g.vdevs.get_mut(&dev.id) {
        if v.spec.subs == subs && v.spec.channels == dev.channels && cands.contains(&v.spec.main) {
            v.dev = dev.clone();
            sync_taps(v, groups)?;
            return refresh_layout(v);
        }
        g.vdevs.remove(&dev.id);
    }
    let mut last = anyhow!("no candidate main device");
    for main in cands {
        match build_vdev(
            dev,
            AggSpec {
                main,
                subs: subs.clone(),
                channels: dev.channels,
            },
            groups,
        ) {
            Ok(v) => {
                g.vdevs.insert(dev.id.clone(), v);
                return Ok(());
            }
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn retap(g: &mut Inner) -> Result<()> {
    let groups = apps::list_groups();
    for v in g.vdevs.values_mut() {
        sync_taps(v, &groups)?;
    }
    Ok(())
}

fn make_desc(name: &str, objs: &[u32]) -> Retained<CATapDescription> {
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
        desc.setMuteBehavior(CATapMuteBehavior::Unmuted);
    }
    desc
}

fn create_tap(source_id: &str, bundle: &str, objs: Vec<u32>) -> Result<Tap> {
    let desc = make_desc(&format!("sr-{source_id}"), &objs);
    let mut id = 0u32;
    hal::check("AudioHardwareCreateProcessTap", unsafe {
        AudioHardwareCreateProcessTap(Some(&desc), &mut id)
    })?;
    let uid = hal::get_string(id, kAudioTapPropertyUID)
        .map_err(|s| anyhow!("tap uid: {}", hal::fourcc(s)))?;
    Ok(Tap {
        source_id: source_id.into(),
        bundle: bundle.into(),
        id,
        uid,
        objs,
    })
}

fn app_objs(groups: &[AppGroup], bundle: &str) -> Vec<u32> {
    groups
        .iter()
        .find(|g| g.bundle_id == bundle)
        .map(|g| g.objs.clone())
        .unwrap_or_default()
}

fn app_sources(dev: &VirtualDevice) -> impl Iterator<Item = (&str, &str)> {
    dev.sources.iter().filter_map(|s| match &s.kind {
        SourceKind::App { bundle_id } => Some((s.id.as_str(), bundle_id.as_str())),
        _ => None,
    })
}

/// Bring the live taps in line with the config + running processes (live tap-list edit, I/O keeps running).
fn sync_taps(v: &mut Vdev, groups: &[AppGroup]) -> Result<()> {
    let mut changed = false;
    let mut removed = Vec::new();
    let mut i = 0;
    while i < v.taps.len() {
        let t = &v.taps[i];
        let want = app_sources(&v.dev)
            .find(|(id, b)| *id == t.source_id && *b == t.bundle)
            .map(|(_, b)| app_objs(groups, b));
        match want {
            Some(objs) if !objs.is_empty() => {
                if objs != t.objs {
                    let desc = make_desc(&format!("sr-{}", t.source_id), &objs);
                    hal::set_object(t.id, kAudioTapPropertyDescription, &desc)
                        .map_err(|s| anyhow!("set tap description: {}", hal::fourcc(s)))?;
                    v.taps[i].objs = objs;
                }
                i += 1;
            }
            _ => {
                removed.push(v.taps.remove(i));
                changed = true;
            }
        }
    }
    let new: Vec<(String, String, Vec<u32>)> = app_sources(&v.dev)
        .filter(|(id, _)| !v.taps.iter().any(|t| t.source_id == *id))
        .map(|(id, b)| (id.to_owned(), b.to_owned(), app_objs(groups, b)))
        .filter(|(_, _, o)| !o.is_empty())
        .collect();
    for (id, b, objs) in new {
        match create_tap(&id, &b, objs) {
            Ok(t) => {
                v.taps.push(t);
                changed = true;
            }
            Err(e) => eprintln!("sr-audio: tap for '{b}' failed: {e:#}"),
        }
    }
    if changed {
        v.shared.expected.store(0, Relaxed); // silence until the new layout is in
        let uids: Vec<String> = v.taps.iter().map(|t| t.uid.clone()).collect();
        let r = hal::set_object(
            v.agg,
            kAudioAggregateDevicePropertyTapList,
            &hal::string_array(&uids),
        );
        for t in removed {
            unsafe { AudioHardwareDestroyProcessTap(t.id) };
        }
        r.map_err(|s| anyhow!("set aggregate tap list: {}", hal::fourcc(s)))?;
        let want = v.subs.iter().map(|s| s.in_bufs.len()).sum::<usize>() + v.taps.len();
        for _ in 0..50 {
            if hal::stream_channels(v.agg, kAudioObjectPropertyScopeInput).len() == want {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        refresh_layout(v)?;
    }
    Ok(())
}

/// Recompute the layout from the aggregate's current streams, compile the plan, publish both.
fn refresh_layout(v: &mut Vdev) -> Result<()> {
    let ain = hal::stream_channels(v.agg, kAudioObjectPropertyScopeInput);
    let aout = hal::stream_channels(v.agg, kAudioObjectPropertyScopeOutput);
    let passthru = v
        .dev
        .sources
        .iter()
        .find(|s| s.kind == SourceKind::PassThru)
        .map(|s| s.id.clone());
    let device_sources: HashMap<String, String> = v
        .dev
        .sources
        .iter()
        .rev()
        .filter_map(|s| match &s.kind {
            SourceKind::Device { uid } => Some((uid.clone(), s.id.clone())),
            _ => None,
        })
        .collect();
    let monitors: HashSet<String> = v
        .dev
        .monitors
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.device_uid.clone())
        .collect();
    let tap_ids: Vec<String> = v.taps.iter().map(|t| t.source_id.clone()).collect();
    let spec = LayoutSpec {
        subs: &v.subs,
        passthru: passthru.as_deref(),
        device_sources: &device_sources,
        monitors: &monitors,
        taps: &tap_ids,
    };
    let lay = layout::build_layout(&spec, &ain, &aout)?;
    if lay.inputs.len() > MAX_CH || lay.outputs.len() > MAX_CH {
        bail!(
            "aggregate has {} in / {} out channels; limit is {MAX_CH}",
            lay.inputs.len(),
            lay.outputs.len()
        );
    }
    v.handle.set_plan(MixPlan::compile(&v.dev, &lay));
    v.layout = lay;
    v.shared
        .expected
        .store(layout::shape_key_of(&ain, &aout), Relaxed);
    Ok(())
}

fn build_vdev(dev: &VirtualDevice, spec: AggSpec, groups: &[AppGroup]) -> Result<Vdev> {
    let mut taps = Vec::new();
    for (id, b) in app_sources(dev) {
        let objs = app_objs(groups, b);
        if objs.is_empty() {
            continue;
        }
        match create_tap(id, b, objs) {
            Ok(t) => taps.push(t),
            Err(e) => eprintln!("sr-audio: tap for '{b}' failed: {e:#}"),
        }
    }
    let destroy_taps = |taps: &[Tap]| {
        taps.iter()
            .for_each(|t| unsafe { _ = AudioHardwareDestroyProcessTap(t.id) })
    };

    let sub_dicts: Vec<Retained<_>> = std::iter::once((&spec.main, false))
        .chain(spec.subs.iter().map(|s| (s, true)))
        .map(|(uid, drift)| {
            let (u, d) = (NSString::from_str(uid), hal::bool_obj(drift));
            hal::cdict(&[
                (kAudioSubDeviceUIDKey, &u),
                (kAudioSubDeviceDriftCompensationKey, &d),
            ])
        })
        .collect();
    let tap_dicts: Vec<Retained<_>> = taps
        .iter()
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
    let agg_uid = NSString::from_str(&format!("{AGG_UID_PREFIX}{}", dev.id));
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
    let shared = Arc::new(Shared {
        expected: AtomicU64::new(0),
        state: AtomicU64::new(0),
    });
    let cb = Box::into_raw(Box::new(Cb {
        mixer: mx,
        shared: shared.clone(),
        in_planar: vec![0.0; SCRATCH_LEN],
        out_planar: vec![0.0; SCRATCH_LEN],
    }));
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
        taps,
    };
    // From here `Drop` cleans up the aggregate, taps and callback state.
    refresh_layout(&mut v)?;
    let st = unsafe {
        AudioDeviceCreateIOProcID(
            agg,
            Some(ioproc),
            cb as *mut c_void,
            NonNull::from(&mut v.proc_id),
        )
    };
    hal::check("AudioDeviceCreateIOProcID", st).context("creating IOProc")?;
    let (proc_id, sh) = (v.proc_id, v.shared.clone());
    // AudioDeviceStart blocks until the TCC prompt is answered: keep it off the caller's thread.
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
        })?;
    v.handle.flush();
    Ok(v)
}

unsafe extern "C-unwind" fn ioproc(
    _dev: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _it: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _ot: NonNull<AudioTimeStamp>,
    ud: *mut c_void,
) -> i32 {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        run(&mut *(ud as *mut Cb), input.as_ptr(), output.as_ptr())
    }));
    0
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
    let ok = key == cb.shared.expected.load(Relaxed)
        && n_in <= MAX_CH
        && n_out <= MAX_CH
        && ins
            .iter()
            .chain(outs.iter())
            .all(|b| b.mNumberChannels > 0 && frames_of(b) == frames);
    if !ok {
        for b in outs.iter() {
            if !b.mData.is_null() {
                unsafe { std::ptr::write_bytes(b.mData as *mut u8, 0, b.mDataByteSize as usize) };
            }
        }
        return;
    }
    let Cb {
        mixer,
        in_planar,
        out_planar,
        ..
    } = cb;
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
        let mut in_refs: [&[f32]; MAX_CH] = [&[]; MAX_CH];
        for (c, r) in in_refs.iter_mut().enumerate().take(n_in) {
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
        mixer.process(&in_refs[..n_in], &mut out_refs[..n_out], n);
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

#[cfg(test)]
mod tests {
    use super::*;
    use sr_core::config::{Monitor, Source};

    fn src(id: &str, kind: SourceKind) -> Source {
        Source {
            id: id.into(),
            kind,
            channels: 2,
            volume_db: 0.0,
            muted: false,
        }
    }
    fn mon(uid: &str, enabled: bool) -> Monitor {
        Monitor {
            device_uid: uid.into(),
            enabled,
            volume_db: 0.0,
            wires: vec![],
        }
    }

    #[test]
    fn subs_deduped_and_filtered() {
        let dev = VirtualDevice {
            id: "v1".into(),
            name: "n".into(),
            enabled: true,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            sources: vec![
                src("a", SourceKind::Device { uid: "mic".into() }),
                src("b", SourceKind::Device { uid: "gone".into() }),
                src("c", SourceKind::PassThru),
            ],
            wires: vec![],
            monitors: vec![mon("spk", true), mon("mic", true), mon("off", false)],
        };
        assert_eq!(plan_subs(&dev, &|u| u != "gone"), ["mic", "spk"]);
    }
}
