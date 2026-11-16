//! CoreFoundation helpers: strings and the device-list property list.
use crate::config;
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use sr_core::driver_proto::{DriverDevice, MAX_DEVICES};
use std::ffi::c_void;

const MAX_STR_UTF16: isize = 65536;

/// Borrow a host-owned CF pointer (null -> None). Caller keeps it alive for `'a`.
pub unsafe fn borrow<'a>(p: *const c_void) -> Option<&'a CFType> {
    unsafe { p.cast::<CFType>().as_ref() }
}

pub fn rstr(v: Option<&CFType>) -> Option<String> {
    let s = v?.downcast_ref::<CFString>()?;
    if s.length() > MAX_STR_UTF16 {
        return None;
    }
    Some(s.to_string())
}

/// Parse and validate a device-list property list (an untrusted CFArray of CFDictionary).
pub fn parse_devices(plist: Option<&CFType>) -> Result<Vec<DriverDevice>, String> {
    let arr = plist
        .and_then(|p| p.downcast_ref::<CFArray>())
        .ok_or("not an array")?;
    let n = arr.len();
    if n > MAX_DEVICES {
        return Err(format!("{n} devices, max {MAX_DEVICES}"));
    }
    // SAFETY: element type is only used through downcast_ref, which checks the CF type id.
    let arr = unsafe { arr.cast_unchecked::<CFType>() };
    let (k_uid, k_name, k_ch) = (
        CFString::from_static_str("uid"),
        CFString::from_static_str("name"),
        CFString::from_static_str("channels"),
    );
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let d = arr
            .get(i)
            .ok_or_else(|| format!("device {i}: not a dictionary"))?;
        let d = d
            .downcast_ref::<CFDictionary>()
            .ok_or_else(|| format!("device {i}: not a dictionary"))?;
        // SAFETY: keys/values are only used through checked downcasts.
        let d = unsafe { d.cast_unchecked::<CFString, CFType>() };
        let (Some(uid), Some(name)) = (
            rstr(d.get(&k_uid).as_deref()),
            rstr(d.get(&k_name).as_deref()),
        ) else {
            return Err(format!("device {i}: uid/name must be strings"));
        };
        let ch = d
            .get(&k_ch)
            .and_then(|n| n.downcast::<CFNumber>().ok())
            .and_then(|n| n.as_i32())
            .ok_or_else(|| format!("device {i}: channels must be an integer"))?;
        let channels =
            u16::try_from(ch).map_err(|_| format!("device {i}: channels out of range"))?;
        out.push(DriverDevice {
            uid,
            name,
            channels,
        });
    }
    config::validate(out)
}

