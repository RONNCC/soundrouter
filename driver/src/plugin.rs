//! Non-real-time vtable entries: Initialize, property get/set, configuration changes, reconcile.
use crate::cf::{borrow, devices_to_plist, parse_devices, rstr};
use crate::config::{Op, diff};
use crate::props::*;
use crate::state::*;
use coreaudio_sys::*;
use objc2_core_foundation::{CFNumber, CFRetained, CFString, CFType};
use sr_core::driver_proto::{DriverDevice, MAX_DEVICES, PROP_READ_AHEAD, STORAGE_KEY};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::*;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

const UNKNOWN_PROP: OSStatus = kAudioHardwareUnknownPropertyError as OSStatus;
const BAD_SIZE: OSStatus = kAudioHardwareBadPropertySizeError as OSStatus;
const BAD_OBJECT: OSStatus = kAudioHardwareBadObjectError as OSStatus;
const ILLEGAL: OSStatus = kAudioHardwareIllegalOperationError as OSStatus;

pub fn default_devices() -> Vec<DriverDevice> {
    vec![DriverDevice {
        uid: "soundrouter:v1".into(),
        name: "Loopback Audio".into(),
        channels: 2,
    }]
}

/// How long the HAL gets to tear removed devices down before their slots are handed out again.
const TEARDOWN: std::time::Duration = std::time::Duration::from_secs(1);

/// Make the slots match `WANTED`. A freed slot is quarantined for `TEARDOWN`, so when wanted devices are
/// left over, the removals are announced, `settle` runs (the HAL tears the old devices down) and another
/// pass places the rest. Bounded: a pass after the quarantine always fits everything wanted.
fn reconcile(announce: bool, settle: impl Fn()) {
    let mut left = pass(announce);
    for _ in 0..4 {
        if !(left && announce) {
            return;
        }
        settle();
        left = pass(true);
    }
}

/// Reconcile requests for the single worker: `pending` coalesces requests (each run applies the newest
/// `WANTED`), `running` says the worker thread exists. One worker means passes never race each other.
struct Work {
    pending: bool,
    running: bool,
}
static WORK: Mutex<Work> = Mutex::new(Work {
    pending: false,
    running: false,
});
static WAKE: Condvar = Condvar::new();

fn work() -> MutexGuard<'static, Work> {
    WORK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn worker() {
    loop {
        {
            let mut w = work();
            while !w.pending {
                w = WAKE.wait(w).unwrap_or_else(PoisonError::into_inner);
            }
            w.pending = false;
        }
        reconcile(true, || std::thread::sleep(TEARDOWN));
    }
}

/// One pass. Returns true when wanted devices are still without a slot.
fn pass(announce: bool) -> bool {
    let _g = apply_lock();
    let want = wanted().clone();
    let cur: Vec<_> = SLOTS.iter().map(Slot::snapshot).collect();
    let freed: Vec<_> = SLOTS.iter().map(|s| s.freed_at.load(Acquire)).collect();
    let settle = (TEARDOWN.as_secs_f64() * ticks_per_second()) as u64;
    let mut list_changed = false;
    let mut changing = [false; MAX_DEVICES];
    for op in diff(&cur, &freed, now_ticks(), settle, &want) {
        match op {
            Op::Remove(s) => {
                SLOTS[s].alive.store(false, Release);
                SLOTS[s].mark_freed();
                list_changed = true;
            }
            Op::Rename(s, name) => {
                SLOTS[s].meta().1 = name;
                if announce {
                    for side in [Side::Visible, Side::Engine] {
                        notify(encode(s, side, Part::Device), &[kAudioObjectPropertyName]);
                    }
                }
            }
            // The HAL applies a change per device, so both must be asked; until both land, I/O is silent.
            Op::Channels(s, n) if announce => {
                changing[s] = true;
                let c = Change::Channels(n);
                let ask = SLOTS[s].want(&c);
                for side in [Side::Visible, Side::Engine] {
                    if ask[side as usize] {
                        request_change(encode(s, side, Part::Device), &c);
                    }
                }
            }
            Op::Channels(s, n) => {
                SLOTS[s].ch_want.store(n as u32, Release);
                for c in &SLOTS[s].channels {
                    c.store(n as u32, Release);
                }
            }
            Op::Add(s, d) => {
                SLOTS[s].activate(d);
                list_changed = true;
            }
        }
    }
    if announce && list_changed {
        notify(
            PLUGIN,
            &[
                kAudioObjectPropertyOwnedObjects,
                kAudioPlugInPropertyDeviceList,
            ],
        );
    }
    if announce {
        // A change the HAL applied to one side only leaves the pair silent for good: ask the laggard again.
        for (s, slot) in SLOTS.iter().enumerate() {
            if !slot.alive.load(Acquire) || changing[s] {
                continue;
            }
            for (side, c) in slot.lagging() {
                request_change(encode(s, side, Part::Device), &c);
            }
        }
    }
    let have: Vec<_> = SLOTS.iter().filter_map(Slot::snapshot).collect();
    want.iter().any(|w| have.iter().all(|h| h.uid != w.uid))
}

