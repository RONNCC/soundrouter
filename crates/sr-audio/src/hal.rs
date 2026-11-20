//! Thin typed wrappers over the AudioObject property API.
use std::ffi::{CStr, c_void};
use std::ptr::{NonNull, null};

use anyhow::{Result, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

pub const SYSTEM: u32 = kAudioObjectSystemObject as u32;

pub fn addr(sel: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: sel,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// `'abcd'` rendering for OSStatus values.
pub fn fourcc(st: i32) -> String {
    let b = (st as u32).to_be_bytes();
    if b.iter().all(|c| c.is_ascii_graphic()) {
        format!("{st} '{}'", String::from_utf8_lossy(&b))
    } else {
        st.to_string()
    }
}

pub fn check(what: &str, st: i32) -> Result<()> {
    if st != 0 {
        bail!("{what} failed: OSStatus {}", fourcc(st));
    }
    Ok(())
}

/// Fixed-size property read.
///
/// # Safety
/// `T` must be plain data valid for any bit pattern.
pub unsafe fn get<T: Copy>(
    obj: u32,
    sel: u32,
    scope: u32,
    qualifier: Option<&[u8]>,
) -> Result<T, i32> {
    let a = addr(sel, scope);
    let mut v = std::mem::MaybeUninit::<T>::zeroed();
    let mut sz = std::mem::size_of::<T>() as u32;
    let (qs, qp) = qualifier.map_or((0, null()), |q| {
        (q.len() as u32, q.as_ptr() as *const c_void)
    });
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            qs,
            qp,
            NonNull::from(&mut sz),
            NonNull::new_unchecked(v.as_mut_ptr() as *mut c_void),
        )
    };
    if st != 0 {
        Err(st)
    } else {
        Ok(unsafe { v.assume_init() })
    }
}

pub fn get_global<T: Copy>(obj: u32, sel: u32) -> Result<T, i32> {
    unsafe { get(obj, sel, kAudioObjectPropertyScopeGlobal, None) }
}

pub fn get_u32s(obj: u32, sel: u32) -> Result<Vec<u32>, i32> {
    get_u32s_in(obj, sel, kAudioObjectPropertyScopeGlobal)
}

pub fn get_u32s_in(obj: u32, sel: u32, scope: u32) -> Result<Vec<u32>, i32> {
    let a = addr(sel, scope);
    let mut sz = 0u32;
    let st = unsafe {
        AudioObjectGetPropertyDataSize(obj, NonNull::from(&a), 0, null(), NonNull::from(&mut sz))
    };
    if st != 0 {
        return Err(st);
    }
    let mut v = vec![0u32; sz as usize / 4];
    if v.is_empty() {
        return Ok(v);
    }
    let st = unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            null(),
            NonNull::from(&mut sz),
            NonNull::new_unchecked(v.as_mut_ptr() as *mut c_void),
        )
    };
    if st != 0 {
        return Err(st);
    }
    v.truncate(sz as usize / 4);
    Ok(v)
}

/// CFString property (toll-free bridged); the HAL hands over +1.
pub fn get_string(obj: u32, sel: u32) -> Result<String, i32> {
    let p: *mut NSString = get_global(obj, sel)?;
    Ok(unsafe { Retained::from_raw(p) }
        .map(|s| s.to_string())
        .unwrap_or_default())
}

/// Set a fixed-size global property.
pub fn set_global<T: Copy>(obj: u32, sel: u32, value: &T) -> Result<(), i32> {
    let a = addr(sel, kAudioObjectPropertyScopeGlobal);
    let st = unsafe {
        AudioObjectSetPropertyData(
            obj,
            NonNull::from(&a),
            0,
            null(),
            std::mem::size_of::<T>() as u32,
            NonNull::from(value).cast(),
        )
    };
    if st != 0 { Err(st) } else { Ok(()) }
}

/// Set a property whose value is one object pointer (CFString, CFArray, ...).
pub fn set_object<T: objc2::Message>(obj: u32, sel: u32, value: &Retained<T>) -> Result<(), i32> {
    set_global(obj, sel, &Retained::as_ptr(value))
}

/// Qualifier bytes for a CFString-keyed translation (the pointer value).
fn str_qualifier(s: &NSString) -> [u8; std::mem::size_of::<usize>()] {
    (s as *const NSString as usize).to_ne_bytes()
}

