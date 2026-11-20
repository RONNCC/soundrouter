//! What can go wrong around a running engine and is worth telling the user: tapped apps that deliver
//! nothing (denied System Audio Recording), devices held in hog mode by another process, Bluetooth
//! devices that fell back to the headset profile. The policies are pure (`SilenceWatch`, `hogger`,
//! `looks_hfp`); the functions at the bottom read the HAL.
use std::time::Duration;

use objc2_core_audio::*;
use sr_core::backend::{HogInfo, TccStatus};

use crate::hal;

/// How long a tapped app must be playing with only zeros arriving before the tap is reported silent.
pub const SILENT_AFTER: Duration = Duration::from_secs(3);

/// Zero-fill detector for one tapped source. A denied System Audio Recording permission does not make
/// the tap fail: it delivers all-zero buffers with no error. So "the source is playing but its peak
/// stays 0.0" for [`SILENT_AFTER`] is the signal. It must not flag a source that plays nothing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SilenceWatch {
    /// When the current quiet stretch (active, no signal) began.
    quiet_since: Option<Duration>,
}

impl SilenceWatch {
    /// One observation at monotonic time `now`: `active` = the tapped process is producing output and the
    /// engine's tap path runs; `signal` = a non-zero peak arrived since the previous observation.
    /// Returns whether the tap counts as silent.
    pub fn observe(&mut self, now: Duration, active: bool, signal: bool) -> bool {
        if !active || signal {
            self.quiet_since = None;
            return false;
        }
        let since = *self.quiet_since.get_or_insert(now);
        now.saturating_sub(since) >= SILENT_AFTER
    }
}

/// Whether to look for a zero-filled tap at all. A tapped app that plays digital silence is
/// legitimate once System Audio Recording is granted, so only a permission that is not granted counts.
/// `Unknown` is watched too: an ad-hoc signed build can read as `Unknown` while the permission is in fact
/// denied, and the silence is then the only sign of it.
pub fn watch_taps(active: bool, tcc: TccStatus) -> bool {
    active && tcc != TccStatus::Granted
}

/// `kAudioDevicePropertyHogMode` value: the pid holding the device, `-1` when nobody does.
pub fn hogger(raw: i32) -> Option<i32> {
    (raw != -1).then_some(raw)
}

/// `kAudioDeviceTransportTypeBluetooth` and `...BluetoothLE`.
pub fn is_bluetooth(transport: u32) -> bool {
    transport == u32::from_be_bytes(*b"blue") || transport == u32::from_be_bytes(*b"blea")
}

/// Highest sample rate of the Bluetooth headset profile (HFP is 16 kHz mono, wideband speech 16 kHz,
/// some codecs 24 kHz); A2DP is 44.1 kHz and up.
const HFP_MAX_RATE: f64 = 24_000.0;

/// A Bluetooth device that looks switched to the headset profile: opening its microphone makes macOS drop
/// it from A2DP to HFP, which shows as a low rate or fewer output channels than the aggregate was built
/// with. Only while an input stream of the device is open: a speaker-only route never triggers it.
pub fn looks_hfp(
    bluetooth: bool,
    rate: f64,
    out_ch: u32,
    built_out_ch: u32,
    input_open: bool,
) -> bool {
    bluetooth && input_open && ((rate > 0.0 && rate <= HFP_MAX_RATE) || out_ch < built_out_ch)
}

/// Who holds `dev` in hog mode, if another process than this one does.
pub fn hog_info(dev: u32) -> Option<HogInfo> {
    let pid = hog_pid(dev).filter(|&p| p != std::process::id() as i32)?;
    Some(HogInfo {
        device_name: hal::device_name(dev),
        pid: Some(pid).filter(|&p| p > 0),
        process_name: process_name(pid),
    })
}

/// Every hardware device's trouble regardless of any config: hogged ones, and Bluetooth ones whose rate
/// is a headset rate (the channel-count check needs an aggregate to compare with, so it is left out).
/// For `sr-cli doctor`, which has no engine of its own.
pub fn scan_all() -> (Vec<HogInfo>, Vec<String>) {
    let (mut hogged, mut hfp) = (Vec::new(), Vec::new());
    for dev in hal::all_devices() {
        if crate::engine::is_own_uid(&hal::device_uid(dev)) {
            continue;
        }
        hogged.extend(hog_info(dev));
        let out = u32::from(hal::total_channels(dev, kAudioObjectPropertyScopeOutput));
        let inp = hal::total_channels(dev, kAudioObjectPropertyScopeInput) > 0;
        if looks_hfp(
            is_bluetooth(transport_type(dev)),
            hal::nominal_rate(dev),
            out,
            out,
            inp,
        ) {
            hfp.push(hal::device_name(dev));
        }
    }
    (hogged, hfp)
}