/// Build the property list for a device list.
pub fn devices_to_plist(
    list: &[DriverDevice],
) -> CFRetained<CFArray<CFDictionary<CFString, CFType>>> {
    let keys = [
        CFString::from_static_str("uid"),
        CFString::from_static_str("name"),
        CFString::from_static_str("channels"),
    ];
    let dicts: Vec<_> = list
        .iter()
        .map(|d| {
            let (uid, name) = (CFString::from_str(&d.uid), CFString::from_str(&d.name));
            let num = CFNumber::new_i32(d.channels.into());
            CFDictionary::from_slices(
                &[&*keys[0], &*keys[1], &*keys[2]],
                &[&**uid, &**name, &**num],
            )
        })
        .collect();
    CFArray::from_retained_objects(&dicts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn d(uid: &str, name: &str, channels: u16) -> DriverDevice {
        DriverDevice {
            uid: uid.into(),
            name: name.into(),
            channels,
        }
    }

    #[test]
    fn plist_round_trip() {
        let list = vec![
            d("soundrouter:v1", "Loopback Audio", 2),
            d("x", "Ünïcode ✓", 64),
        ];
        let p = devices_to_plist(&list);
        assert_eq!(parse_devices(Some(&p)).unwrap(), list);
        let empty = devices_to_plist(&[]);
        assert_eq!(parse_devices(Some(&empty)).unwrap(), vec![]);
    }

    #[test]
    fn parse_rejects_garbage() {
        let s = CFString::from_static_str("nope");
        assert!(parse_devices(Some(&s)).is_err());
        assert!(parse_devices(None).is_err());
        let arr = CFArray::from_retained_objects(&[CFString::from_static_str("x")]);
        assert!(parse_devices(Some(&arr)).is_err());
        let dup = devices_to_plist(&[d("a", "n", 2), d("a", "m", 2)]);
        assert!(parse_devices(Some(&dup)).unwrap_err().contains("duplicate"));
    }

    #[derive(Debug, Clone)]
    enum Node {
        Str(String),
        Int(i64),
        Dbl(f64),
        Arr(Vec<Node>),
        Dict(Vec<(String, Node)>),
    }

    fn build(n: &Node) -> CFRetained<CFType> {
        match n {
            Node::Str(s) => CFString::from_str(s).into(),
            Node::Int(i) => CFNumber::new_i64(*i).into(),
            Node::Dbl(f) => CFNumber::new_f64(*f).into(),
            Node::Arr(v) => {
                let items: Vec<_> = v.iter().map(build).collect();
                unsafe { CFRetained::cast_unchecked(CFArray::from_retained_objects(&items)) }
            }
            Node::Dict(kv) => {
                let keys: Vec<_> = kv.iter().map(|(k, _)| CFString::from_str(k)).collect();
                let vals: Vec<_> = kv.iter().map(|(_, v)| build(v)).collect();
                let kr: Vec<&CFString> = keys.iter().map(|k| &**k).collect();
                let vr: Vec<&CFType> = vals.iter().map(|v| &**v).collect();
                unsafe { CFRetained::cast_unchecked(CFDictionary::from_slices(&kr, &vr)) }
            }
        }
    }

    fn arb_str() -> impl Strategy<Value = String> {
        prop_oneof![
            any::<String>(),
            Just("a\0b".to_string()),
            Just("😀✓é".to_string()),
            Just("x".repeat(70_000)),
            "[a-z:]{0,10}",
        ]
    }

    fn arb_num() -> impl Strategy<Value = Node> {
        prop_oneof![
            any::<i64>().prop_map(Node::Int),
            (-5i64..70_000).prop_map(Node::Int),
            Just(Node::Int(i64::MIN)),
            Just(Node::Dbl(f64::NAN)),
            Just(Node::Dbl(f64::INFINITY)),
            any::<f64>().prop_map(Node::Dbl),
        ]
    }

    fn arb_leaf() -> impl Strategy<Value = Node> {
        prop_oneof![arb_str().prop_map(Node::Str), arb_num()]
    }

    fn arb_key() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("uid".to_string()),
            Just("name".to_string()),
            Just("channels".to_string()),
            arb_str(),
        ]
    }

    /// A dictionary biased towards the real schema, with random value types.
    fn arb_dev() -> impl Strategy<Value = Node> {
        let val = prop_oneof![arb_leaf(), Just(Node::Arr(vec![]))];
        prop::collection::vec((arb_key(), val), 0..5).prop_map(Node::Dict)
    }

    fn arb_tree() -> impl Strategy<Value = Node> {
        prop_oneof![
            arb_leaf(),
            prop::collection::vec(arb_dev(), 0..8).prop_map(Node::Arr),
            prop::collection::vec(arb_leaf(), 0..1000).prop_map(Node::Arr),
            prop::collection::vec(arb_dev(), 1000..1001).prop_map(Node::Arr),
            arb_dev(),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn parse_never_panics(t in arb_tree()) {
            let cf = build(&t);
            if let Ok(list) = parse_devices(Some(&cf)) {
                // validated output must survive a round trip unchanged
                let back = devices_to_plist(&list);
                prop_assert_eq!(parse_devices(Some(&back)).unwrap(), list);
            }
        }

        #[test]
        fn valid_lists_round_trip(
            n in 0usize..=MAX_DEVICES,
            ch in 1u16..=2,
            name in "[a-zA-Z0-9✓]{1,20}",
        ) {
            let list: Vec<_> = (0..n).map(|i| d(&format!("u{i}"), &name, ch)).collect();
            let p = devices_to_plist(&list);
            prop_assert_eq!(parse_devices(Some(&p)).unwrap(), list);
        }
    }

    /// Deterministic leak/over-release check via retain counts held across the loop.
    #[test]
    fn round_trip_does_not_leak() {
        let list: Vec<_> = (0..4)
            .map(|i| d(&format!("uid-{i}"), "Loopback", 2))
            .collect();
        let held = devices_to_plist(&list);
        let dict = unsafe { held.get_unchecked(0) };
        let (arr_rc, dict_rc) = (held.retain_count(), dict.retain_count());
        for _ in 0..1_000 {
            // get_data-style +1 handoff: raw pointer out, reclaimed exactly once.
            let raw = CFRetained::into_raw(held.clone()).as_ptr() as *const c_void;
            let back =
                unsafe { CFRetained::from_raw(std::ptr::NonNull::new(raw as *mut _).unwrap()) };
            assert_eq!(parse_devices(Some(&back)).unwrap(), list);
            let fresh = devices_to_plist(&list);
            assert_eq!(parse_devices(Some(&fresh)).unwrap().len(), 4);
        }
        assert_eq!(held.retain_count(), arr_rc);
        assert_eq!(dict.retain_count(), dict_rc);
    }
}
