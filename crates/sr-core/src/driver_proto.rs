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
