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
    /// Measured delay per running device (devices that have not run a cycle yet are absent).
    pub delays: Vec<(String, Delay)>,
    /// The path of tapped apps where a split vdev keeps them in an aggregate of their own; it is longer
    /// than the device's other paths. Absent for devices without a running app tap.
    pub tap_delays: Vec<(String, Delay)>,
    /// macOS Low Power Mode is on: clocks drop and small audio delays may glitch.
    pub low_power_mode: bool,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
