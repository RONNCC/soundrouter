use serde::{Deserialize, Serialize};

use crate::config::Config;
#[derive(Clone, Debug)]
pub struct HwDevice {
    pub uid: String,
    pub name: String,
    pub in_ch: u16,
    pub out_ch: u16,
}
#[derive(Clone, Debug)]
pub struct AppInfo {
    pub bundle_id: String,
    pub name: String,
    pub running_output: bool,
    /// A regular GUI app (shows in the Dock, activation policy `.regular`); false for daemons,
    /// agents, helpers and command-line players, which the source picker hides by default.
    pub regular: bool,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeterReading {
    pub sources: Vec<(String, Vec<f32>)>,
    pub channels: Vec<f32>,
    /// One entry per mixer node of the device, in config order: its id and the post-fader peak per channel.
    pub nodes: Vec<(String, Vec<f32>)>,
} // linear peak since last read
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TccStatus {
    Granted,
    Denied,
    #[default]
    Unknown,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceState {
    #[default]
    Idle,
    WaitingForPermission,
    Starting,
    Running,
    Failed(String),
}
/// What a running virtual device adds between an input frame arriving and its output frame playing:
/// the distance the engine's IOProc reports (output sample time minus input sample time). It includes
/// the aggregate's IO buffers and its safety offsets, so it is the delay as it really is, not a guess
/// from the buffer setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delay {
    pub frames: u32,
    pub rate_hz: u32,
}

impl Delay {
    pub fn ms(self) -> f64 {
        frames_to_ms(self.frames, self.rate_hz)
    }
}

/// `frames` at `rate_hz` in milliseconds; 0 at an unknown (0 Hz) rate.
pub fn frames_to_ms(frames: u32, rate_hz: u32) -> f64 {
    if rate_hz == 0 {
        return 0.0;
    }
    f64::from(frames) * 1000.0 / f64::from(rate_hz)
}

/// A device the config uses that another process holds in hog (exclusive) mode: the HAL hides it from
/// everyone else, so routes through it carry nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HogInfo {
    pub device_name: String,
    /// The process holding it; `None` when the HAL reported a hog but the pid is unusable.
    pub pid: Option<i32>,
    pub process_name: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineStatus {
    pub per_device: Vec<(String, DeviceState)>,
    pub warnings: Vec<String>,
    pub background_error: Option<String>,
    /// System Audio Recording permission (app taps).
    pub tcc: TccStatus,
    /// Microphone permission, which reading a virtual device's input needs. `Unknown` = not asked yet.
    pub mic: TccStatus,
    /// Protocol version of the installed driver plug-in; `None` = driver not installed.
    pub driver_version: Option<String>,
    /// Measured delay of each running device's main path: mic, pass-thru and monitors (devices that
    /// have not run a cycle yet are absent). In an unsplit device it is also the apps' path.
    pub delays: Vec<(String, Delay)>,
    /// The path of tapped apps where a split vdev keeps them in an aggregate of their own; it is longer
    /// than `delays`. Absent for devices without a running app tap.
    pub tap_delays: Vec<(String, Delay)>,
    /// macOS Low Power Mode is on: clocks drop and small audio delays may glitch.
    pub low_power_mode: bool,
    /// Processor overloads (`kAudioDeviceProcessorOverload`: an IOProc did not finish in time) the HAL
    /// reported for each running virtual device's aggregates since it was built; devices without one
    /// are absent.
    pub xruns: Vec<(String, u64)>,
    /// Virtual device ids with a tapped app that is producing audio while the tap delivers nothing but
    /// zeros for 3 s or more: what a denied System Audio Recording permission looks like (the HAL
    /// reports no error).
    pub tap_silent: Vec<String>,
    /// Devices the config uses that another process holds in hog mode.
    pub hogged: Vec<HogInfo>,
    /// Names of Bluetooth devices that look switched to the headset (HFP) profile: a sample rate of
    /// 24 kHz or less, or fewer output channels than when the engine built its aggregate, while an input
    /// stream of the device is open.
    pub bluetooth_hfp: Vec<String>,
    /// The driver's loop read-ahead switch as it reads now (`Unsupported` = no driver or an old one).
    pub read_ahead: ReadAheadState,
    /// Per virtual device id: the driver's late and short read counters (`'srov'`) of its visible and
    /// companion device added up, sorted like `xruns`. Devices the driver does not publish are absent.
    pub driver_overloads: Vec<(String, u64)>,
    /// The engine switched read-ahead off because a driver overload or an xrun grew while it was on;
    /// sticky until options ask for read-ahead again.
    pub read_ahead_tripped: bool,
}

/// What the driver says about its loop read-ahead switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadAheadState {
    /// No driver, or one without the property (reinstall it).
    #[default]
    Unsupported,
    Off,
    On,
}

