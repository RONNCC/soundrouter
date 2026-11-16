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
pub trait Backend {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()>; // idempotent diff: driver devices, aggregates, taps, plans
    fn hardware(&self) -> Vec<HwDevice>; // excludes our own vdevs and aggregates
    fn apps(&self) -> Vec<AppInfo>; // processes known to the HAL, deduped by bundle_id
    fn meters(&self, device_id: &str) -> Option<MeterReading>;
}
