//! Property model for the plug-in, the two devices of each slot, and their streams.
use crate::state::*;
use coreaudio_sys::*;
use sr_core::driver_proto::{
    MANUFACTURER, PROP_DEVICES, PROP_OVERLOADS, PROP_VERSION, SAMPLE_RATES, companion_uid,
};
use std::mem::size_of;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

pub const PROTOCOL_VERSION: &str = "1";
/// Clock domains are per slot: V and C share one, different slots may run at different rates.
const CLOCK_DOMAIN_BASE: u32 = 0x5352_0000;

pub enum V {
    U32(u32),
    F32(f32),
    F64(f64),
    Str(String),
    Ids(Vec<u32>),
    Asbd(AudioStreamBasicDescription),
    Ranged(Vec<AudioStreamRangedDescription>),
    Ranges(Vec<AudioValueRange>),
    Custom(Vec<AudioServerPlugInCustomPropertyInfo>),
    Pair([u32; 2]),
    /// 64-bit counter as a CFNumber, built at read time.
    Count(u64),
    /// The qualifier-less level conversions: input value is read from the caller's buffer in get_data.
    ScalarToDb,
    DbToScalar,
    /// Current device list as a CFArray, built at read time.
    DeviceList,
    /// UID -> device id; needs the qualifier, resolved in get_data.
    TranslateUid,
}

pub fn asbd(ch: u32, rate: f64) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat
            | kAudioFormatFlagIsPacked
            | kAudioFormatFlagsNativeEndian,
        mBytesPerPacket: 4 * ch,
        mFramesPerPacket: 1,
        mBytesPerFrame: 4 * ch,
        mChannelsPerFrame: ch,
        mBitsPerChannel: 32,
        mReserved: 0,
    }
}

fn live_devices() -> Vec<u32> {
    let mut v = vec![];
    for (s, slot) in SLOTS.iter().enumerate() {
        if slot.alive.load(Acquire) {
            v.push(encode(s, Side::Visible, Part::Device));
            v.push(encode(s, Side::Engine, Part::Device));
        }
    }
    v
}

pub fn find_uid(uid: &str) -> Option<u32> {
    for (s, slot) in SLOTS.iter().enumerate() {
        if slot.alive.load(Acquire) {
            let own = slot.meta().0.clone();
            if own == uid {
                return Some(encode(s, Side::Visible, Part::Device));
            }
            if companion_uid(&own) == uid {
                return Some(encode(s, Side::Engine, Part::Device));
            }
        }
    }
    None
}

