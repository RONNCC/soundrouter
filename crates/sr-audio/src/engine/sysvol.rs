//! V's volume and mute controls -> the mixer's per-source `SysGain`.
//!
//! The driver applies V's output-scope controls to what apps play into V (`C.in`). App taps never pass
//! through V, so with an app playing to V the volume keys would not reach its tap: the engine reads the same
//! controls and the mixer applies them to that app's source. An app that plays elsewhere (the user switched
//! the output to headphones) must not be scaled by a V nobody can reach any more, so each app source follows
//! V only while its process objects list V among their output devices (`kAudioProcessPropertyDevices`); when
//! none of them can be read, only while V is the system default output. The HAL notifies on listener
//! threads, which do the reads; the audio thread only loads one atomic per app source per block.
use std::sync::{Arc, Mutex, MutexGuard};

use objc2_core_audio::*;
use sr_core::driver_proto::volume_gain;
use sr_core::mixer::SysGain;

use super::{Listener, hal};

/// An aggregate whose main device is V itself (fallback without the companion or with an old driver) reads V's
/// own input loop, where the driver already applied V's controls to what apps play into V, so the mixer must
/// not apply them a second time.
pub(super) fn follows_controls(main: &str, v_uid: &str) -> bool {
    main != v_uid
}

/// Whether an app plays to V. `devices` is, per process object of the app, its output devices (`None`: not
/// readable). Without a single readable process object, V is followed only while it is the system default.
pub(super) fn plays_to_v(
    devices: &[Option<Vec<u32>>],
    v: u32,
    default_output: Option<u32>,
) -> bool {
    if devices.iter().all(Option::is_none) {
        return default_output == Some(v);
    }
    devices.iter().flatten().any(|d| d.contains(&v))
}

struct App {
    source: String,
    objs: Vec<u32>,
    follow: bool,
}

struct Inner {
    /// V's gain from its volume and mute controls.
    gain: f32,
    apps: Vec<App>,
}

struct State {
    v: u32,
    vol: Option<u32>,
    mute: Option<u32>,
    out: Arc<SysGain>,
    inner: Mutex<Inner>,
}

impl State {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self, g: &Inner) {
        for a in &g.apps {
            self.out.set(&a.source, if a.follow { g.gain } else { 1.0 });
        }
    }

    /// A volume or mute change: the apps keep their follow decision.
    fn read_gain(&self) {
        let scalar = self
            .vol
            .and_then(|c| hal::get_global::<f32>(c, kAudioLevelControlPropertyScalarValue).ok())
            .unwrap_or(1.0);
        let muted = self
            .mute
            .and_then(|c| hal::get_global::<u32>(c, kAudioBooleanControlPropertyValue).ok())
            .is_some_and(|m| m != 0);
        let mut g = self.lock();
        g.gain = volume_gain(scalar, muted);
        self.publish(&g);
    }

    /// An app started or stopped playing to V, or the system default output moved.
    fn read_follow(&self) {
        let default =
            hal::get_global::<u32>(hal::SYSTEM, kAudioHardwarePropertyDefaultOutputDevice).ok();
        let mut g = self.lock();
        for a in &mut g.apps {
            let devices: Vec<Option<Vec<u32>>> = a
                .objs
                .iter()
                .map(|&o| {
                    hal::get_u32s_in(
                        o,
                        kAudioProcessPropertyDevices,
                        kAudioObjectPropertyScopeOutput,
                    )
                    .ok()
                })
                .collect();
            a.follow = plays_to_v(&devices, self.v, default);
        }
        self.publish(&g);
    }
}

/// Keeps every app source's `SysGain` at V's volume and mute while that app plays to V, unity otherwise.
/// Inert (gains stay unity) when V is absent, is the aggregate's main, or the installed driver has no controls.
#[derive(Default)]
pub(super) struct SysVol {
    state: Option<Arc<State>>,
    /// V's controls and the default output; fixed for the life of the vdev.
    _controls: Vec<Listener>,
    /// The app process objects' device lists; replaced when the processes change.
    procs: Vec<Listener>,
}

