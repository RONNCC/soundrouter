//! TCC audio-capture permission preflight (private framework, dlopen'd; no prompt is triggered).
use std::ffi::{c_char, c_int, c_void};

use std::time::Duration;

use block2::RcBlock;
use objc2::msg_send;
use objc2::runtime::{AnyClass, Bool};
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

/// Microphone permission, without prompting. AVFoundation's public answer wins; the private TCC
/// preflight is the fallback when AVFoundation is unavailable or says "not asked yet".
pub fn microphone_status() -> CaptureStatus {
    match av_authorization() {
        Some(3) => CaptureStatus::Granted,
        Some(1 | 2) => CaptureStatus::Denied, // restricted, denied
        Some(0) => match preflight("kTCCServiceMicrophone") {
            CaptureStatus::Unknown(-1) => CaptureStatus::Unknown(0),
            other => other,
        },
        _ => preflight("kTCCServiceMicrophone"),
    }
}

/// `AVCaptureDevice.authorizationStatus(for: .audio)`: 0 not determined, 1 restricted, 2 denied, 3 authorized.
fn av_authorization() -> Option<isize> {
    let cls = av_capture_device()?;
    // SAFETY: class method taking an NSString and returning an NSInteger enum.
    Some(unsafe {
        msg_send![cls, authorizationStatusForMediaType: &*NSString::from_str(AUDIO_MEDIA)]
    })
}

/// AVMediaTypeAudio.
const AUDIO_MEDIA: &str = "soun";

fn av_capture_device() -> Option<&'static AnyClass> {
    const RTLD_NOW: c_int = 2;
    // SAFETY: loading a system framework by path.
    unsafe {
        dlopen(
            c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation".as_ptr(),
            RTLD_NOW,
        );
    }
    AnyClass::get(c"AVCaptureDevice")
}

/// Ask for microphone access when it has not been decided yet, which shows the system prompt
/// (a bare IOProc start on a virtual device does not always raise it). Blocks until the user
/// answers, at most `wait`. Returns the status afterwards.
pub fn request_microphone(wait: Duration) -> CaptureStatus {
    if av_authorization() == Some(0)
        && let Some(cls) = av_capture_device()
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let done = RcBlock::new(move |_granted: Bool| {
            let _ = tx.send(());
        });
        // SAFETY: class method taking an NSString and a `void (^)(BOOL)` completion block.
        let _: () = unsafe {
            msg_send![cls, requestAccessForMediaType: &*NSString::from_str(AUDIO_MEDIA), completionHandler: &*done]
        };
        let _ = rx.recv_timeout(wait);
    }
    microphone_status()
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
