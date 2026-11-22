//! Control client for the SoundRouter HAL plug-in ('srcf' device list property).
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use sr_core::driver_proto::{DriverDevice, PLUGIN_BUNDLE_ID, PROP_DEVICES, PROP_VERSION};

use crate::hal;

pub fn find_plugin() -> Option<u32> {
    hal::plugin_by_bundle_id(PLUGIN_BUNDLE_ID)
}

/// Protocol version string, if the driver reports one.
pub fn version(plugin: u32) -> Option<String> {
    hal::get_string(plugin, PROP_VERSION).ok()
}

pub fn devices_to_array(
    list: &[DriverDevice],
) -> Retained<NSArray<NSDictionary<NSString, AnyObject>>> {
    let dicts: Vec<_> = list
        .iter()
        .map(|d| {
            let (uid, name, ch) = (
                NSString::from_str(&d.uid),
                NSString::from_str(&d.name),
                NSNumber::new_i32(i32::from(d.channels)),
            );
            hal::dict(&[("uid", &uid), ("name", &name), ("channels", &ch)])
        })
        .collect();
    NSArray::from_retained_slice(&dicts)
}

pub fn array_to_devices(arr: &AnyObject) -> Result<Vec<DriverDevice>> {
    let arr = arr
        .downcast_ref::<NSArray<AnyObject>>()
        .context("srcf value is not an array")?;
    arr.iter()
        .map(|item| {
            let d = item
                .downcast_ref::<NSDictionary>()
                .context("srcf entry is not a dictionary")?;
            let s = |k: &str| -> Result<String> {
                let key = NSString::from_str(k);
                let v = d
                    .objectForKey(&key)
                    .with_context(|| format!("srcf entry missing '{k}'"))?;
                Ok(v.downcast_ref::<NSString>()
                    .with_context(|| format!("'{k}' is not a string"))?
                    .to_string())
            };
            let key = NSString::from_str("channels");
            let ch = d
                .objectForKey(&key)
                .context("srcf entry missing 'channels'")?;
            let ch = ch
                .downcast_ref::<NSNumber>()
                .context("'channels' is not a number")?
                .as_i32();
            Ok(DriverDevice {
                uid: s("uid")?,
                name: s("name")?,
                channels: u16::try_from(ch).context("channels out of range")?,
            })
        })
        .collect()
}

pub fn get_devices(plugin: u32) -> Result<Vec<DriverDevice>> {
    let p: *mut AnyObject = hal::get_global(plugin, PROP_DEVICES)
        .map_err(|st| anyhow::anyhow!("get 'srcf' failed: OSStatus {}", hal::fourcc(st)))?;
    let obj = unsafe { Retained::from_raw(p) }.context("driver returned a null device list")?;
    array_to_devices(&obj)
}

pub fn set_devices(plugin: u32, list: &[DriverDevice]) -> Result<()> {
    let arr = devices_to_array(list);
    hal::set_object(plugin, PROP_DEVICES, &arr)
        .map_err(|st| anyhow::anyhow!("set 'srcf' failed: OSStatus {}", hal::fourcc(st)))
}

/// Poll until every uid resolves to a device or `timeout` passes; returns the unresolved ones.
pub fn wait_for_uids(uids: &[String], timeout: Duration) -> Vec<String> {
    let t0 = Instant::now();
    loop {
        let missing: Vec<String> = uids
            .iter()
            .filter(|u| hal::device_by_uid(u).is_none())
            .cloned()
            .collect();
        if missing.is_empty() || t0.elapsed() >= timeout {
            return missing;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn require_plugin() -> Result<u32> {
    match find_plugin() {
        Some(p) => Ok(p),
        None => bail!(
            "SoundRouter driver not installed ({PLUGIN_BUNDLE_ID} not loaded). Install it from the app's setup \
             sheet, the SoundRouter .pkg, or a checkout: packaging/driver-dev.sh install"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_roundtrip() {
        let list = vec![
            DriverDevice {
                uid: "soundrouter:v1".into(),
                name: "Loopback".into(),
                channels: 2,
            },
            DriverDevice {
                uid: "soundrouter:v2".into(),
                name: "Big".into(),
                channels: 16,
            },
        ];
        let arr = devices_to_array(&list);
        let obj: &AnyObject = &arr;
        assert_eq!(array_to_devices(obj).unwrap(), list);
    }

    #[test]
    fn wait_for_uids_reports_missing_after_timeout() {
        let uid = "soundrouter:no-such-device".to_string();
        let t0 = Instant::now();
        let missing = wait_for_uids(std::slice::from_ref(&uid), Duration::from_millis(200));
        assert_eq!(missing, vec![uid]);
        assert!(t0.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn rejects_bad_shapes() {
        let s = NSString::from_str("x");
        assert!(array_to_devices(&s).is_err());
        let bad = NSArray::from_retained_slice(&[NSString::from_str("x")]);
        assert!(array_to_devices(&bad).is_err());
    }
}