pub fn prop(obj: u32, a: &AudioObjectPropertyAddress) -> Option<V> {
    use V::*;
    let sel = a.mSelector;
    if obj == PLUGIN {
        return Some(match sel {
            kAudioObjectPropertyBaseClass => U32(kAudioObjectClassID),
            kAudioObjectPropertyClass => U32(kAudioPlugInClassID),
            kAudioObjectPropertyOwner => U32(kAudioObjectUnknown),
            kAudioObjectPropertyManufacturer => Str(MANUFACTURER.into()),
            kAudioObjectPropertyOwnedObjects | kAudioPlugInPropertyDeviceList => {
                Ids(live_devices())
            }
            kAudioPlugInPropertyBoxList | kAudioPlugInPropertyClockDeviceList => Ids(vec![]),
            kAudioPlugInPropertyTranslateUIDToBox => U32(kAudioObjectUnknown),
            kAudioPlugInPropertyTranslateUIDToDevice => TranslateUid,
            kAudioPlugInPropertyResourceBundle => Str(String::new()),
            kAudioObjectPropertyCustomPropertyInfoList => Custom(vec![
                AudioServerPlugInCustomPropertyInfo {
                    mSelector: PROP_DEVICES,
                    mPropertyDataType: kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
                    mQualifierDataType: kAudioServerPlugInCustomPropertyDataTypeNone,
                },
                AudioServerPlugInCustomPropertyInfo {
                    mSelector: PROP_VERSION,
                    mPropertyDataType: kAudioServerPlugInCustomPropertyDataTypeCFString,
                    mQualifierDataType: kAudioServerPlugInCustomPropertyDataTypeNone,
                },
            ]),
            PROP_DEVICES => DeviceList,
            PROP_VERSION => Str(PROTOCOL_VERSION.into()),
            _ => return None,
        });
    }
    let (slot, s, side, part) = lookup(obj)?;
    let ch = slot.channels(side);
    let rate = slot.rate(side);
    let engine = side == Side::Engine;
    let dev = encode(s, side, Part::Device);
    Some(match (part, sel) {
        (Part::Device, kAudioObjectPropertyBaseClass) => U32(kAudioObjectClassID),
        (Part::Device, kAudioObjectPropertyClass) => U32(kAudioDeviceClassID),
        (Part::Device, kAudioObjectPropertyOwner) => U32(PLUGIN),
        (Part::Device, kAudioObjectPropertyName) => {
            let n = slot.meta().1.clone();
            Str(if engine { format!("{n} (engine)") } else { n })
        }
        (Part::Device, kAudioObjectPropertyManufacturer) => Str(MANUFACTURER.into()),
        (Part::Device, kAudioDevicePropertyDeviceUID) => {
            let u = slot.meta().0.clone();
            Str(if engine { companion_uid(&u) } else { u })
        }
        (Part::Device, kAudioDevicePropertyModelUID) => Str("SoundRouterLoopback".into()),
        (Part::Device, kAudioDevicePropertyTransportType) => U32(kAudioDeviceTransportTypeVirtual),
        (Part::Device, kAudioDevicePropertyRelatedDevices) => Ids(vec![obj]),
        (Part::Device, kAudioDevicePropertyClockDomain) => U32(CLOCK_DOMAIN_BASE + s as u32 + 1),
        (Part::Device, kAudioDevicePropertyDeviceIsAlive) => U32(1),
        (Part::Device, kAudioDevicePropertyDeviceIsRunning) => {
            U32((slot.clients[side as usize].load(Acquire) > 0) as u32)
        }
        (Part::Device, kAudioDevicePropertyDeviceCanBeDefaultDevice) => U32(!engine as u32),
        (Part::Device, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice) => U32(0),
        (Part::Device, kAudioDevicePropertyLatency | kAudioDevicePropertySafetyOffset) => U32(0),
        (Part::Device, kAudioObjectPropertyOwnedObjects) => {
            let mut v = streams(s, side, a.mScope);
            v.extend(controls(s, side, a.mScope));
            Ids(v)
        }
        (Part::Device, kAudioDevicePropertyStreams) => Ids(streams(s, side, a.mScope)),
        (Part::Device, kAudioObjectPropertyControlList) => Ids(controls(s, side, a.mScope)),
        (Part::Device, kAudioObjectPropertyCustomPropertyInfoList) => {
            Custom(vec![AudioServerPlugInCustomPropertyInfo {
                mSelector: PROP_OVERLOADS,
                mPropertyDataType: kAudioServerPlugInCustomPropertyDataTypeCFPropertyList,
                mQualifierDataType: kAudioServerPlugInCustomPropertyDataTypeNone,
            }])
        }
        (Part::Device, PROP_OVERLOADS) => Count(slot.overloads[side as usize].load(Relaxed)),
        (Part::Device, kAudioDevicePropertyNominalSampleRate) => F64(rate),
        (Part::Device, kAudioDevicePropertyAvailableNominalSampleRates) => Ranges(rate_ranges()),
        (Part::Device, kAudioDevicePropertyIsHidden) => U32((engine && COMPANION_HIDDEN) as u32),
        (Part::Device, kAudioDevicePropertyZeroTimeStampPeriod) => U32(RING_FRAMES),
        (Part::Device, kAudioDevicePropertyPreferredChannelsForStereo) => Pair([1, 2.min(ch)]),
        (p, _) if control_kind(p).is_some() => return control_prop(slot, dev, p, sel),
        (Part::Input | Part::Output, kAudioObjectPropertyBaseClass) => U32(kAudioObjectClassID),
        (Part::Input | Part::Output, kAudioObjectPropertyClass) => U32(kAudioStreamClassID),
        (Part::Input | Part::Output, kAudioObjectPropertyOwner) => U32(dev),
        (Part::Input | Part::Output, kAudioObjectPropertyOwnedObjects) => Ids(vec![]),
        (Part::Input | Part::Output, kAudioStreamPropertyIsActive) => U32(1),
        (p @ (Part::Input | Part::Output), kAudioStreamPropertyDirection) => {
            U32((p == Part::Input) as u32)
        }
        (Part::Input | Part::Output, kAudioStreamPropertyTerminalType) => {
            U32(kAudioStreamTerminalTypeLine)
        }
        (Part::Input | Part::Output, kAudioStreamPropertyStartingChannel) => U32(1),
        (Part::Input | Part::Output, kAudioStreamPropertyLatency) => U32(0),
        (
            Part::Input | Part::Output,
            kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat,
        ) => Asbd(asbd(ch, rate)),
        (
            Part::Input | Part::Output,
            kAudioStreamPropertyAvailableVirtualFormats
            | kAudioStreamPropertyAvailablePhysicalFormats,
        ) => Ranged(
            rate_ranges()
                .into_iter()
                .map(|r| AudioStreamRangedDescription {
                    mFormat: asbd(ch, r.mMinimum),
                    mSampleRateRange: r,
                })
                .collect(),
        ),
        _ => return None,
    })
}

