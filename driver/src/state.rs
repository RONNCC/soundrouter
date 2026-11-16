//! Global driver state: object-id layout, the fixed slot pool, host handle, clock.
use crate::config::MAX_CHANNELS;
use crate::ring::Ring;
use coreaudio_sys::*;
use sr_core::driver_proto::{DEFAULT_RATE, DriverDevice, MAX_DEVICES, SAMPLE_RATES};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering::*};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

pub const PLUGIN: u32 = kAudioObjectPlugInObject;
/// Ring length in frames; also the zero-timestamp period.
pub const RING_FRAMES: u32 = 16384;
/// Fallback switch: `false` makes the engine companion visible (it still can never be a default device).
pub const COMPANION_HIDDEN: bool = true;

// Object ids per slot s: base = 16 + 8s. Visible device V at +0 (streams +1 in, +2 out),
// engine companion C at +4 (streams +5 in, +6 out).
const BASE: u32 = 16;
const STRIDE: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    /// V: what apps see.
    Visible,
    /// C: what the engine sees.
    Engine,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    Device,
    Input,
    Output,
}

pub fn encode(slot: usize, side: Side, part: Part) -> u32 {
    let side = if side == Side::Engine { 4 } else { 0 };
    BASE + STRIDE * slot as u32 + side + part as u32
}

/// Structure only; callers check `Slot::alive`.
pub fn decode(id: u32) -> Option<(usize, Side, Part)> {
    let n = id.checked_sub(BASE)?;
    let slot = (n / STRIDE) as usize;
    let (side, part) = match n % STRIDE {
        0 => (Side::Visible, Part::Device),
        1 => (Side::Visible, Part::Input),
        2 => (Side::Visible, Part::Output),
        4 => (Side::Engine, Part::Device),
        5 => (Side::Engine, Part::Input),
        6 => (Side::Engine, Part::Output),
        _ => return None,
    };
    (slot < MAX_DEVICES).then_some((slot, side, part))
}

/// Like `decode`, but only for a live slot.
pub fn lookup(id: u32) -> Option<(&'static Slot, usize, Side, Part)> {
    let (s, side, part) = decode(id)?;
    SLOTS[s]
        .alive
        .load(Acquire)
        .then(|| (&SLOTS[s], s, side, part))
}

// Device configuration changes are requested with an opaque 64-bit action.
const KIND_CHANNELS: u64 = 1 << 32;
const KIND_RATE: u64 = 2 << 32;
#[derive(Debug, PartialEq)]
pub enum Change {
    Channels(u16),
    Rate(usize),
}
pub fn change_action(c: &Change) -> u64 {
    match *c {
        Change::Channels(n) => KIND_CHANNELS | n as u64,
        Change::Rate(i) => KIND_RATE | i as u64,
    }
}
pub fn parse_action(a: u64) -> Option<Change> {
    let v = a & 0xffff_ffff;
    match a & !0xffff_ffff {
        KIND_CHANNELS if (1..=MAX_CHANNELS as u64).contains(&v) => Some(Change::Channels(v as u16)),
        KIND_RATE if (v as usize) < SAMPLE_RATES.len() => Some(Change::Rate(v as usize)),
        _ => None,
    }
}

pub fn rate_index(rate: f64) -> Option<usize> {
    SAMPLE_RATES.iter().position(|&r| r == rate)
}

/// One device pair. Memory lives forever so stale I/O after removal stays memory-safe.
pub struct Slot {
    pub alive: AtomicBool,
    pub channels: AtomicU32,
    pub rate_idx: AtomicU32,
    pub meta: Mutex<(String, String)>, // (uid, name)
    /// Clients with I/O running per side (index = `Side as usize`).
    pub clients: [AtomicU32; 2],
    /// Shared by V and C so both report identical timestamps. Only set when the first client starts.
    pub anchor: AtomicU64,
    pub seed: AtomicU64,
    /// V.out -> C.in
    pub ring_a: OnceLock<Ring>,
    /// C.out -> V.in
    pub ring_b: OnceLock<Ring>,
}

impl Slot {
    const fn new() -> Self {
        Slot {
            alive: AtomicBool::new(false),
            channels: AtomicU32::new(2),
            rate_idx: AtomicU32::new(0),
            meta: Mutex::new((String::new(), String::new())),
            clients: [AtomicU32::new(0), AtomicU32::new(0)],
            anchor: AtomicU64::new(0),
            seed: AtomicU64::new(0),
            ring_a: OnceLock::new(),
            ring_b: OnceLock::new(),
        }
    }
    pub fn meta(&self) -> MutexGuard<'_, (String, String)> {
        self.meta.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub fn rate(&self) -> f64 {
        SAMPLE_RATES[self.rate_idx.load(Relaxed) as usize % SAMPLE_RATES.len()]
    }
    pub fn ring(&self, a: bool) -> Option<&Ring> {
        if a {
            self.ring_a.get()
        } else {
            self.ring_b.get()
        }
    }
    pub fn reset_rings(&self) {
        for r in [&self.ring_a, &self.ring_b] {
            if let Some(r) = r.get() {
                r.reset();
            }
        }
    }
    pub fn snapshot(&self) -> Option<DriverDevice> {
        self.alive.load(Acquire).then(|| {
            let m = self.meta();
            DriverDevice {
                uid: m.0.clone(),
                name: m.1.clone(),
                channels: self.channels.load(Relaxed) as u16,
            }
        })
    }
    /// Publish a device in this (dead) slot. Not real-time; ring memory is allocated once per slot.
    pub fn activate(&self, d: DriverDevice) {
        self.ring_a
            .get_or_init(|| Ring::new(RING_FRAMES as usize, MAX_CHANNELS as usize));
        self.ring_b
            .get_or_init(|| Ring::new(RING_FRAMES as usize, MAX_CHANNELS as usize));
        self.reset_rings();
        self.channels.store(d.channels as u32, Release);
        let default = rate_index(DEFAULT_RATE).unwrap_or(0) as u32;
        self.rate_idx.store(default, Release);
        for c in &self.clients {
            c.store(0, Release);
        }
        *self.meta() = (d.uid, d.name);
        self.alive.store(true, Release);
    }
}

