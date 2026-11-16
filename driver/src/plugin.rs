//! Non-real-time vtable entries: Initialize, property get/set, configuration changes, reconcile.
use crate::cf::{cfstr, devices_to_plist, parse_devices, rstr};
use crate::config::{Op, diff};
use crate::props::*;
use crate::state::*;
use coreaudio_sys::*;
use sr_core::driver_proto::{DriverDevice, STORAGE_KEY};
use std::ffi::c_void;
use std::mem::size_of;
use std::sync::atomic::Ordering::*;

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

/// Make the slots match `WANTED`. With `announce` the host is told (worker threads only);
/// without it (Initialize) devices are simply there when the host first looks.
fn reconcile(announce: bool) {
    let _g = apply_lock();
    let want = wanted().clone();
    let cur: Vec<_> = SLOTS.iter().map(Slot::snapshot).collect();
    let mut list_changed = false;
    for op in diff(&cur, &want) {
        match op {
            Op::Remove(s) => {
                SLOTS[s].alive.store(false, Release);
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
            Op::Channels(s, n) if announce => {
                request_change(encode(s, Side::Visible, Part::Device), &Change::Channels(n))
            }
            Op::Channels(s, n) => SLOTS[s].channels.store(n as u32, Release),
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
}

unsafe fn store(list: &[DriverDevice]) {
    let Some(h) = host() else { return };
    unsafe {
        let key = cfstr(STORAGE_KEY);
        let plist = devices_to_plist(list);
        if let Some(f) = (*h).WriteToStorage {
            f(h, key, plist as CFPropertyListRef);
        }
        CFRelease(plist as CFTypeRef);
        CFRelease(key as CFTypeRef);
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
        let key = cfstr(STORAGE_KEY);
        let st = (*host)
            .CopyFromStorage
            .map_or(-1, |f| f(host, key, &mut stored));
        CFRelease(key as CFTypeRef);
        let parsed = (st == 0 && !stored.is_null()).then(|| parse_devices(stored as CFTypeRef));
        if !stored.is_null() {
            CFRelease(stored as CFTypeRef);
        }
        match parsed {
            Some(Ok(l)) => l,
            _ => default_devices(), // nothing stored, or unreadable
        }
    };
    *wanted() = list;
    reconcile(false);
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
    let (Some((slot, ..)), Some(change)) = (lookup(dev), parse_action(action)) else {
        return BAD_OBJECT;
    };
    match change {
        Change::Channels(n) => slot.channels.store(n as u32, Release),
        Change::Rate(i) => slot.rate_idx.store(i as u32, Release),
    }
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
            V::F64(x) => copy((&raw const x).cast()),
            V::Pair(x) => copy(x.as_ptr().cast()),
            V::Asbd(x) => copy((&raw const x).cast()),
            V::Ranged(x) => copy(x.as_ptr().cast()),
            V::Ranges(x) => copy(x.as_ptr().cast()),
            V::Ids(x) => copy(x.as_ptr().cast()),
            V::Custom(x) => copy(x.as_ptr().cast()),
            V::Str(s) => *(out as *mut CFStringRef) = cfstr(&s),
            V::DeviceList => *(out as *mut CFTypeRef) = devices_to_plist(&wanted()) as CFTypeRef,
            V::TranslateUid => {
                let want = if qsz as usize == size_of::<CFStringRef>() && !q.is_null() {
                    rstr(*(q as *const CFTypeRef))
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
        return set_devices(sz, data);
    }
    let Some((slot, s, ..)) = lookup(obj) else {
        return BAD_OBJECT;
    };
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
        let want = asbd(slot.channels.load(Acquire), f.mSampleRate);
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
    if i as u32 != slot.rate_idx.load(Acquire) {
        request_change(encode(s, Side::Visible, Part::Device), &Change::Rate(i));
    }
    0
}

/// Replace the device list: validate, persist, then apply on a worker thread
/// (host notifications must not come from inside a property call).
fn set_devices(sz: u32, data: *const c_void) -> OSStatus {
    if sz as usize != size_of::<CFPropertyListRef>() {
        return BAD_SIZE;
    }
    let Ok(list) = (unsafe { parse_devices(*(data as *const CFTypeRef)) }) else {
        return ILLEGAL;
    };
    {
        let mut w = wanted();
        if *w == list {
            return 0;
        }
        unsafe { store(&list) };
        *w = list;
    }
    let _ = std::thread::Builder::new()
        .name("sr-reconcile".into())
        .spawn(|| reconcile(true));
    0
}