fn streams(s: usize, side: Side, scope: u32) -> Vec<u32> {
    match scope {
        kAudioObjectPropertyScopeInput => vec![encode(s, side, Part::Input)],
        kAudioObjectPropertyScopeOutput => vec![encode(s, side, Part::Output)],
        _ => vec![encode(s, side, Part::Input), encode(s, side, Part::Output)],
    }
}

/// Only V has volume and mute controls; C stays at unity.
fn controls(s: usize, side: Side, scope: u32) -> Vec<u32> {
    if side == Side::Engine {
        return vec![];
    }
    let (input, output) = (
        [
            encode(s, side, Part::VolumeIn),
            encode(s, side, Part::MuteIn),
        ],
        [
            encode(s, side, Part::VolumeOut),
            encode(s, side, Part::MuteOut),
        ],
    );
    match scope {
        kAudioObjectPropertyScopeInput => input.to_vec(),
        kAudioObjectPropertyScopeOutput => output.to_vec(),
        _ => [input, output].concat(),
    }
}

/// (is volume, scope index into `Slot::levels`) for control parts.
pub fn control_kind(p: Part) -> Option<(bool, usize)> {
    match p {
        Part::VolumeIn => Some((true, 0)),
        Part::MuteIn => Some((false, 0)),
        Part::VolumeOut => Some((true, 1)),
        Part::MuteOut => Some((false, 1)),
        _ => None,
    }
}

fn control_prop(slot: &Slot, dev: u32, part: Part, sel: u32) -> Option<V> {
    use V::*;
    let (volume, scope) = control_kind(part)?;
    let level = &slot.levels[scope];
    Some(match sel {
        kAudioObjectPropertyBaseClass => U32(if volume {
            kAudioLevelControlClassID
        } else {
            kAudioBooleanControlClassID
        }),
        kAudioObjectPropertyClass => U32(if volume {
            kAudioVolumeControlClassID
        } else {
            kAudioMuteControlClassID
        }),
        kAudioObjectPropertyOwner => U32(dev),
        kAudioObjectPropertyOwnedObjects => Ids(vec![]),
        kAudioControlPropertyScope => U32(if scope == 0 {
            kAudioObjectPropertyScopeInput
        } else {
            kAudioObjectPropertyScopeOutput
        }),
        kAudioControlPropertyElement => U32(kAudioObjectPropertyElementMain),
        kAudioLevelControlPropertyScalarValue if volume => F32(level.scalar()),
        kAudioLevelControlPropertyDecibelValue if volume => F32(level.db()),
        kAudioLevelControlPropertyDecibelRange if volume => Ranges(vec![AudioValueRange {
            mMinimum: MIN_DB as f64,
            mMaximum: 0.0,
        }]),
        kAudioLevelControlPropertyConvertScalarToDecibels if volume => ScalarToDb,
        kAudioLevelControlPropertyConvertDecibelsToScalar if volume => DbToScalar,
        kAudioBooleanControlPropertyValue if !volume => U32(level.muted() as u32),
        _ => return None,
    })
}

fn rate_ranges() -> Vec<AudioValueRange> {
    SAMPLE_RATES
        .iter()
        .map(|&r| AudioValueRange {
            mMinimum: r,
            mMaximum: r,
        })
        .collect()
}

