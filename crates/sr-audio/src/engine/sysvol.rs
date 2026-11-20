//! V's volume and mute controls -> the mixer's `SysGain`.
//!
//! The driver applies V's output-scope controls to what apps play into V (`C.in`). App taps never pass through
//! V, so with V as the system output the volume keys would not reach them: the engine reads the same controls
//! and the mixer applies them to the app sources of that device. The HAL notifies on a listener thread; the
//! audio thread only loads one atomic per block.
use std::sync::Arc;

use objc2_core_audio::*;
use sr_core::driver_proto::volume_gain;
use sr_core::mixer::SysGain;

use super::{Listener, hal};

/// Listeners that keep `gain` at V's current volume and mute. Empty (gain stays unity) when V is absent or the
/// installed driver has no controls.
pub(super) fn follow(uid: &str, gain: Arc<SysGain>) -> Vec<Listener> {
    let Some(v) = hal::device_by_uid(uid) else {
        return Vec::new();
    };
    let (mut vol, mut mute) = (None, None);
    for c in hal::get_u32s(v, kAudioObjectPropertyControlList).unwrap_or_default() {
        let class = hal::get_global::<u32>(c, kAudioObjectPropertyClass).ok();
        let scope = hal::get_global::<u32>(c, kAudioControlPropertyScope).ok();
        if scope != Some(kAudioObjectPropertyScopeOutput) {
            continue;
        }
        if class == Some(kAudioVolumeControlClassID) {
            vol = Some(c);
        } else if class == Some(kAudioMuteControlClassID) {
            mute = Some(c);
        }
    }
    let update = Arc::new(move || {
        let scalar = vol
            .and_then(|c| hal::get_global::<f32>(c, kAudioLevelControlPropertyScalarValue).ok())
            .unwrap_or(1.0);
        let muted = mute
            .and_then(|c| hal::get_global::<u32>(c, kAudioBooleanControlPropertyValue).ok())
            .is_some_and(|m| m != 0);
        gain.set(volume_gain(scalar, muted));
    });
    update();
    [
        (vol, kAudioLevelControlPropertyScalarValue),
        (mute, kAudioBooleanControlPropertyValue),
    ]
    .into_iter()
    .filter_map(|(c, sel)| {
        let u = update.clone();
        Listener::with(c?, sel, kAudioObjectPropertyScopeGlobal, move || u())
            .map_err(|e| log::warn!("no volume listener on '{uid}': {e:#}"))
            .ok()
    })
    .collect()
}
