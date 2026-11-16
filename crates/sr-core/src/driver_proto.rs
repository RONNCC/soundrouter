pub const PLUGIN_BUNDLE_ID: &str = "com.ronncc.soundrouter.driver";
pub const APP_BUNDLE_ID: &str = "com.ronncc.soundrouter";
pub const DEVICE_UID_PREFIX: &str = "soundrouter:";
pub const MANUFACTURER: &str = "SoundRouter";
/// Custom property on the plug-in object. Global scope, element main. Data = CFStringRef holding JSON Vec<DriverDevice>.
/// Get returns the current list. Set replaces it. Advertised in kAudioObjectPropertyCustomPropertyInfoList as 'cfst'.
pub const PROP_DEVICES: u32 = u32::from_be_bytes(*b"srdv");
pub const STORAGE_KEY: &str = "devices"; // host WriteToStorage key, same JSON
pub const SAMPLE_RATES: [f64; 3] = [44_100.0, 48_000.0, 96_000.0];
pub const DEFAULT_RATE: f64 = 48_000.0;
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DriverDevice {
    pub uid: String,
    pub name: String,
    pub channels: u16,
}