pub fn settable(obj: u32, sel: u32) -> bool {
    if obj == PLUGIN {
        return sel == PROP_DEVICES;
    }
    match decode(obj) {
        Some((_, _, Part::Device)) => sel == kAudioDevicePropertyNominalSampleRate,
        Some((_, _, p)) if control_kind(p).is_some() => matches!(
            sel,
            kAudioLevelControlPropertyScalarValue
                | kAudioLevelControlPropertyDecibelValue
                | kAudioBooleanControlPropertyValue
        ),
        Some(_) => matches!(
            sel,
            kAudioStreamPropertyVirtualFormat | kAudioStreamPropertyPhysicalFormat
        ),
        None => false,
    }
}

pub fn vsize(v: &V) -> usize {
    match v {
        V::U32(_) | V::F32(_) | V::ScalarToDb | V::DbToScalar => 4,
        V::F64(_) => 8,
        V::Str(_) | V::DeviceList | V::Count(_) => size_of::<CFTypeRef>(),
        V::TranslateUid => 4,
        V::Ids(x) => 4 * x.len(),
        V::Asbd(_) => size_of::<AudioStreamBasicDescription>(),
        V::Ranged(x) => x.len() * size_of::<AudioStreamRangedDescription>(),
        V::Ranges(x) => x.len() * size_of::<AudioValueRange>(),
        V::Custom(x) => x.len() * size_of::<AudioServerPlugInCustomPropertyInfo>(),
        V::Pair(_) => 8,
    }
}

