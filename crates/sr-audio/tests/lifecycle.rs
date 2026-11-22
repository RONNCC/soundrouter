//! Live config lifecycle against the INSTALLED driver: devices added, renamed, re-channeled, disabled,
//! re-enabled and removed while the engine runs, plus a rapid edit burst. Silent: nothing plays. It changes
//! the driver's device list (left empty at the end), so run it only when no SoundRouter app is running.
//! Run with `cargo test -p sr-audio --test lifecycle -- --ignored --nocapture`.
use std::time::{Duration, Instant};

use sr_audio::{Engine, IoState, driver, loopback};
use sr_core::backend::Backend;
use sr_core::config::*;
use sr_core::driver_proto::{DEVICE_UID_PREFIX, DriverDevice};

fn dev(id: &str, name: &str, channels: u16, enabled: bool) -> VirtualDevice {
    VirtualDevice {
        id: id.into(),
        name: name.into(),
        enabled,
        channels,
        volume_db: 0.0,
        muted: false,
        sources: vec![Source {
            id: "pt".into(),
            kind: SourceKind::PassThru,
            channels,
            volume_db: 0.0,
            muted: false,
            mute_original: false,
            name: String::new(),
        }],
        wires: vec![],
        monitors: vec![],
    }
}

fn cfg(devices: Vec<VirtualDevice>, buffer_frames: u32) -> Config {
    Config {
        devices,
        buffer_frames,
        ..Config::default()
    }
}

/// The engine runs exactly the enabled devices of `c`, on their companions, with the driver in step.
fn check(e: &Engine, c: &Config, what: &str) {
    let want: Vec<DriverDevice> = c
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| DriverDevice {
            uid: format!("{DEVICE_UID_PREFIX}{}", d.id),
            name: d.name.clone(),
            channels: d.channels,
        })
        .collect();
    let plugin = driver::require_plugin().unwrap();
    assert_eq!(
        driver::get_devices(plugin).unwrap(),
        want,
        "{what}: driver list"
    );
    let t0 = Instant::now();
    for d in &c.devices {
        if !d.enabled {
            assert_eq!(e.io_state(&d.id), None, "{what}: '{}' still built", d.id);
            continue;
        }
        // Running on its companion: a vdev built on the visible fallback (companion published late)
        // must move there once it appears.
        let uid = format!("{DEVICE_UID_PREFIX}{}", d.id);
        loop {
            let s = e.io_state(&d.id);
            if s == Some(IoState::Running) && loopback::engine_holds_companion(&uid) {
                break;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(15),
                "{what}: '{}' {s:?}, on companion: {}",
                d.id,
                loopback::engine_holds_companion(&uid)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    for d in c.devices.iter().filter(|d| d.enabled) {
        assert_eq!(
            e.mismatches(&d.id),
            0,
            "{what}: '{}' shape mismatches",
            d.id
        );
    }
    let st = e.status();
    assert_eq!(st.background_error, None, "{what}");
    let ids: Vec<&str> = st.per_device.iter().map(|(id, _)| id.as_str()).collect();
    let mut on: Vec<&str> = c
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| d.id.as_str())
        .collect();
    on.sort();
    assert_eq!(ids, on, "{what}: status devices");
    println!("{what}: ok after {:?}", t0.elapsed());
}

#[test]
#[ignore = "needs the SoundRouter driver installed; changes the driver device list"]
fn add_rename_rechannel_disable_remove_while_running() {
    let mut e = Engine::new().unwrap();
    let steps = [
        ("add a", cfg(vec![dev("la", "A", 2, true)], 256)),
        ("rename a", cfg(vec![dev("la", "A renamed", 2, true)], 256)),
        (
            "re-channel a 2 -> 4",
            cfg(vec![dev("la", "A renamed", 4, true)], 256),
        ),
        (
            "add b",
            cfg(
                vec![dev("la", "A renamed", 4, true), dev("lb", "B", 2, true)],
                256,
            ),
        ),
        (
            "disable a",
            cfg(
                vec![dev("la", "A renamed", 4, false), dev("lb", "B", 2, true)],
                256,
            ),
        ),
        (
            "enable a",
            cfg(
                vec![dev("la", "A renamed", 4, true), dev("lb", "B", 2, true)],
                256,
            ),
        ),
        (
            "buffer 256 -> 128",
            cfg(
                vec![dev("la", "A renamed", 4, true), dev("lb", "B", 2, true)],
                128,
            ),
        ),
        ("remove b", cfg(vec![dev("la", "A renamed", 4, true)], 128)),
    ];
    for (what, c) in &steps {
        e.apply(c).unwrap();
        check(&e, c, what);
    }
    // A burst of edits, as a user dragging through channel counts and toggling a device: only the last
    // one counts, whatever the driver was still doing for the earlier ones.
    let mut last = cfg(vec![], 128);
    for i in 0..8u16 {
        last = cfg(
            vec![
                dev("la", "A", 2 + 2 * (i % 3), true),
                dev("lb", "B", 2, i % 2 == 0),
            ],
            128,
        );
        e.apply(&last).unwrap();
    }
    check(&e, &last, "burst");
    let empty = cfg(vec![], 256);
    e.apply(&empty).unwrap();
    check(&e, &empty, "remove all");
}

/// Devices enabled together while the driver publishes their companions one by one: one whose main was
/// the visible fallback at its build must still move to its companion (the device event that announces
/// the companion must not compare equal to a baseline taken after the vdev decided).
#[test]
#[ignore = "needs the SoundRouter driver installed; changes the driver device list"]
fn devices_enabled_together_all_reach_their_companions() {
    let mut e = Engine::new().unwrap();
    let on = cfg(
        vec![
            dev("la", "A", 2, true),
            dev("lb", "B", 2, true),
            dev("lc", "C", 2, true),
        ],
        256,
    );
    let off = cfg(vec![], 256);
    for round in 0..4 {
        e.apply(&on).unwrap();
        check(&e, &on, &format!("round {round}: all on"));
        e.apply(&off).unwrap();
        check(&e, &off, &format!("round {round}: all off"));
        // coreaudiod needs a moment to drain the removals before it publishes the next three.
        std::thread::sleep(Duration::from_secs(2));
    }
}
