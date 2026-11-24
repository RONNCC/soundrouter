use crate::backend::ReadAheadState;

pub const PLUGIN_BUNDLE_ID: &str = "io.github.ronncc.soundrouter.driver";
pub const APP_BUNDLE_ID: &str = "com.ronncc.soundrouter";
pub const DEVICE_UID_PREFIX: &str = "soundrouter:";
pub const MANUFACTURER: &str = "SoundRouter";
/// Protocol version, read-only, data = CFString "1".
pub const PROP_VERSION: u32 = u32::from_be_bytes(*b"srvr");
/// Late/short counter, read-only, on each device object (V and C), type 'plst' (CFNumber, 64-bit): how many
/// WriteMix calls on that device arrived after their output time or exceeded the ring, plus ReadInput calls whose
/// read-ahead reached past the written frames (silent tail). Resets when the slot is reused.
pub const PROP_OVERLOADS: u32 = u32::from_be_bytes(*b"srov");
/// Loop read-ahead switch, on the plug-in object (global scope, element main). Type 'plst' (CFNumber), settable:
/// nonzero = readers of the loopback rings read ahead of the writer's frames (saves up to ~1 buffer per ring
/// traversal, calibrated per stream), 0 = off. Default off (`READ_AHEAD_DEFAULT` in the driver); not persisted,
/// so the setter writes it after each driver start. Changing it restarts every stream's calibration.
pub const PROP_READ_AHEAD: u32 = u32::from_be_bytes(*b"srla");
/// Whether the driver's read-ahead must be written again: wanted on or off (`want`) and the driver reports
/// another value. The driver does not persist the switch (a restart puts it back at its default) and every
/// write restarts the calibration of all streams, so write only when this says so. `None` when it matches
/// or the driver has no such property.
pub fn read_ahead_repush(want: bool, live: ReadAheadState) -> Option<bool> {
    match live {
        ReadAheadState::Unsupported => None,
        live => (want != live.is_on()).then_some(want),
    }
}

/// The `srov` sum of a virtual device's visible half and its hidden companion. `None` unless both
/// halves read: a half that fails once would otherwise look like a counter dip and, on the next good
/// look, like growth that trips the read-ahead guard. A skipped look is a new baseline, not growth.
pub fn sum_overloads(visible: Option<i64>, companion: Option<i64>) -> Option<u64> {
    Some((visible?.max(0) as u64).saturating_add(companion?.max(0) as u64))
}

/// Counters per device id, e.g. `EngineStatus::xruns`.
pub type Counters = [(String, u64)];
type OwnedCounters = Vec<(String, u64)>;

/// Whether any counter in `now` is above the same id's in `before` (a new or lower counter is a new
/// baseline, not growth).
pub fn counters_grew(before: &Counters, now: &Counters) -> bool {
    now.iter()
        .any(|(id, n)| before.iter().any(|(b, was)| b == id && n > was))
}

/// Safety net for the experimental read-ahead: while it is on, any growth of a device's driver
/// overload counter (`srov`: late writes and read-ahead shortfalls) or xruns between two looks means
/// trouble. Pure logic: the caller supplies the counters, one look per status poll.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadAheadGuard {
    last: Option<(OwnedCounters, OwnedCounters)>,
}

impl ReadAheadGuard {
    /// True when `overloads` or `xruns` grew since the previous look. The first look only records.
    pub fn tripped(&mut self, overloads: &Counters, xruns: &Counters) -> bool {
        let grew = self
            .last
            .as_ref()
            .is_some_and(|(o, x)| counters_grew(o, overloads) || counters_grew(x, xruns));
        self.last = Some((overloads.to_vec(), xruns.to_vec()));
        grew
    }
}

/// Device list, on the plug-in object (global scope, element main). Type 'plst' (CFPropertyList), settable.
/// Value = CFArray of CFDictionary {uid: CFString starting with `DEVICE_UID_PREFIX`, name: CFString, channels: CFNumber 1..=64}, at most `MAX_DEVICES`.
/// Set replaces the whole list. The driver persists it via host storage under `STORAGE_KEY`.
pub const PROP_DEVICES: u32 = u32::from_be_bytes(*b"srcf");
pub const STORAGE_KEY: &str = "devices"; // host WriteToStorage key, same CFArray
pub const MAX_DEVICES: usize = 16;
/// Hidden engine companion UID = device UID + this suffix. See `companion_uid`.
pub const COMPANION_SUFFIX: &str = ".engine";
pub const SAMPLE_RATES: [f64; 3] = [44_100.0, 48_000.0, 96_000.0];
pub const DEFAULT_RATE: f64 = 48_000.0;
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DriverDevice {
    pub uid: String,
    pub name: String,
    pub channels: u16,
}
/// UID of the hidden engine companion paired with visible device `uid`.
pub fn companion_uid(uid: &str) -> String {
    format!("{uid}{COMPANION_SUFFIX}")
}

