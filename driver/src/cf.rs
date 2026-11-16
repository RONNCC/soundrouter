//! CoreFoundation helpers: strings and the device-list property list.
use crate::config;
use coreaudio_sys::*;
use sr_core::driver_proto::{DriverDevice, MAX_DEVICES};
use std::ffi::c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {}

/// New CFString (+1). Interior NULs are fine: CF takes the byte length.
pub unsafe fn cfstr(s: &str) -> CFStringRef {
    unsafe {
        CFStringCreateWithBytes(
            kCFAllocatorDefault,
            s.as_ptr(),
            s.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        )
    }
}

pub unsafe fn rstr(v: CFTypeRef) -> Option<String> {
    unsafe {
        if v.is_null() || CFGetTypeID(v) != CFStringGetTypeID() {
            return None;
        }
        let s = v as CFStringRef;
        let max =
            CFStringGetMaximumSizeForEncoding(CFStringGetLength(s), kCFStringEncodingUTF8) + 1;
        if !(1..=65536).contains(&max) {
            return None; // absurd length: reject rather than allocate
        }
        let mut buf = vec![0u8; max as usize];
        if CFStringGetCString(s, buf.as_mut_ptr().cast(), max, kCFStringEncodingUTF8) == 0 {
            return None;
        }
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        buf.truncate(n);
        String::from_utf8(buf).ok()
    }
}

unsafe fn dict_get(d: CFDictionaryRef, key: &str) -> CFTypeRef {
    unsafe {
        let k = cfstr(key);
        let v = CFDictionaryGetValue(d, k as *const c_void);
        CFRelease(k as CFTypeRef);
        v
    }
}

/// Parse and validate a device-list property list (an untrusted CFArray of CFDictionary).
pub unsafe fn parse_devices(plist: CFTypeRef) -> Result<Vec<DriverDevice>, String> {
    unsafe {
        if plist.is_null() || CFGetTypeID(plist) != CFArrayGetTypeID() {
            return Err("not an array".into());
        }
        let arr = plist as CFArrayRef;
        let n = CFArrayGetCount(arr) as usize;
        if n > MAX_DEVICES {
            return Err(format!("{n} devices, max {MAX_DEVICES}"));
        }
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let d = CFArrayGetValueAtIndex(arr, i as CFIndex) as CFTypeRef;
            if d.is_null() || CFGetTypeID(d) != CFDictionaryGetTypeID() {
                return Err(format!("device {i}: not a dictionary"));
            }
            let d = d as CFDictionaryRef;
            let (Some(uid), Some(name)) = (rstr(dict_get(d, "uid")), rstr(dict_get(d, "name")))
            else {
                return Err(format!("device {i}: uid/name must be strings"));
            };
            let num = dict_get(d, "channels");
            let mut ch: i32 = 0;
            if num.is_null()
                || CFGetTypeID(num) != CFNumberGetTypeID()
                || CFNumberGetValue(
                    num as CFNumberRef,
                    kCFNumberSInt32Type as CFNumberType,
                    (&raw mut ch).cast(),
                ) == 0
            {
                return Err(format!("device {i}: channels must be an integer"));
            }
            let channels =
                u16::try_from(ch).map_err(|_| format!("device {i}: channels out of range"))?;
            out.push(DriverDevice {
                uid,
                name,
                channels,
            });
        }
        config::validate(out)
    }
}

/// Build the property list for a device list (+1 retained).
pub unsafe fn devices_to_plist(list: &[DriverDevice]) -> CFArrayRef {
    unsafe {
        let arr = CFArrayCreateMutable(
            kCFAllocatorDefault,
            list.len() as CFIndex,
            &kCFTypeArrayCallBacks,
        );
        let keys = [cfstr("uid"), cfstr("name"), cfstr("channels")];
        for d in list {
            let ch = d.channels as i32;
            let num = CFNumberCreate(
                kCFAllocatorDefault,
                kCFNumberSInt32Type as CFNumberType,
                (&raw const ch).cast(),
            );
            let vals = [
                cfstr(&d.uid) as *const c_void,
                cfstr(&d.name) as *const c_void,
                num as *const c_void,
            ];
            let dict = CFDictionaryCreate(
                kCFAllocatorDefault,
                keys.as_ptr() as *mut *const c_void,
                vals.as_ptr() as *mut *const c_void,
                3,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            );
            for v in vals {
                CFRelease(v as CFTypeRef);
            }
            CFArrayAppendValue(arr, dict as *const c_void);
            CFRelease(dict as CFTypeRef);
        }
        for k in keys {
            CFRelease(k as CFTypeRef);
        }
        arr as CFArrayRef
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null;

    fn d(uid: &str, name: &str, channels: u16) -> DriverDevice {
        DriverDevice {
            uid: uid.into(),
            name: name.into(),
            channels,
        }
    }

    #[test]
    fn plist_round_trip() {
        let list = vec![
            d("soundrouter:v1", "Loopback Audio", 2),
            d("x", "Ünïcode ✓", 64),
        ];
        unsafe {
            let p = devices_to_plist(&list);
            assert_eq!(parse_devices(p as CFTypeRef).unwrap(), list);
            CFRelease(p as CFTypeRef);
            let empty = devices_to_plist(&[]);
            assert_eq!(parse_devices(empty as CFTypeRef).unwrap(), vec![]);
            CFRelease(empty as CFTypeRef);
        }
    }

    #[test]
    fn parse_rejects_garbage() {
        unsafe {
            let s = cfstr("nope");
            assert!(parse_devices(s as CFTypeRef).is_err());
            assert!(parse_devices(null()).is_err());
            CFRelease(s as CFTypeRef);
            // array of strings instead of dictionaries
            let s = cfstr("x");
            let items = [s as *const c_void];
            let arr = CFArrayCreate(
                kCFAllocatorDefault,
                items.as_ptr() as *mut *const c_void,
                1,
                &kCFTypeArrayCallBacks,
            );
            assert!(parse_devices(arr as CFTypeRef).is_err());
            CFRelease(arr as CFTypeRef);
            CFRelease(s as CFTypeRef);
            // duplicate uid is rejected by validate
            let dup = devices_to_plist(&[d("a", "n", 2), d("a", "m", 2)]);
            assert!(
                parse_devices(dup as CFTypeRef)
                    .unwrap_err()
                    .contains("duplicate")
            );
            CFRelease(dup as CFTypeRef);
        }
    }
}
