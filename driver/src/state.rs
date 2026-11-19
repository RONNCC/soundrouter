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
// engine companion C at +4 (streams +5 in, +6 out). V's four controls live after all slot blocks:
// `CTRL_BASE + 4s + k` (k = input volume, input mute, output volume, output mute); C has none.
const BASE: u32 = 16;
const STRIDE: u32 = 8;
const CTRL_BASE: u32 = BASE + STRIDE * MAX_DEVICES as u32;

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
    VolumeIn,
    MuteIn,
    VolumeOut,
    MuteOut,
}
const CONTROLS: [Part; 4] = [Part::VolumeIn, Part::MuteIn, Part::VolumeOut, Part::MuteOut];

pub fn encode(slot: usize, side: Side, part: Part) -> u32 {
    if let Some(k) = CONTROLS.iter().position(|&c| c == part) {
        return CTRL_BASE + 4 * slot as u32 + k as u32;
    }
    let side = if side == Side::Engine { 4 } else { 0 };
    BASE + STRIDE * slot as u32 + side + part as u32
}

/// Structure only; callers check `Slot::alive`.
pub fn decode(id: u32) -> Option<(usize, Side, Part)> {
    if let Some(n) = id.checked_sub(CTRL_BASE) {
        let slot = (n / 4) as usize;
        return (slot < MAX_DEVICES).then(|| (slot, Side::Visible, CONTROLS[(n % 4) as usize]));
    }
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

/// Lowest level of V's volume controls; at this value the gain is exactly 0.
pub const MIN_DB: f32 = -64.0;

/// dB is the primary value and scalar its linear-in-dB view (0 -> `MIN_DB`, 1 -> 0 dB), like the
/// HAL's default level-control mapping. `gain` is derived on the (non-real-time) set path so that
/// `do_io` only does one atomic load.
pub fn scalar_to_db(s: f32) -> f32 {
    MIN_DB * (1.0 - s.clamp(0.0, 1.0))
}
pub fn db_to_scalar(db: f32) -> f32 {
    1.0 - db.clamp(MIN_DB, 0.0) / MIN_DB
}

/// Volume + mute of one scope of V. Defaults: 0 dB, unmuted.
pub struct Level {
    db: AtomicU32,
    gain: AtomicU32,
    mute: AtomicU32,
}
impl Level {
    const fn new() -> Self {
        Level {
            db: AtomicU32::new(0),             // 0.0f32 bits
            gain: AtomicU32::new(0x3f80_0000), // 1.0f32 bits
            mute: AtomicU32::new(0),
        }
    }
    pub fn db(&self) -> f32 {
        f32::from_bits(self.db.load(Relaxed))
    }
    pub fn scalar(&self) -> f32 {
        db_to_scalar(self.db())
    }
    /// Out-of-range values clamp; NaN is ignored.
    pub fn set_db(&self, db: f32) {
        if db.is_nan() {
            return;
        }
        let db = db.clamp(MIN_DB, 0.0);
        let gain = if db <= MIN_DB {
            0.0
        } else {
            10f64.powf(db as f64 / 20.0) as f32
        };
        self.gain.store(gain.to_bits(), Relaxed);
        self.db.store(db.to_bits(), Relaxed);
    }
    pub fn set_scalar(&self, s: f32) {
        if !s.is_nan() {
            self.set_db(scalar_to_db(s));
        }
    }
    pub fn muted(&self) -> bool {
        self.mute.load(Relaxed) != 0
    }
    pub fn set_muted(&self, m: bool) {
        self.mute.store(m as u32, Relaxed);
    }
    /// Multiplier for `do_io`: 0 when muted. Real-time safe.
    pub fn effective(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            f32::from_bits(self.gain.load(Relaxed))
        }
    }
}