/// Bottom of the volume controls the driver publishes on V (dB); at this value the gain is exactly 0.
pub const VOLUME_MIN_DB: f32 = -64.0;
/// Linear gain of a V volume control (`scalar` 0..=1, linear in dB from `VOLUME_MIN_DB` to 0 dB) and its mute:
/// the driver applies it to what apps play into V, the engine to the app taps it routes from V.
/// Out-of-range scalars clamp; NaN counts as unity (a control that cannot be read must not silence the route).
pub fn volume_gain(scalar: f32, muted: bool) -> f32 {
    let s = if scalar.is_nan() {
        1.0
    } else {
        scalar.clamp(0.0, 1.0)
    };
    if muted || s == 0.0 {
        0.0
    } else {
        10f64.powf(f64::from(VOLUME_MIN_DB) * f64::from(1.0 - s) / 20.0) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn c(v: &[(&str, u64)]) -> Vec<(String, u64)> {
        v.iter().map(|&(i, n)| (i.to_string(), n)).collect()
    }

    #[test]
    fn counters_grow_only_where_an_id_is_in_both_and_higher() {
        assert!(counters_grew(&c(&[("a", 1)]), &c(&[("a", 2)])));
        assert!(!counters_grew(&c(&[("a", 2)]), &c(&[("a", 2)])));
        assert!(
            !counters_grew(&c(&[("a", 5)]), &c(&[("a", 0)])),
            "rebuilt: lower is a new baseline"
        );
        assert!(
            !counters_grew(&c(&[("a", 1)]), &c(&[("b", 9)])),
            "a new id has no history"
        );
        assert!(counters_grew(
            &c(&[("a", 1), ("b", 1)]),
            &c(&[("a", 1), ("b", 2)])
        ));
        assert!(!counters_grew(&[], &c(&[("a", 1)])));
    }

    #[test]
    fn overloads_need_both_halves_so_a_failed_read_is_not_a_dip_then_growth() {
        assert_eq!(sum_overloads(Some(3), Some(4)), Some(7));
        assert_eq!(sum_overloads(Some(-1), Some(4)), Some(4));
        assert_eq!(sum_overloads(Some(3), None), None);
        assert_eq!(sum_overloads(None, Some(4)), None);
        assert_eq!(sum_overloads(None, None), None);
        // The skipped look leaves the id out; the next one is a baseline and trips nothing.
        let mut g = ReadAheadGuard::default();
        assert!(!g.tripped(&c(&[("v1", 7)]), &[]));
        assert!(!g.tripped(&[], &[]));
        assert!(!g.tripped(&c(&[("v1", 7)]), &[]));
    }

    #[test]
    fn guard_records_first_then_trips_on_any_growth() {
        let mut g = ReadAheadGuard::default();
        assert!(
            !g.tripped(&c(&[("v1", 7)]), &c(&[("v1", 2)])),
            "first look is the baseline"
        );
        assert!(!g.tripped(&c(&[("v1", 7)]), &c(&[("v1", 2)])));
        assert!(g.tripped(&c(&[("v1", 8)]), &c(&[("v1", 2)])));
        // the grown value is the new baseline
        assert!(!g.tripped(&c(&[("v1", 8)]), &c(&[("v1", 2)])));
        assert!(
            g.tripped(&c(&[("v1", 8)]), &c(&[("v1", 3)])),
            "xruns count too"
        );
    }

    #[test]
    fn guard_ignores_a_rebuilt_device_that_restarts_from_zero() {
        let mut g = ReadAheadGuard::default();
        g.tripped(&c(&[("v1", 7)]), &c(&[("v1", 4)]));
        assert!(!g.tripped(&c(&[("v1", 0)]), &c(&[("v1", 0)])));
        assert!(g.tripped(&c(&[("v1", 1)]), &c(&[("v1", 0)])));
    }

    #[test]
    fn repush_only_when_the_driver_differs_and_can_do_it() {
        use ReadAheadState::*;
        assert_eq!(read_ahead_repush(true, Off), Some(true));
        assert_eq!(read_ahead_repush(true, On), None);
        assert_eq!(read_ahead_repush(false, On), Some(false));
        assert_eq!(read_ahead_repush(false, Off), None);
        assert_eq!(read_ahead_repush(true, Unsupported), None);
        assert_eq!(read_ahead_repush(false, Unsupported), None);
    }

    #[test]
    fn volume_gain_is_linear_in_db_with_exact_silence_at_the_bottom() {
        assert_eq!(volume_gain(1.0, false), 1.0);
        assert_eq!(volume_gain(0.0, false), 0.0);
        assert_eq!(volume_gain(1.0, true), 0.0);
        // scalar 0.5 = -32 dB, 0.75 = -16 dB
        assert!((20.0 * volume_gain(0.5, false).log10() + 32.0).abs() < 1e-4);
        assert!((20.0 * volume_gain(0.75, false).log10() + 16.0).abs() < 1e-4);
        assert_eq!(volume_gain(7.0, false), 1.0);
        assert_eq!(volume_gain(-1.0, false), 0.0);
        assert_eq!(volume_gain(f32::NAN, false), 1.0);
    }
}