/// Array-valued properties may be truncated to whatever whole elements fit; others need the full size.
pub fn is_array(v: &V) -> bool {
    matches!(v, V::Ids(_) | V::Ranged(_) | V::Ranges(_) | V::Custom(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sr_core::driver_proto::{DEFAULT_RATE, DriverDevice};

    fn a(sel: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: sel,
            mScope: scope,
            mElement: 0,
        }
    }

    #[test]
    fn plugin_lists_are_consistent() {
        let Some(V::Custom(c)) = prop(PLUGIN, &a(kAudioObjectPropertyCustomPropertyInfoList, 0))
        else {
            panic!()
        };
        assert_eq!(
            c.iter().map(|i| i.mSelector).collect::<Vec<_>>(),
            [PROP_DEVICES, PROP_VERSION]
        );
        assert!(settable(PLUGIN, PROP_DEVICES) && !settable(PLUGIN, PROP_VERSION));
        assert!(prop(PLUGIN, &a(kAudioDevicePropertyDeviceUID, 0)).is_none());
        // Nothing alive in a fresh process: no device objects resolve.
        assert!(prop(16, &a(kAudioObjectPropertyName, 0)).is_none());
    }

    #[test]
    fn stream_format_shape() {
        let f = asbd(8, DEFAULT_RATE);
        assert_eq!(
            (f.mBytesPerFrame, f.mChannelsPerFrame, f.mBitsPerChannel),
            (32, 8, 32)
        );
        assert_eq!(
            vsize(&V::Ranges(rate_ranges())),
            3 * size_of::<AudioValueRange>()
        );
        assert!(is_array(&V::Ids(vec![])) && !is_array(&V::U32(0)));
    }

    #[test]
    fn settable_rules() {
        let v = encode(0, Side::Visible, Part::Device);
        let c = encode(0, Side::Engine, Part::Input);
        assert!(settable(v, kAudioDevicePropertyNominalSampleRate));
        assert!(!settable(v, kAudioObjectPropertyName));
        assert!(settable(c, kAudioStreamPropertyVirtualFormat));
        assert!(!settable(3, kAudioDevicePropertyNominalSampleRate));
    }

    fn get<T: Copy>(obj: u32, sel: u32, scope: u32, f: impl Fn(V) -> T) -> T {
        f(prop(obj, &a(sel, scope)).expect("property"))
    }
    fn u(obj: u32, sel: u32) -> u32 {
        get(
            obj,
            sel,
            0,
            |v| if let V::U32(x) = v { x } else { panic!() },
        )
    }
    fn f(obj: u32, sel: u32) -> f32 {
        get(
            obj,
            sel,
            0,
            |v| if let V::F32(x) = v { x } else { panic!() },
        )
    }

    #[test]
    fn v_has_volume_and_mute_per_scope_and_c_has_none() {
        SLOTS[7].activate(DriverDevice {
            uid: "p7".into(),
            name: "p".into(),
            channels: 2,
        });
        let (v, c) = (
            encode(7, Side::Visible, Part::Device),
            encode(7, Side::Engine, Part::Device),
        );
        let ids = |obj, sel, scope| match prop(obj, &a(sel, scope)) {
            Some(V::Ids(x)) => x,
            _ => panic!(),
        };
        let ctl = |p| encode(7, Side::Visible, p);
        let list = |scope| ids(v, kAudioObjectPropertyControlList, scope);
        assert_eq!(
            list(kAudioObjectPropertyScopeInput),
            [ctl(Part::VolumeIn), ctl(Part::MuteIn)]
        );
        assert_eq!(
            list(kAudioObjectPropertyScopeOutput),
            [ctl(Part::VolumeOut), ctl(Part::MuteOut)]
        );
        assert_eq!(list(kAudioObjectPropertyScopeGlobal).len(), 4);
        // The HAL finds controls through OwnedObjects too; Streams stays streams only.
        assert_eq!(ids(v, kAudioObjectPropertyOwnedObjects, 0).len(), 6);
        assert_eq!(ids(v, kAudioDevicePropertyStreams, 0).len(), 2);
        assert!(list_of(c).is_empty());

        let (vol, mute) = (ctl(Part::VolumeOut), ctl(Part::MuteIn));
        assert_eq!(
            u(vol, kAudioObjectPropertyClass),
            kAudioVolumeControlClassID
        );
        assert_eq!(
            u(vol, kAudioObjectPropertyBaseClass),
            kAudioLevelControlClassID
        );
        assert_eq!(u(mute, kAudioObjectPropertyClass), kAudioMuteControlClassID);
        assert_eq!(
            u(mute, kAudioObjectPropertyBaseClass),
            kAudioBooleanControlClassID
        );
        assert_eq!(u(vol, kAudioObjectPropertyOwner), v);
        assert_eq!(
            u(vol, kAudioControlPropertyScope),
            kAudioObjectPropertyScopeOutput
        );
        assert_eq!(
            u(mute, kAudioControlPropertyScope),
            kAudioObjectPropertyScopeInput
        );
        assert_eq!(u(vol, kAudioControlPropertyElement), 0);
        assert_eq!(f(vol, kAudioLevelControlPropertyScalarValue), 1.0);
        assert_eq!(f(vol, kAudioLevelControlPropertyDecibelValue), 0.0);
        let Some(V::Ranges(r)) = prop(vol, &a(kAudioLevelControlPropertyDecibelRange, 0)) else {
            panic!()
        };
        assert_eq!((r[0].mMinimum, r[0].mMaximum), (-64.0, 0.0));
        assert_eq!(u(mute, kAudioBooleanControlPropertyValue), 0);
        // Volume has no Value, mute has no ScalarValue.
        assert!(prop(vol, &a(kAudioBooleanControlPropertyValue, 0)).is_none());
        assert!(prop(mute, &a(kAudioLevelControlPropertyScalarValue, 0)).is_none());
        for sel in [
            kAudioLevelControlPropertyScalarValue,
            kAudioLevelControlPropertyDecibelValue,
        ] {
            assert!(settable(vol, sel));
        }
        assert!(settable(mute, kAudioBooleanControlPropertyValue));
        assert!(!settable(vol, kAudioLevelControlPropertyDecibelRange));
        assert!(!settable(vol, kAudioObjectPropertyClass));
        // Device list stays free of control ids; custom overload property is read-only.
        assert!(matches!(prop(v, &a(PROP_OVERLOADS, 0)), Some(V::Count(0))));
        assert!(!settable(v, PROP_OVERLOADS));
    }

    fn list_of(dev: u32) -> Vec<u32> {
        match prop(dev, &a(kAudioObjectPropertyControlList, 0)) {
            Some(V::Ids(x)) => x,
            _ => panic!(),
        }
    }

    #[test]
    fn can_be_default_only_for_v_and_never_as_system_device() {
        SLOTS[4].activate(DriverDevice {
            uid: "p4".into(),
            name: "p".into(),
            channels: 2,
        });
        let (v, c) = (
            encode(4, Side::Visible, Part::Device),
            encode(4, Side::Engine, Part::Device),
        );
        assert_eq!(u(v, kAudioDevicePropertyDeviceCanBeDefaultDevice), 1);
        assert_eq!(u(v, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice), 0);
        assert_eq!(u(c, kAudioDevicePropertyDeviceCanBeDefaultDevice), 0);
        assert_eq!(u(c, kAudioDevicePropertyDeviceCanBeDefaultSystemDevice), 0);
    }
}
