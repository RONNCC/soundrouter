//! Hardware stress for vdev teardown (F3): build a vdev and drop it right away, 50 times, through BlackHole.
//! The IOProc starter thread may still be inside `AudioDeviceStart` when the vdev is retired.
//! Run with `cargo test -p sr-audio --test vdev_drop -- --ignored`.
use sr_audio::Engine;
use sr_core::backend::Backend;
use sr_core::config::*;

fn cfg(enabled: bool) -> Config {
    Config {
        devices: vec![VirtualDevice {
            id: "drop".into(),
            name: "drop".into(),
            enabled,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            sources: vec![Source {
                id: "pt".into(),
                kind: SourceKind::PassThru,
                channels: 2,
                volume_db: 0.0,
                muted: false,
                mute_original: false,
                name: String::new(),
            }],
            wires: vec![],
            monitors: vec![],
        }],
        ..Config::default()
    }
}

#[test]
#[ignore = "needs BlackHole 16ch installed"]
fn build_and_drop_immediately_50x() {
    let probe = Engine::new().unwrap();
    let Some(bh) = probe
        .hardware()
        .into_iter()
        .find(|d| d.name.contains("BlackHole 16ch"))
    else {
        println!("SKIP: BlackHole 16ch not installed");
        return;
    };
    drop(probe);
    // SAFETY: the only test in this binary, so no other thread reads the environment concurrently.
    unsafe { std::env::set_var("SR_MAIN_UID_OVERRIDE", &bh.uid) };

    // Disable via apply (retire path), then drop the whole engine while a vdev may still be starting.
    let mut e = Engine::new().unwrap();
    for i in 0..50 {
        e.apply(&cfg(true)).unwrap();
        if i % 2 == 0 {
            e.apply(&cfg(false)).unwrap();
        } else {
            drop(e);
            e = Engine::new().unwrap();
        }
    }
    e.apply(&cfg(true)).unwrap();
    drop(e);
}