/// Process id holding `dev` in hog mode.
pub fn hog_pid(dev: u32) -> Option<i32> {
    hogger(hal::get_global::<i32>(dev, kAudioDevicePropertyHogMode).ok()?)
}

pub fn transport_type(dev: u32) -> u32 {
    hal::get_global(dev, kAudioDevicePropertyTransportType).unwrap_or(0)
}

/// True while the process object `proc_obj` runs audio output.
pub fn process_running_output(proc_obj: u32) -> bool {
    hal::get_global::<u32>(proc_obj, kAudioProcessPropertyIsRunningOutput).is_ok_and(|v| v != 0)
}

/// Executable name of `pid`.
pub fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is 256 bytes and proc_name writes at most that many.
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn silence_needs_three_seconds_of_playing_without_signal() {
        let mut w = SilenceWatch::default();
        assert!(!w.observe(secs(0), true, false), "just started");
        assert!(!w.observe(secs(2), true, false));
        assert!(w.observe(secs(3), true, false), "3 s exactly");
        assert!(w.observe(secs(10), true, false), "stays flagged");
    }

    #[test]
    fn signal_or_a_silent_source_resets_the_clock() {
        let mut w = SilenceWatch::default();
        w.observe(secs(0), true, false);
        assert!(!w.observe(secs(2), true, true), "signal arrived");
        assert!(!w.observe(secs(4), true, false), "clock restarted at 4");
        assert!(w.observe(secs(7), true, false));
        assert!(
            !w.observe(secs(8), true, true),
            "signal clears the flag at once"
        );
        // a source that stops playing is not a denied tap
        w.observe(secs(9), true, false);
        assert!(!w.observe(secs(20), false, false));
        assert!(
            !w.observe(secs(21), true, false),
            "playing again: counts from here"
        );
        assert!(w.observe(secs(24), true, false));
    }

    #[test]
    fn only_a_permission_that_is_not_granted_is_watched_for_silence() {
        for (tcc, watched) in [
            (TccStatus::Granted, false),
            (TccStatus::Denied, true),
            (TccStatus::Unknown, true),
        ] {
            assert_eq!(watch_taps(true, tcc), watched, "{tcc:?}");
            assert!(!watch_taps(false, tcc), "an idle tap is never watched");
        }
        // Granted: a playing app with a zero peak never reaches the 3 s mark, because the watch is fed
        // `active = false` and resets.
        let mut w = SilenceWatch::default();
        for t in 0..20 {
            assert!(!w.observe(secs(t), watch_taps(true, TccStatus::Granted), false));
        }
        for t in 20..24 {
            w.observe(secs(t), watch_taps(true, TccStatus::Denied), false);
        }
        assert!(w.observe(secs(24), watch_taps(true, TccStatus::Denied), false));
        assert!(
            !w.observe(secs(25), watch_taps(true, TccStatus::Granted), false),
            "granted in the meantime clears the flag"
        );
    }

    #[test]
    fn a_source_that_never_plays_is_never_flagged() {
        let mut w = SilenceWatch::default();
        for t in 0..60 {
            assert!(!w.observe(secs(t), false, false));
        }
    }

    #[test]
    fn hog_mode_minus_one_is_free() {
        assert_eq!(hogger(-1), None);
        assert_eq!(hogger(4321), Some(4321));
    }

    #[test]
    fn bluetooth_transport_codes() {
        assert!(is_bluetooth(u32::from_be_bytes(*b"blue")));
        assert!(is_bluetooth(u32::from_be_bytes(*b"blea")));
        assert!(!is_bluetooth(u32::from_be_bytes(*b"bltn")));
        assert!(!is_bluetooth(u32::from_be_bytes(*b"virt")));
    }

    #[test]
    fn hfp_is_a_bluetooth_device_with_a_low_rate_or_lost_channels_and_an_open_input() {
        assert!(looks_hfp(true, 16_000.0, 1, 2, true));
        assert!(looks_hfp(true, 24_000.0, 2, 2, true), "rate alone");
        assert!(
            looks_hfp(true, 48_000.0, 1, 2, true),
            "channel count dropped alone"
        );
        assert!(!looks_hfp(true, 48_000.0, 2, 2, true), "healthy A2DP");
        assert!(
            !looks_hfp(true, 16_000.0, 1, 2, false),
            "no input stream open"
        );
        assert!(!looks_hfp(false, 16_000.0, 1, 2, true), "not Bluetooth");
        assert!(
            !looks_hfp(true, 0.0, 2, 2, true),
            "unknown rate is not a low rate"
        );
    }
}
