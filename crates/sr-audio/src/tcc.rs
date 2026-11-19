//! TCC permission checks (private framework, dlopen'd) and the System Audio Recording request.
use std::ffi::{CStr, c_char, c_int, c_void};

use std::time::Duration;

use block2::{Block, RcBlock};
use objc2::msg_send;
use objc2::runtime::{AnyClass, Bool};
use objc2_foundation::NSString;
use sr_core::backend::TccStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    Granted,
    Denied,
    /// Not decided yet (raw 2) or unreadable; carries the raw value, -1 = private API unavailable.
    Unknown(i32),
}

impl From<CaptureStatus> for TccStatus {
    fn from(s: CaptureStatus) -> Self {
        match s {
            CaptureStatus::Granted => TccStatus::Granted,
            CaptureStatus::Denied => TccStatus::Denied,
            CaptureStatus::Unknown(_) => TccStatus::Unknown,
        }
    }
}

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const AUDIO_CAPTURE: &str = "kTCCServiceAudioCapture";

/// System Audio Recording (process taps), without prompting.
pub fn audio_capture_status() -> CaptureStatus {
    preflight(AUDIO_CAPTURE)
}

/// Ask macOS for System Audio Recording the way AudioCap does (`TCCAccessRequest`): the system
/// prompt when the question is still open, an immediate answer once it was decided. Blocks until
/// the user answers, at most `wait`. Returns the status afterwards.
pub fn request_audio_capture(wait: Duration) -> CaptureStatus {
    let granted = tcc_symbol(c"TCCAccessRequest").and_then(|f| {
        // SAFETY: `void TCCAccessRequest(CFStringRef service, CFDictionaryRef options, void (^)(Boolean))`;
        // NSString is toll-free bridged to CFString, and TCC copies the block it calls later.
        let f: unsafe extern "C" fn(*const NSString, *const c_void, *const Block<dyn Fn(Bool)>) =
            unsafe { std::mem::transmute(f) };
        let (tx, rx) = std::sync::mpsc::channel();
        let done = RcBlock::new(move |granted: Bool| {
            let _ = tx.send(granted.as_bool());
        });
        let svc = NSString::from_str(AUDIO_CAPTURE);
        unsafe { f(&*svc, std::ptr::null(), &*done) };
        rx.recv_timeout(wait).ok()
    });
    after_request(granted, audio_capture_status())
}

/// The status after a request: a "yes" from the completion is final; anything else (no, or no
/// answer in time) is whatever TCC now says, so a prompt that never showed stays "not asked".
fn after_request(granted: Option<bool>, now: CaptureStatus) -> CaptureStatus {
    match granted {
        Some(true) => CaptureStatus::Granted,
        _ => now,
    }
}

/// Microphone permission, without prompting. AVFoundation's public answer wins; the private TCC
/// preflight is the fallback when AVFoundation is unavailable or says "not asked yet".
pub fn microphone_status() -> CaptureStatus {
    mic_from_av(av_authorization(), || preflight("kTCCServiceMicrophone"))
}

