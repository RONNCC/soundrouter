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
}
#[derive(Clone, Debug, Default)]
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
}
