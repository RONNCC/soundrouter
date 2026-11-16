//! TCC audio-capture permission preflight (private framework, dlopen'd; no prompt is triggered).
use std::ffi::{c_char, c_int, c_void};

use objc2_foundation::NSString;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    Granted,
    Denied,
    /// Not decided yet, or the private API is unavailable (carries the raw value, -1 = unavailable).
    Unknown(i32),
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

pub fn audio_capture_status() -> CaptureStatus {
    preflight("kTCCServiceAudioCapture")
}

/// Microphone permission, without prompting.
pub fn microphone_status() -> CaptureStatus {
    preflight("kTCCServiceMicrophone")
}

fn preflight(service: &str) -> CaptureStatus {
    const RTLD_NOW: c_int = 2;
    unsafe {
        let h = dlopen(
            c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC".as_ptr(),
            RTLD_NOW,
        );
        if h.is_null() {
            return CaptureStatus::Unknown(-1);
        }
        let f = dlsym(h, c"TCCAccessPreflight".as_ptr());
        if f.is_null() {
            return CaptureStatus::Unknown(-1);
        }
        let f: unsafe extern "C" fn(*const NSString, *const c_void) -> c_int =
            std::mem::transmute(f);
        let svc = NSString::from_str(service);
        match f(&*svc, std::ptr::null()) {
            0 => CaptureStatus::Granted,
            1 => CaptureStatus::Denied,
            v => CaptureStatus::Unknown(v),
        }
    }
}