pub fn device_by_uid(uid: &str) -> Option<u32> {
    let s = NSString::from_str(uid);
    let id: u32 = unsafe {
        get(
            SYSTEM,
            kAudioHardwarePropertyTranslateUIDToDevice,
            kAudioObjectPropertyScopeGlobal,
            Some(&str_qualifier(&s)),
        )
    }
    .ok()?;
    (id != kAudioObjectUnknown).then_some(id)
}

pub fn plugin_by_bundle_id(bundle: &str) -> Option<u32> {
    let s = NSString::from_str(bundle);
    let id: u32 = unsafe {
        get(
            SYSTEM,
            kAudioHardwarePropertyTranslateBundleIDToPlugIn,
            kAudioObjectPropertyScopeGlobal,
            Some(&str_qualifier(&s)),
        )
    }
    .ok()?;
    (id != kAudioObjectUnknown).then_some(id)
}

pub fn all_devices() -> Vec<u32> {
    get_u32s(SYSTEM, kAudioHardwarePropertyDevices).unwrap_or_default()
}

pub fn device_uid(dev: u32) -> String {
    get_string(dev, kAudioDevicePropertyDeviceUID).unwrap_or_default()
}

pub fn device_name(dev: u32) -> String {
    get_string(dev, kAudioObjectPropertyName).unwrap_or_default()
}

/// `kAudioDevicePropertyDeviceIsAlive`; a device that cannot be asked is not alive.
pub fn is_alive(dev: u32) -> bool {
    get_global::<u32>(dev, kAudioDevicePropertyDeviceIsAlive).is_ok_and(|a| a != 0)
}

pub fn nominal_rate(dev: u32) -> f64 {
    get_global(dev, kAudioDevicePropertyNominalSampleRate).unwrap_or(0.0)
}

/// Channels per stream buffer, in buffer order (`scope` = input or output).
pub fn stream_channels(dev: u32, scope: u32) -> Vec<u16> {
    let a = addr(kAudioDevicePropertyStreamConfiguration, scope);
    let mut sz = 0u32;
    if unsafe {
        AudioObjectGetPropertyDataSize(dev, NonNull::from(&a), 0, null(), NonNull::from(&mut sz))
    } != 0
        || sz == 0
    {
        return vec![];
    }
    // At least one whole `AudioBufferList`, so the reference below never covers unallocated bytes.
    let len = (sz as usize).max(std::mem::size_of::<AudioBufferList>());
    let mut buf = vec![0u64; len.div_ceil(8)];
    let st = unsafe {
        AudioObjectGetPropertyData(
            dev,
            NonNull::from(&a),
            0,
            null(),
            NonNull::from(&mut sz),
            NonNull::new_unchecked(buf.as_mut_ptr() as *mut c_void),
        )
    };
    if st != 0 {
        return vec![];
    }
    let abl = unsafe { &*(buf.as_ptr() as *const AudioBufferList) };
    // Never trust `mNumberBuffers` beyond the bytes actually returned (the list can change between the calls).
    let fit = (sz as usize).saturating_sub(std::mem::offset_of!(AudioBufferList, mBuffers))
        / std::mem::size_of::<AudioBuffer>();
    let n = (abl.mNumberBuffers as usize).min(fit);
    let bufs = unsafe { std::slice::from_raw_parts(abl.mBuffers.as_ptr(), n) };
    bufs.iter().map(|b| b.mNumberChannels as u16).collect()
}

pub fn total_channels(dev: u32, scope: u32) -> u16 {
    stream_channels(dev, scope).iter().sum()
}

pub fn dict(pairs: &[(&str, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = pairs.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let vals: Vec<&AnyObject> = pairs.iter().map(|(_, v)| *v).collect();
    NSDictionary::from_slices(&key_refs, &vals)
}

/// [`dict`] keyed by CoreAudio's own `kAudio*Key` constants.
pub fn cdict(pairs: &[(&CStr, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let pairs: Vec<(&str, &AnyObject)> = pairs
        .iter()
        .map(|(k, v)| (k.to_str().expect("CoreAudio keys are ASCII"), *v))
        .collect();
    dict(&pairs)
}

pub fn bool_obj(b: bool) -> Retained<NSNumber> {
    NSNumber::new_bool(b)
}

pub fn string_array(items: &[String]) -> Retained<NSArray<NSString>> {
    let v: Vec<Retained<NSString>> = items.iter().map(|s| NSString::from_str(s)).collect();
    NSArray::from_retained_slice(&v)
}