impl ReadAheadState {
    /// The property's value (`None` = the driver has no such property).
    pub fn from_prop(v: Option<i64>) -> Self {
        match v {
            None => Self::Unsupported,
            Some(0) => Self::Off,
            Some(_) => Self::On,
        }
    }
    pub fn is_on(self) -> bool {
        self == Self::On
    }
}

/// Engine behaviour the user can change; the app keeps it in its settings and sends it to the backend.
/// An environment variable the engine already honours (`SR_TAP_SPLIT`, `SR_TAP_CUSHION`) wins over it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineOptions {
    /// Loop read-ahead in the driver (experimental, saves up to ~1 buffer per ring traversal).
    pub read_ahead: bool,
    /// Captured apps get an aggregate of their own, so mics and pass-thru keep the short path.
    pub tap_split: bool,
    /// Safety margin of an app capture in whole buffers; the engine clamps it to 1..=2.
    pub tap_cushion_blocks: u8,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            read_ahead: false,
            tap_split: true,
            tap_cushion_blocks: 1,
        }
    }
}
/// Result of a loopback self-test: a tone into a virtual device, read back from its input.
#[derive(Clone, Debug, PartialEq)]
pub struct SelfTest {
    pub rms_dbfs: f64,
    pub dropouts: u64,
    pub rate: f64,
    pub passed: bool,
}
pub trait Backend {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()>; // idempotent diff: driver devices, aggregates, taps, plans
    fn hardware(&self) -> Vec<HwDevice>; // excludes our own vdevs and aggregates
    fn apps(&self) -> Vec<AppInfo>; // processes known to the HAL, deduped by bundle_id
    fn meters(&self, device_id: &str) -> Option<MeterReading>;
    fn status(&self) -> EngineStatus {
        EngineStatus::default()
    }
    /// Loopback-test virtual device `id`. Blocks for a couple of seconds (and while a permission
    /// prompt is open), so callers run it off the UI thread.
    fn self_test(&mut self, _id: &str) -> anyhow::Result<SelfTest> {
        anyhow::bail!("self-test is not supported by this backend")
    }
    /// Time one click from `id`'s output to its input, in milliseconds: the delay through the virtual
    /// device as it is routed now. Blocks for a couple of seconds, so callers run it off the UI thread.
    fn measure_delay(&mut self, _id: &str) -> anyhow::Result<f64> {
        anyhow::bail!("delay measurement is not supported by this backend")
    }
    /// New engine options. The engine acts on the ones that changed (rebuilding devices where needed);
    /// backends without such options ignore them.
    fn set_options(&mut self, _o: &EngineOptions) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_ahead_state_maps_the_property_value() {
        assert_eq!(ReadAheadState::from_prop(None), ReadAheadState::Unsupported);
        assert_eq!(ReadAheadState::from_prop(Some(0)), ReadAheadState::Off);
        assert_eq!(ReadAheadState::from_prop(Some(1)), ReadAheadState::On);
        assert_eq!(ReadAheadState::from_prop(Some(7)), ReadAheadState::On);
        assert_eq!(ReadAheadState::default(), ReadAheadState::Unsupported);
    }

    #[test]
    fn engine_options_default_and_fill_missing_fields() {
        let d = EngineOptions::default();
        assert!(!d.read_ahead && d.tap_split && d.tap_cushion_blocks == 1);
        assert_eq!(serde_json::from_str::<EngineOptions>("{}").unwrap(), d);
        let o: EngineOptions = serde_json::from_str(r#"{"read_ahead":true}"#).unwrap();
        assert!(o.read_ahead && o.tap_split);
    }

    #[test]
    fn frames_convert_to_milliseconds() {
        assert_eq!(frames_to_ms(48, 48_000), 1.0);
        assert!((frames_to_ms(512, 48_000) - 10.666_666).abs() < 1e-3);
        assert_eq!(frames_to_ms(0, 48_000), 0.0);
        assert_eq!(frames_to_ms(480, 0), 0.0);
        let d = Delay {
            frames: 1024,
            rate_hz: 44_100,
        };
        assert!((d.ms() - 23.219).abs() < 1e-3);
    }
}
