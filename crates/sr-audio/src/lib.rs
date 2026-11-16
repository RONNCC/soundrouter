//! CoreAudio engine (slice C): process taps + private aggregates + one IOProc per virtual device.
pub mod apps;
pub mod driver;
pub mod engine;
pub mod hal;
pub mod layout;
pub mod loopback;
pub mod power;
pub mod tcc;

pub use engine::{Engine, IoState};

/// e.g. "Version 14.7.1 (Build 23H222)".
pub fn os_version() -> String {
    objc2_foundation::NSProcessInfo::processInfo()
        .operatingSystemVersionString()
        .to_string()
}

#[cfg(test)]
mod hardware_tests {
    use sr_core::backend::Backend;

    /// Needs a Mac with CoreAudio; run with `cargo test -p sr-audio -- --ignored`.
    #[test]
    #[ignore = "hardware"]
    fn enumerates_hardware_and_apps() {
        let e = crate::Engine::new().unwrap();
        assert!(!e.hardware().is_empty());
        let _ = e.apps();
    }

    /// Needs the SoundRouter driver installed (sudo) plus mic TCC for the test binary.
    #[test]
    #[ignore = "needs installed driver"]
    fn loopback_v1() {
        let r = crate::loopback::loopback_test("soundrouter:v1", 3).unwrap();
        assert!(r.passed(), "{r:?}");
    }
}