unsafe fn store(list: &[DriverDevice]) {
    let Some(h) = host() else { return };
    unsafe {
        let key = CFString::from_static_str(STORAGE_KEY);
        let plist = devices_to_plist(list);
        if let Some(f) = (*h).WriteToStorage {
            f(
                h,
                (&*key as *const CFString).cast(),
                (&*plist as *const _ as *const c_void).cast(),
            );
        }
    }
}

pub unsafe extern "C" fn initialize(
    _: AudioServerPlugInDriverRef,
    host: AudioServerPlugInHostRef,
) -> OSStatus {
    if host.is_null() {
        return ILLEGAL;
    }
    set_host(host);
    ticks_per_second();
    let list = unsafe {
        let mut stored: CFPropertyListRef = std::ptr::null();
        let key = CFString::from_static_str(STORAGE_KEY);
        let st = (*host).CopyFromStorage.map_or(-1, |f| {
            f(host, (&*key as *const CFString).cast(), &mut stored)
        });
        // CopyFromStorage returns +1; wrap exactly once so Drop releases it.
        let owned = NonNull::new(stored as *mut CFType).map(|p| CFRetained::from_raw(p));
        match (st == 0).then(|| parse_devices(owned.as_deref())) {
            Some(Ok(l)) => l,
            _ => default_devices(), // nothing stored, or unreadable
        }
    };
    *wanted() = list;
    reconcile(false, || {});
    0
}

pub unsafe extern "C" fn create_device(
    _: AudioServerPlugInDriverRef,
    _: CFDictionaryRef,
    _: *const AudioServerPlugInClientInfo,
    _: *mut AudioObjectID,
) -> OSStatus {
    kAudioHardwareUnsupportedOperationError as OSStatus
}
pub unsafe extern "C" fn destroy_device(
    _: AudioServerPlugInDriverRef,
    _: AudioObjectID,
) -> OSStatus {
    kAudioHardwareUnsupportedOperationError as OSStatus
}
pub unsafe extern "C" fn device_client(
    _: AudioServerPlugInDriverRef,
    _: AudioObjectID,
    _: *const AudioServerPlugInClientInfo,
) -> OSStatus {
    0
}

/// Called by the host with I/O stopped.
pub unsafe extern "C" fn perform_change(
    _: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    action: u64,
    _: *mut c_void,
) -> OSStatus {
    let (Some((slot, _, side, _)), Some(change)) = (lookup(dev), parse_action(action)) else {
        return BAD_OBJECT;
    };
    slot.perform(side, &change);
    slot.reset_rings();
    0
}
pub unsafe extern "C" fn abort_change(
    _: AudioServerPlugInDriverRef,
    _: AudioObjectID,
    _: u64,
    _: *mut c_void,
) -> OSStatus {
    0
}

pub unsafe extern "C" fn has_property(
    _: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _: pid_t,
    a: *const AudioObjectPropertyAddress,
) -> Boolean {
    (!a.is_null() && prop(obj, unsafe { &*a }).is_some()) as Boolean
}

pub unsafe extern "C" fn is_settable(
    _: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _: pid_t,
    a: *const AudioObjectPropertyAddress,
    out: *mut Boolean,
) -> OSStatus {
    if a.is_null() || out.is_null() {
        return BAD_OBJECT;
    }
    let a = unsafe { &*a };
    if prop(obj, a).is_none() {
        return UNKNOWN_PROP;
    }
    unsafe { *out = settable(obj, a.mSelector) as Boolean };
    0
}