/// One device pair. Memory lives forever so stale I/O after removal stays memory-safe.
pub struct Slot {
    pub alive: AtomicBool,
    /// Per side (index = `Side as usize`): the HAL changes each device's format separately, while
    /// that device's I/O is stopped, so a device must only ever trust its own side.
    pub channels: [AtomicU32; 2],
    /// Channel count last asked for (the device list, or activation): both sides converge on it.
    pub ch_want: AtomicU32,
    pub rate_idx: [AtomicU32; 2],
    /// Rate index last asked for (by a client, or the default on activation): both sides converge on it.
    pub rate_want: AtomicU32,
    /// Host time (`now_ticks`) when this slot was last removed; 0 = never used. See `config::diff`.
    pub freed_at: AtomicU64,
    pub meta: Mutex<(String, String)>, // (uid, name)
    /// Clients with I/O running per side (index = `Side as usize`).
    pub clients: [AtomicU32; 2],
    /// Shared by V and C (like `seed`): one timeline origin, set when the first client starts. Each
    /// device derives its sample time from its own rate, so V and C report identical timestamps
    /// while their rates agree; see docs/ARCHITECTURE.md, "Clock and zero timestamps".
    pub anchor: AtomicU64,
    pub seed: AtomicU64,
    /// Volume/mute on V, applied when reading: [input scope, output scope]. C has none.
    pub levels: [Level; 2],
    /// WriteMix calls that arrived late or were too big for the ring, per side. Read-only property `srov`.
    pub overloads: [AtomicU64; 2],
    /// V.out -> C.in
    pub ring_a: OnceLock<Ring>,
    /// C.out -> V.in
    pub ring_b: OnceLock<Ring>,
}

impl Slot {
    const fn new() -> Self {
        Slot {
            alive: AtomicBool::new(false),
            channels: [AtomicU32::new(2), AtomicU32::new(2)],
            ch_want: AtomicU32::new(2),
            rate_idx: [AtomicU32::new(0), AtomicU32::new(0)],
            rate_want: AtomicU32::new(0),
            freed_at: AtomicU64::new(0),
            meta: Mutex::new((String::new(), String::new())),
            clients: [AtomicU32::new(0), AtomicU32::new(0)],
            anchor: AtomicU64::new(0),
            seed: AtomicU64::new(0),
            levels: [const { Level::new() }; 2],
            overloads: [AtomicU64::new(0), AtomicU64::new(0)],
            ring_a: OnceLock::new(),
            ring_b: OnceLock::new(),
        }
    }
    pub fn meta(&self) -> MutexGuard<'_, (String, String)> {
        self.meta.lock().unwrap_or_else(PoisonError::into_inner)
    }
    pub fn channels(&self, side: Side) -> u32 {
        self.channels[side as usize].load(Acquire)
    }
    pub fn rate(&self, side: Side) -> f64 {
        SAMPLE_RATES[self.rate_idx[side as usize].load(Acquire) as usize % SAMPLE_RATES.len()]
    }
    /// V and C agree on format. While a change has landed on one side only, I/O must stay silent.
    pub fn in_sync(&self) -> bool {
        self.channels[0].load(Acquire) == self.channels[1].load(Acquire)
            && self.rate_idx[0].load(Acquire) == self.rate_idx[1].load(Acquire)
    }
    /// The per-side values and the wanted value a change of this kind moves.
    fn format(&self, c: &Change) -> (&[AtomicU32; 2], &AtomicU32) {
        match c {
            Change::Channels(_) => (&self.channels, &self.ch_want),
            Change::Rate(_) => (&self.rate_idx, &self.rate_want),
        }
    }
    /// The channel count or rate in `c` is now wanted: record it and return which sides (index =
    /// `Side as usize`) the host must be asked to change. When the wanted value moved, both sides are
    /// asked even if one already runs it: an older request may still be pending, or a Perform may be
    /// running with the old wanted value, and that side must get one more Perform after this set.
    pub fn want(&self, c: &Change) -> [bool; 2] {
        let v = match *c {
            Change::Channels(n) => n as u32,
            Change::Rate(i) => i as u32,
        };
        let (have, want) = self.format(c);
        let prev = want.swap(v, AcqRel);
        [0, 1].map(|s| prev != v || have[s].load(Acquire) != v)
    }
    /// The host performs a change of `c`'s kind on `side`: it converges on the value wanted now, not on
    /// the one in the request, which is stale when the host runs requests late or out of order.
    pub fn perform(&self, side: Side, c: &Change) {
        let (have, want) = self.format(c);
        have[side as usize].store(want.load(Acquire), Release);
    }
    /// Format changes that bring a pair with sides off the wanted channel count or rate back in sync, so
    /// a pass that runs while a change has landed on one side only completes it and never sends the
    /// other side back. Only sides that differ are listed.
    pub fn lagging(&self) -> Vec<(Side, Change)> {
        let mut out = vec![];
        let ch = self.ch_want.load(Acquire);
        for side in [Side::Visible, Side::Engine] {
            if self.channels(side) != ch {
                out.push((side, Change::Channels(ch as u16)));
            }
        }
        let rate = self.rate_want.load(Acquire);
        for side in [Side::Visible, Side::Engine] {
            if self.rate_idx[side as usize].load(Acquire) != rate {
                out.push((side, Change::Rate(rate as usize)));
            }
        }
        out
    }
    /// Mark this slot as just freed: `diff` keeps it out of use until the removal settled.
    pub fn mark_freed(&self) {
        self.freed_at.store(now_ticks(), Release);
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
                // The wanted count, not a side's: `diff` asks again whenever the list moves away from it.
                channels: self.ch_want.load(Acquire) as u16,
            }
        })
    }
    /// Publish a device in this (dead) slot. Not real-time; ring memory is allocated once per slot.
    pub fn activate(&self, d: DriverDevice) {
        self.ring_a.get_or_init(new_ring);
        self.ring_b.get_or_init(new_ring);
        self.reset_rings();
        let default = rate_index(DEFAULT_RATE).unwrap_or(0) as u32;
        for side in 0..2 {
            self.channels[side].store(d.channels as u32, Release);
            self.rate_idx[side].store(default, Release);
        }
        self.ch_want.store(d.channels as u32, Release);
        self.rate_want.store(default, Release);
        for c in &self.clients {
            c.store(0, Release);
        }
        for l in &self.levels {
            l.set_db(0.0);
            l.set_muted(false);
        }
        for o in &self.overloads {
            o.store(0, Relaxed);
        }
        *self.meta() = (d.uid, d.name);
        self.alive.store(true, Release);
    }
}