impl SysVol {
    /// `main` is the aggregate's main device uid, `uid` V's.
    pub(super) fn start(main: &str, uid: &str, out: Arc<SysGain>) -> SysVol {
        if !follows_controls(main, uid) {
            return SysVol::default();
        }
        let Some(v) = hal::device_by_uid(uid) else {
            return SysVol::default();
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
        let state = Arc::new(State {
            v,
            vol,
            mute,
            out,
            inner: Mutex::new(Inner {
                gain: 1.0,
                apps: Vec::new(),
            }),
        });
        // Listeners first, then the baseline read: a change in between is notified, one before is in the read.
        let mut watch = vec![(
            hal::SYSTEM,
            kAudioHardwarePropertyDefaultOutputDevice,
            false,
        )];
        watch.extend(vol.map(|c| (c, kAudioLevelControlPropertyScalarValue, true)));
        watch.extend(mute.map(|c| (c, kAudioBooleanControlPropertyValue, true)));
        let ctl = watch
            .into_iter()
            .filter_map(|(obj, sel, gain)| {
                let s = state.clone();
                Listener::with(obj, sel, kAudioObjectPropertyScopeGlobal, move || {
                    if gain { s.read_gain() } else { s.read_follow() }
                })
                .map_err(|e| log::warn!("no volume listener on '{uid}': {e:#}"))
                .ok()
            })
            .collect();
        state.read_gain();
        SysVol {
            state: Some(state),
            _controls: ctl,
            procs: Vec::new(),
        }
    }

    /// The app sources and the process objects each one taps now (empty: not running).
    pub(super) fn set_apps(&mut self, apps: Vec<(String, Vec<u32>)>) {
        let Some(state) = &self.state else {
            return;
        };
        {
            let g = state.lock();
            if g.apps.len() == apps.len()
                && g.apps
                    .iter()
                    .zip(&apps)
                    .all(|(a, (s, o))| a.source == *s && a.objs == *o)
            {
                return;
            }
        }
        // Registered before the read below, and outside the lock: a callback waits for it, and dropping a
        // listener waits for callbacks.
        let new: Vec<Listener> = apps
            .iter()
            .flat_map(|(_, objs)| objs)
            .filter_map(|&o| {
                let s = state.clone();
                Listener::with(
                    o,
                    kAudioProcessPropertyDevices,
                    kAudioObjectPropertyScopeOutput,
                    move || s.read_follow(),
                )
                .map_err(|e| log::debug!("no device listener on process {o}: {e:#}"))
                .ok()
            })
            .collect();
        state.lock().apps = apps
            .into_iter()
            .map(|(source, objs)| App {
                source,
                objs,
                follow: false,
            })
            .collect();
        state.read_follow();
        drop(std::mem::replace(&mut self.procs, new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_aggregate_main_being_v_does_not_follow_v_twice() {
        assert!(!follows_controls("sr:v1", "sr:v1"));
        assert!(follows_controls("sr:v1.companion", "sr:v1"));
    }

    #[test]
    fn an_app_follows_v_while_its_devices_list_v() {
        let (v, headphones) = (7, 9);
        assert!(plays_to_v(
            &[Some(vec![headphones, v])],
            v,
            Some(headphones)
        ));
        assert!(!plays_to_v(&[Some(vec![headphones])], v, Some(v)));
        assert!(
            !plays_to_v(&[Some(vec![])], v, Some(v)),
            "idle: lists no device"
        );
        // One helper process on V is enough; an unreadable one does not count against it.
        assert!(plays_to_v(
            &[None, Some(vec![v]), Some(vec![headphones])],
            v,
            None
        ));
        assert!(!plays_to_v(&[None, Some(vec![headphones])], v, Some(v)));
    }

    #[test]
    fn unreadable_devices_fall_back_to_the_default_output() {
        assert!(plays_to_v(&[None], 7, Some(7)));
        assert!(plays_to_v(&[None, None], 7, Some(7)));
        assert!(!plays_to_v(&[None], 7, Some(9)));
        assert!(!plays_to_v(&[None], 7, None));
        assert!(plays_to_v(&[], 7, Some(7)), "no process objects at all");
    }
}