pub unsafe extern "C" fn get_size(
    _: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _: pid_t,
    a: *const AudioObjectPropertyAddress,
    _: u32,
    _: *const c_void,
    out: *mut u32,
) -> OSStatus {
    if a.is_null() || out.is_null() {
        return BAD_OBJECT;
    }
    match prop(obj, unsafe { &*a }) {
        Some(v) => {
            unsafe { *out = vsize(&v) as u32 };
            0
        }
        None => UNKNOWN_PROP,
    }
}

pub unsafe extern "C" fn get_data(
    _: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _: pid_t,
    a: *const AudioObjectPropertyAddress,
    qsz: u32,
    q: *const c_void,
    sz: u32,
    out_sz: *mut u32,
    out: *mut c_void,
) -> OSStatus {
    if a.is_null() || out.is_null() || out_sz.is_null() {
        return BAD_OBJECT;
    }
    let Some(v) = prop(obj, unsafe { &*a }) else {
        return UNKNOWN_PROP;
    };
    let mut n = vsize(&v);
    if n > sz as usize {
        if !is_array(&v) {
            return BAD_SIZE;
        }
        n = sz as usize;
    }
    let elem = match &v {
        V::Ids(_) => 4,
        V::Ranged(_) => size_of::<AudioStreamRangedDescription>(),
        V::Ranges(_) => size_of::<AudioValueRange>(),
        V::Custom(_) => size_of::<AudioServerPlugInCustomPropertyInfo>(),
        _ => 1,
    };
    n -= n % elem;
    let copy = |p: *const u8| unsafe { std::ptr::copy_nonoverlapping(p, out as *mut u8, n) };
    unsafe {
        match v {
            V::U32(x) => copy((&raw const x).cast()),
            V::F32(x) => copy((&raw const x).cast()),
            V::F64(x) => copy((&raw const x).cast()),
            V::Pair(x) => copy(x.as_ptr().cast()),
            V::Asbd(x) => copy((&raw const x).cast()),
            V::Ranged(x) => copy(x.as_ptr().cast()),
            V::Ranges(x) => copy(x.as_ptr().cast()),
            V::Ids(x) => copy(x.as_ptr().cast()),
            V::Custom(x) => copy(x.as_ptr().cast()),
            V::Str(s) => {
                *(out as *mut CFStringRef) =
                    CFRetained::into_raw(CFString::from_str(&s)).as_ptr() as CFStringRef
            }
            V::DeviceList => {
                *(out as *mut CFTypeRef) =
                    CFRetained::into_raw(devices_to_plist(&wanted())).as_ptr() as CFTypeRef
            }
            V::Count(n) => {
                *(out as *mut CFTypeRef) =
                    CFRetained::into_raw(CFNumber::new_i64(n as i64)).as_ptr() as CFTypeRef
            }
            // The caller passes the value to convert in its output buffer and gets the result back there.
            V::ScalarToDb => *(out as *mut f32) = scalar_to_db(*(out as *const f32)),
            V::DbToScalar => *(out as *mut f32) = db_to_scalar(*(out as *const f32)),
            V::TranslateUid => {
                let want = if qsz as usize == size_of::<CFStringRef>() && !q.is_null() {
                    rstr(borrow(*(q as *const CFTypeRef)))
                } else {
                    None
                };
                *(out as *mut u32) = want
                    .and_then(|u| find_uid(&u))
                    .unwrap_or(kAudioObjectUnknown);
            }
        }
        *out_sz = n as u32;
    }
    0
}