fn mic_from_av(av: Option<isize>, preflight: impl FnOnce() -> CaptureStatus) -> CaptureStatus {
    match av {
        Some(3) => CaptureStatus::Granted,
        Some(1 | 2) => CaptureStatus::Denied, // restricted, denied
        Some(0) => match preflight() {
            CaptureStatus::Unknown(-1) => CaptureStatus::Unknown(0),
            other => other,
        },
        _ => preflight(),
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

/// A function of the private TCC framework.
fn tcc_symbol(name: &CStr) -> Option<*mut c_void> {
    const RTLD_NOW: c_int = 2;
    // SAFETY: loading a system framework by path and looking a symbol up in it.
    unsafe {
        let h = dlopen(
            c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC".as_ptr(),
            RTLD_NOW,
        );
        let f = if h.is_null() {
            h
        } else {
            dlsym(h, name.as_ptr())
        };
        (!f.is_null()).then_some(f)
    }
}

/// `TCCAccessPreflight`: 0 allowed, 1 denied, 2 not decided yet (checked on macOS 14.7.1 with a
/// fresh binary launched by launchd, i.e. its own TCC subject).
fn preflight(service: &str) -> CaptureStatus {
    let Some(f) = tcc_symbol(c"TCCAccessPreflight") else {
        return CaptureStatus::Unknown(-1);
    };
    // SAFETY: `int TCCAccessPreflight(CFStringRef service, CFDictionaryRef options)`.
    let f: unsafe extern "C" fn(*const NSString, *const c_void) -> c_int =
        unsafe { std::mem::transmute(f) };
    let svc = NSString::from_str(service);
    from_preflight(unsafe { f(&*svc, std::ptr::null()) })
}

fn from_preflight(v: c_int) -> CaptureStatus {
    match v {
        0 => CaptureStatus::Granted,
        1 => CaptureStatus::Denied,
        v => CaptureStatus::Unknown(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_tcc_functions_exist() {
        assert!(tcc_symbol(c"TCCAccessPreflight").is_some());
        assert!(tcc_symbol(c"TCCAccessRequest").is_some());
    }

    #[test]
    fn only_a_yes_from_the_request_is_final() {
        let not_asked = CaptureStatus::Unknown(2);
        assert_eq!(after_request(Some(true), not_asked), CaptureStatus::Granted);
        assert_eq!(
            after_request(Some(false), CaptureStatus::Denied),
            CaptureStatus::Denied
        );
        // no prompt could be shown, or the user has not answered yet
        assert_eq!(after_request(Some(false), not_asked), not_asked);
        assert_eq!(after_request(None, not_asked), not_asked);
        assert_eq!(
            TccStatus::from(CaptureStatus::Unknown(-1)),
            TccStatus::Unknown
        );
    }

    #[test]
    fn capture_status_maps_to_tcc_status() {
        assert_eq!(TccStatus::from(CaptureStatus::Granted), TccStatus::Granted);
        assert_eq!(TccStatus::from(CaptureStatus::Denied), TccStatus::Denied);
        assert_eq!(
            TccStatus::from(CaptureStatus::Unknown(2)),
            TccStatus::Unknown
        );
    }

    #[test]
    fn preflight_codes() {
        assert_eq!(from_preflight(0), CaptureStatus::Granted);
        assert_eq!(from_preflight(1), CaptureStatus::Denied);
        assert_eq!(from_preflight(2), CaptureStatus::Unknown(2));
    }

    #[test]
    fn avfoundation_wins_and_tcc_is_only_the_fallback() {
        let tcc = |s| move || s;
        let unreachable = || panic!("AVFoundation already answered");
        assert_eq!(mic_from_av(Some(3), unreachable), CaptureStatus::Granted);
        assert_eq!(mic_from_av(Some(2), unreachable), CaptureStatus::Denied);
        assert_eq!(mic_from_av(Some(1), unreachable), CaptureStatus::Denied); // restricted
        // not determined: TCC may know better; with no TCC symbol it is "not asked", not an error code
        assert_eq!(
            mic_from_av(Some(0), tcc(CaptureStatus::Granted)),
            CaptureStatus::Granted
        );
        assert_eq!(
            mic_from_av(Some(0), tcc(CaptureStatus::Denied)),
            CaptureStatus::Denied
        );
        assert_eq!(
            mic_from_av(Some(0), tcc(CaptureStatus::Unknown(-1))),
            CaptureStatus::Unknown(0)
        );
        // no AVFoundation at all: TCC verbatim
        assert_eq!(
            mic_from_av(None, tcc(CaptureStatus::Denied)),
            CaptureStatus::Denied
        );
        assert_eq!(
            mic_from_av(None, tcc(CaptureStatus::Unknown(-1))),
            CaptureStatus::Unknown(-1)
        );
    }
}