pub static SLOTS: [Slot; MAX_DEVICES] = [const { Slot::new() }; MAX_DEVICES];

/// Requested device list (what `srcf` returns and what is persisted).
pub static WANTED: Mutex<Vec<DriverDevice>> = Mutex::new(Vec::new());
/// Serialises reconcile passes. Held across host calls, so never taken from an I/O entry point.
pub static APPLY: Mutex<()> = Mutex::new(());
/// Serialises StartIO/StopIO bookkeeping. Never held across a host call.
pub static IO_LOCK: Mutex<()> = Mutex::new(());

pub fn wanted() -> MutexGuard<'static, Vec<DriverDevice>> {
    WANTED.lock().unwrap_or_else(PoisonError::into_inner)
}
pub fn apply_lock() -> MutexGuard<'static, ()> {
    APPLY.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn io_lock() -> MutexGuard<'static, ()> {
    IO_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

static HOST: AtomicPtr<AudioServerPlugInHostInterface> = AtomicPtr::new(std::ptr::null_mut());
pub fn set_host(h: AudioServerPlugInHostRef) {
    HOST.store(h as *mut _, Release);
}
pub fn host() -> Option<AudioServerPlugInHostRef> {
    let h = HOST.load(Acquire);
    (!h.is_null()).then_some(h as AudioServerPlugInHostRef)
}

pub fn addr(sel: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: sel,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Tell the host these properties changed. Must not be called from inside a property call.
pub fn notify(obj: u32, sels: &[u32]) {
    let Some(h) = host() else { return };
    let a: Vec<_> = sels.iter().map(|&s| addr(s)).collect();
    // SAFETY: host pointer came from Initialize and lives as long as the plug-in.
    unsafe {
        if let Some(f) = (*h).PropertiesChanged {
            f(h, obj, a.len() as u32, a.as_ptr());
        }
    }
}

pub fn request_change(dev: u32, c: &Change) {
    let Some(h) = host() else { return };
    // SAFETY: as above.
    unsafe {
        if let Some(f) = (*h).RequestDeviceConfigurationChange {
            f(h, dev, change_action(c), std::ptr::null_mut());
        }
    }
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
}
pub fn now_ticks() -> u64 {
    unsafe { mach_absolute_time() }
}
static TIMEBASE: OnceLock<f64> = OnceLock::new();
/// Host clock ticks per second. Cached; the first call happens in Initialize, off the real-time path.
pub fn ticks_per_second() -> f64 {
    *TIMEBASE.get_or_init(|| {
        let mut tb = [0u32; 2];
        unsafe { mach_timebase_info(&mut tb) };
        if tb[0] == 0 {
            1e9
        } else {
            1e9 * tb[1] as f64 / tb[0] as f64
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_do_not_overlap() {
        let mut seen = std::collections::HashSet::new();
        for s in 0..MAX_DEVICES {
            for side in [Side::Visible, Side::Engine] {
                for part in [Part::Device, Part::Input, Part::Output] {
                    let id = encode(s, side, part);
                    assert!(seen.insert(id));
                    assert_eq!(decode(id), Some((s, side, part)));
                }
            }
        }
        assert_eq!(encode(0, Side::Visible, Part::Device), 16);
        assert_eq!(encode(1, Side::Engine, Part::Output), 16 + 8 + 6);
        for bad in [0, 1, 15, 19, 23, 16 + 8 * MAX_DEVICES as u32] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn actions_round_trip() {
        for c in [
            Change::Channels(1),
            Change::Channels(64),
            Change::Rate(0),
            Change::Rate(2),
        ] {
            assert_eq!(parse_action(change_action(&c)), Some(c));
        }
        assert_eq!(parse_action(KIND_CHANNELS | 65), None);
        assert_eq!(parse_action(KIND_RATE | 3), None);
        assert_eq!(parse_action(5), None);
    }

    #[test]
    fn rates() {
        assert_eq!(rate_index(48_000.0), Some(1));
        assert_eq!(rate_index(47_999.0), None);
        assert!(ticks_per_second() > 0.0);
    }
}