pub unsafe extern "C" fn set_data(
    _: AudioServerPlugInDriverRef,
    obj: AudioObjectID,
    _: pid_t,
    a: *const AudioObjectPropertyAddress,
    _: u32,
    _: *const c_void,
    sz: u32,
    data: *const c_void,
) -> OSStatus {
    if a.is_null() || data.is_null() {
        return BAD_OBJECT;
    }
    let sel = unsafe { (*a).mSelector };
    if prop(obj, unsafe { &*a }).is_none() || !settable(obj, sel) {
        return UNKNOWN_PROP;
    }
    if obj == PLUGIN {
        return if sel == PROP_READ_AHEAD {
            set_read_ahead_prop(sz, data)
        } else {
            set_devices(sz, data)
        };
    }
    let Some((slot, s, side, part)) = lookup(obj) else {
        return BAD_OBJECT;
    };
    if let Some((_, scope)) = control_kind(part) {
        return set_control(&slot.levels[scope], obj, sel, sz, data);
    }
    let rate = if sel == kAudioDevicePropertyNominalSampleRate {
        if sz as usize != size_of::<f64>() {
            return BAD_SIZE;
        }
        unsafe { *(data as *const f64) }
    } else {
        if sz as usize != size_of::<AudioStreamBasicDescription>() {
            return BAD_SIZE;
        }
        let f = unsafe { *(data as *const AudioStreamBasicDescription) };
        let want = asbd(slot.channels(side), f.mSampleRate);
        let same = f.mFormatID == want.mFormatID
            && f.mFormatFlags == want.mFormatFlags
            && f.mChannelsPerFrame == want.mChannelsPerFrame
            && f.mBytesPerFrame == want.mBytesPerFrame
            && f.mBitsPerChannel == want.mBitsPerChannel;
        if !same {
            return kAudioDeviceUnsupportedFormatError as OSStatus;
        }
        f.mSampleRate
    };
    let Some(i) = rate_index(rate) else {
        return kAudioDeviceUnsupportedFormatError as OSStatus;
    };
    // Both devices of the pair must follow, or one side would run at a different rate. A reconcile pass
    // that runs before both changes landed re-asks the laggard for this rate (`Slot::lagging`).
    let c = Change::Rate(i);
    let ask = slot.want(&c);
    for side in [Side::Visible, Side::Engine] {
        if ask[side as usize] {
            request_change(encode(s, side, Part::Device), &c);
        }
    }
    0
}

/// Volume and mute writes take effect on the next I/O cycle. The host is told which values moved with
/// them: a scalar write changes the dB value too, and vice versa.
fn set_control(level: &Level, obj: u32, sel: u32, sz: u32, data: *const c_void) -> OSStatus {
    if sz as usize != 4 {
        return BAD_SIZE;
    }
    let changed: &[u32] = match sel {
        kAudioLevelControlPropertyScalarValue | kAudioLevelControlPropertyDecibelValue => {
            let v = unsafe { std::ptr::read_unaligned(data as *const f32) };
            if sel == kAudioLevelControlPropertyScalarValue {
                level.set_scalar(v)
            } else {
                level.set_db(v)
            }
            &[
                kAudioLevelControlPropertyScalarValue,
                kAudioLevelControlPropertyDecibelValue,
            ]
        }
        _ => {
            level.set_muted(unsafe { std::ptr::read_unaligned(data as *const u32) } != 0);
            &[kAudioBooleanControlPropertyValue]
        }
    };
    notify(obj, changed);
    0
}

/// Switch the loop read-ahead on (any nonzero integer) or off (0). Takes effect on the next I/O cycle.
fn set_read_ahead_prop(sz: u32, data: *const c_void) -> OSStatus {
    if sz as usize != size_of::<CFTypeRef>() {
        return BAD_SIZE;
    }
    let on = unsafe { borrow(*(data as *const CFTypeRef)) }
        .and_then(|n| n.downcast_ref::<CFNumber>())
        .and_then(|n| n.as_i64());
    match on {
        Some(v) => {
            set_read_ahead(v != 0);
            0
        }
        None => ILLEGAL,
    }
}

/// Replace the device list: validate, persist, then wake the reconcile worker
/// (host notifications must not come from inside a property call).
fn set_devices(sz: u32, data: *const c_void) -> OSStatus {
    if sz as usize != size_of::<CFPropertyListRef>() {
        return BAD_SIZE;
    }
    let Ok(list) = (unsafe { parse_devices(borrow(*(data as *const CFTypeRef))) }) else {
        return ILLEGAL;
    };
    {
        let mut w = wanted();
        // The same list again is not stored, but still runs a pass: that re-asks a side whose format change the
        // host aborted, which otherwise leaves the pair silent until some other list is set.
        if *w != list {
            unsafe { store(&list) };
            *w = list;
        }
    }
    let mut w = work();
    w.pending = true;
    if !w.running {
        w.running = std::thread::Builder::new()
            .name("sr-reconcile".into())
            .spawn(worker)
            .is_ok();
    }
    WAKE.notify_one();
    0
}