/// Readable window is half the ring and must cover WriteMix running ~2 client buffers ahead of now plus
/// ReadInput starting ~1 behind, for client buffers up to 4096 frames. Sized for `MAX_CHANNELS`:
/// calloc'd zero pages cost nothing until a channel's samples are first written.
fn new_ring() -> Ring {
    Ring::new(2 * RING_FRAMES as usize, MAX_CHANNELS as usize)
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
            for part in CONTROLS {
                let id = encode(s, Side::Visible, part);
                assert!(seen.insert(id));
                assert_eq!(decode(id), Some((s, Side::Visible, part)));
            }
        }
        assert_eq!(encode(0, Side::Visible, Part::Device), 16);
        assert_eq!(encode(1, Side::Engine, Part::Output), 16 + 8 + 6);
        assert_eq!(encode(1, Side::Visible, Part::MuteOut), CTRL_BASE + 4 + 3);
        for bad in [
            0,
            1,
            15,
            19,
            23,
            CTRL_BASE - 1,
            CTRL_BASE + 4 * MAX_DEVICES as u32,
        ] {
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
    fn ring_window_covers_4096_frame_clients() {
        let ring = new_ring();
        let now = 100_000i64;
        // WriteMix lands 2 buffers ahead of now, in 4096-frame blocks, ending at now + 8192.
        for k in 0..4 {
            ring.write(1, now - 8192 + k * 4096, &[(k + 1) as f32; 4096]);
        }
        // ReadInput starts 512 behind now.
        let mut o = vec![0.0; 512];
        ring.read(1, now - 512, &mut o);
        assert!(o.iter().all(|&x| x == 2.0), "{:?}", &o[..4]);
        let mut o = vec![0.0; 4096];
        ring.read(1, now - 8192, &mut o);
        assert!(o.iter().all(|&x| x == 1.0));
    }

    #[test]
    fn lagging_side_is_asked_again() {
        let s = Slot::new();
        assert!(s.lagging().is_empty());
        s.ch_want.store(8, Release); // the list asked for 8 ch ...
        s.channels[0].store(8, Release); // ... V landed, C did not
        assert_eq!(s.lagging(), vec![(Side::Engine, Change::Channels(8))]);
        s.channels[1].store(8, Release);
        s.rate_want.store(2, Release); // a client asked for rate 2 ...
        s.rate_idx[0].store(2, Release); // ... and it landed on V only
        assert_eq!(s.lagging(), vec![(Side::Engine, Change::Rate(2))]);
        s.channels[0].store(2, Release); // V lags the wanted count instead
        assert_eq!(
            s.lagging(),
            vec![
                (Side::Visible, Change::Channels(8)),
                (Side::Engine, Change::Rate(2))
            ]
        );
    }

    #[test]
    fn a_rate_change_that_landed_on_the_engine_side_first_is_completed_not_undone() {
        // The HAL applies the pair's rate change to C first; a reconcile pass runs before V's lands.
        let s = Slot::new();
        s.rate_want.store(2, Release);
        s.rate_idx[1].store(2, Release);
        assert_eq!(s.lagging(), vec![(Side::Visible, Change::Rate(2))]);
        // The re-request lands: the pair is in sync and nothing more is asked.
        s.rate_idx[0].store(2, Release);
        assert!(s.in_sync() && s.lagging().is_empty());
    }

    /// The host side of a format change: queued per-device requests (with the value each was asked
    /// with), performed later in an order the host chooses.
    struct Host<'a> {
        s: &'a Slot,
        kind: fn(u32) -> Change,
        q: Vec<(Side, Change)>,
    }
    impl Host<'_> {
        /// A client's rate set, or a reconcile pass for a new channel count in the device list.
        fn set(&mut self, v: u32) {
            let ask = self.s.want(&(self.kind)(v));
            for side in [Side::Visible, Side::Engine] {
                if ask[side as usize] {
                    self.q.push((side, (self.kind)(v)));
                }
            }
        }
        fn perform(&mut self, k: usize) {
            let (side, c) = self.q.remove(k % self.q.len());
            self.s.perform(side, &c);
        }
        fn perform_side(&mut self, side: Side) {
            let k = self.q.iter().position(|q| q.0 == side).unwrap();
            self.perform(k);
        }
        fn values(&self) -> [u32; 2] {
            let (have, _) = self.s.format(&(self.kind)(0));
            [0, 1].map(|s| have[s].load(Acquire))
        }
        fn sides(&self) -> Vec<Side> {
            self.q.iter().map(|q| q.0).collect()
        }
    }
    fn rate(i: u32) -> Change {
        Change::Rate(i as usize)
    }
    fn chans(n: u32) -> Change {
        Change::Channels(n as u16)
    }
    /// A host for `s` with both sides at, and wanting, `v`.
    fn host(s: &Slot, kind: fn(u32) -> Change, v: u32) -> Host<'_> {
        s.want(&kind(v));
        s.format(&kind(v))
            .0
            .iter()
            .for_each(|x| x.store(v, Release));
        Host { s, kind, q: vec![] }
    }

    #[test]
    fn the_last_requested_rate_wins_on_both_sides_whatever_order_the_host_performs_in() {
        // 48k -> 96k -> 44.1k -> 96k -> 48k, with the host performing V's first request between sets.
        let s = Slot::new();
        let mut h = host(&s, rate, 1);
        h.set(2);
        h.perform_side(Side::Visible); // V is 96k, C's 96k request is still pending
        h.set(0);
        h.set(2); // back to V's current rate while V's 44.1k request is pending
        h.set(1);
        while !h.q.is_empty() {
            h.perform(0); // FIFO: the stale requests land after the last set
        }
        assert_eq!(h.values(), [1, 1]);
        // Everything deferred, then performed newest first: the oldest (stale) request lands last.
        let s = Slot::new();
        let mut h = host(&s, rate, 1);
        for i in [2, 0, 2, 1] {
            h.set(i);
        }
        while !h.q.is_empty() {
            h.perform(h.q.len() - 1);
        }
        assert_eq!(h.values(), [1, 1]);
    }

    #[test]
    fn the_last_listed_channel_count_wins_on_both_sides_whatever_order_the_host_performs_in() {
        // 2 -> 8 -> 16 -> 8 (-> 16): everything deferred, or V's first request performed between sets
        // (the set back to 8 then finds V at 8), each drained oldest or newest first.
        for seq in [&[8, 16, 8][..], &[8, 16, 8, 16]] {
            for (early, lifo) in [(false, false), (false, true), (true, false), (true, true)] {
                let s = Slot::new();
                let mut h = host(&s, chans, 2);
                for (k, &n) in seq.iter().enumerate() {
                    h.set(n);
                    if early && k == 0 {
                        h.perform_side(Side::Visible);
                    }
                }
                while !h.q.is_empty() {
                    h.perform(if lifo { h.q.len() - 1 } else { 0 });
                }
                let last = seq[seq.len() - 1];
                assert_eq!(
                    h.values(),
                    [last, last],
                    "{seq:?} early={early} lifo={lifo}"
                );
                assert!(s.in_sync() && s.lagging().is_empty());
            }
        }
    }

    #[test]
    fn a_set_racing_a_perform_asks_both_sides_again() {
        for (kind, from, to) in [(rate as fn(u32) -> Change, 1, 2), (chans, 2, 8)] {
            let s = Slot::new();
            let mut h = host(&s, kind, from);
            h.set(to);
            // V's Perform starts on the host's thread and reads the wanted value ...
            let (have, want) = s.format(&kind(0));
            let seen = want.load(Acquire);
            // ... the old value, V's current one, is set again before that Perform stored the new one:
            // V must be asked again.
            assert_eq!(s.want(&kind(from)), [true, true]);
            have[0].store(seen, Release);
            s.perform(Side::Visible, &kind(to));
            s.perform(Side::Engine, &kind(to));
            assert_eq!(h.values(), [from, from]);
            // Setting the wanted value again while both sides run it asks for nothing.
            assert_eq!(s.want(&kind(from)), [false, false]);
        }
    }

    #[test]
    fn setting_the_same_value_again_re_asks_a_side_whose_request_the_host_aborted() {
        for (kind, from, to) in [(rate as fn(u32) -> Change, 1, 2), (chans, 2, 8)] {
            let s = Slot::new();
            let mut h = host(&s, kind, from);
            h.set(to);
            h.q.retain(|q| q.0 == Side::Engine); // the host aborted V's request
            h.perform(0);
            h.set(to);
            assert_eq!(h.sides(), [Side::Visible]);
            h.perform(0);
            assert_eq!(h.values(), [to, to]);
        }
    }

    proptest::proptest! {
        /// Any interleaving of sets and host Performs, then the rest performed in any order: both sides
        /// end at the last requested rate or channel count.
        #[test]
        fn format_converges_to_the_last_set(
            ops in proptest::collection::vec((proptest::bool::ANY, 0usize..3, 0usize..16), 1..24),
            drain in proptest::collection::vec(0usize..16, 32),
            ch in proptest::bool::ANY,
        ) {
            let (kind, vals): (fn(u32) -> Change, _) = if ch { (chans, [2, 8, 16]) } else { (rate, [1, 0, 2]) };
            let s = Slot::new();
            let mut h = host(&s, kind, vals[0]);
            let mut last = vals[0];
            for (set, i, k) in ops {
                if set {
                    h.set(vals[i]);
                    last = vals[i];
                } else if !h.q.is_empty() {
                    h.perform(k);
                }
            }
            for k in drain {
                if !h.q.is_empty() {
                    h.perform(k);
                }
            }
            while !h.q.is_empty() {
                h.perform(0);
            }
            proptest::prop_assert_eq!(h.values(), [last, last]);
        }
    }

    #[test]
    fn rates() {
        assert_eq!(rate_index(48_000.0), Some(1));
        assert_eq!(rate_index(47_999.0), None);
        assert!(ticks_per_second() > 0.0);
    }

    #[test]
    fn level_maps_scalar_and_db_linearly_and_derives_the_gain() {
        assert_eq!(scalar_to_db(1.0), 0.0);
        assert_eq!(scalar_to_db(0.0), MIN_DB);
        assert_eq!(scalar_to_db(0.5), -32.0);
        assert_eq!((db_to_scalar(0.0), db_to_scalar(MIN_DB)), (1.0, 0.0));
        assert_eq!(db_to_scalar(-100.0), 0.0); // out of range clamps
        assert_eq!(db_to_scalar(6.0), 1.0);
        let l = Level::new();
        assert_eq!(
            (l.effective(), l.scalar(), l.db(), l.muted()),
            (1.0, 1.0, 0.0, false)
        );
        l.set_db(-6.0206);
        assert!((l.effective() - 0.5).abs() < 1e-4);
        assert_eq!(l.db(), -6.0206);
        l.set_scalar(0.0);
        assert_eq!((l.db(), l.effective()), (MIN_DB, 0.0)); // bottom of the slider is silence
        l.set_db(f32::NAN);
        l.set_scalar(f32::NAN);
        assert_eq!(l.db(), MIN_DB, "NaN is ignored");
        l.set_db(0.0);
        l.set_muted(true);
        assert_eq!((l.effective(), l.db()), (0.0, 0.0));
        l.set_muted(false);
        assert_eq!(l.effective(), 1.0);
    }

    #[test]
    fn activate_resets_levels_and_counters() {
        let s = Slot::new();
        s.levels[1].set_db(-10.0);
        s.levels[0].set_muted(true);
        s.overloads[1].store(3, Relaxed);
        s.activate(DriverDevice {
            uid: "u".into(),
            name: "n".into(),
            channels: 2,
        });
        assert_eq!(s.levels[1].effective(), 1.0);
        assert_eq!(s.levels[0].effective(), 1.0);
        assert_eq!(s.overloads[1].load(Relaxed), 0);
    }
}
