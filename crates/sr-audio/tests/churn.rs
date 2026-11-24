//! Live memory soak of the engine's rebuild paths against the INSTALLED driver (soundrouter:v1).
//! Silent: nothing plays. It changes the driver's device list, so run it only when no SoundRouter app
//! is running. Run with `cargo test -p sr-audio --test churn -- --ignored --nocapture --test-threads=1`.
use sr_audio::Engine;
use sr_core::backend::Backend;
use sr_core::config::*;
use std::sync::{Mutex, PoisonError};

/// Both tests rebuild the driver's device list: one at a time, even under plain `cargo test -- --ignored`.
static DRIVER: Mutex<()> = Mutex::new(());

fn src(id: &str, kind: SourceKind) -> Source {
    Source {
        id: id.into(),
        kind,
        channels: 2,
        volume_db: 0.0,
        muted: false,
        mute_original: false,
        name: String::new(),
    }
}

fn cfg(enabled: bool, volume_db: f32, with_app: bool) -> Config {
    let mut sources = vec![src("pt", SourceKind::PassThru)];
    let mut wires = vec![Wire::new("pt", 0, 0)];
    if with_app {
        // A running audio process is not needed: an App source with no matching process just waits.
        sources.push(src(
            "a",
            SourceKind::App {
                bundle_id: "com.apple.Safari".into(),
            },
        ));
        wires.push(Wire::new("a", 0, 1));
    }
    Config {
        devices: vec![VirtualDevice {
            id: "v1".into(),
            name: "Churn".into(),
            enabled,
            channels: 2,
            volume_db,
            muted: false,
            sources,
            wires,
            monitors: vec![],
        }],
        ..Config::default()
    }
}

/// Resident set size in KiB.
fn rss_kib() -> u64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// Runs `step` `n` times; the first quarter is warm-up (allocator and CoreAudio caches), growth is
/// measured over the rest.
fn grow(name: &str, n: usize, mut step: impl FnMut(usize)) -> u64 {
    let warm = n / 4;
    (0..warm).for_each(&mut step);
    let (t0, a) = (std::time::Instant::now(), rss_kib());
    (warm..n).for_each(&mut step);
    let b = rss_kib();
    println!(
        "{name}: {n} cycles, {:.0} ms/cycle, rss {a} -> {b} KiB ({:+} KiB)",
        t0.elapsed().as_secs_f64() * 1000.0 / (n - warm) as f64,
        b as i64 - a as i64
    );
    b.saturating_sub(a)
}

#[test]
#[ignore = "needs the SoundRouter driver installed; changes the driver device list"]
fn rebuild_loops_do_not_leak() {
    let _one = DRIVER.lock().unwrap_or_else(PoisonError::into_inner);
    let mut e = Engine::new().unwrap();
    e.apply(&cfg(true, 0.0, false)).unwrap();
    let mut worst = 0;
    // Same device, new gain: the in-place plan swap.
    worst = worst.max(grow("apply (volume change)", 200, |i| {
        e.apply(&cfg(true, -(i as f32 % 20.0), false)).unwrap();
    }));
    // Source added and removed: tap sync + layout refresh.
    worst = worst.max(grow("add/remove app source", 100, |i| {
        e.apply(&cfg(true, 0.0, i % 2 == 0)).unwrap();
    }));
    // Whole aggregate torn down and rebuilt, driver devices removed and re-added. Before the scratch
    // buffers left malloc this grew about 0.4 MiB per cycle (+31 MiB over 38 cycles). Every enable must
    // also find its device: apply used to return before the removed one was gone.
    worst = worst.max(grow("disable/enable (full rebuild)", 50, |i| {
        e.apply(&cfg(i % 2 == 0, 0.0, false)).unwrap();
    }));
    e.apply(&cfg(false, 0.0, false)).unwrap();
    // Allocator noise is a few MiB; a per-cycle leak of even 100 KiB would show as 5+ MiB here.
    assert!(worst < 8 * 1024, "rss grew by {worst} KiB in one loop");
}

/// Like `cfg`, plus a second device `v2` that only plays (nothing reads it), and an app source on the
/// process at `player` (a real executable path) when `with_app`.
fn tap_cfg(with_app: bool, player: &str) -> Config {
    let mut c = cfg(true, 0.0, false);
    if with_app {
        c.devices[0].sources.push(src(
            "a",
            SourceKind::App {
                bundle_id: format!("path:{player}"),
            },
        ));
        c.devices[0].wires.push(Wire::new("a", 0, 1));
    }
    let mut v2 = c.devices[0].clone();
    (v2.id, v2.name, v2.sources, v2.wires) = ("v2".into(), "Player".into(), vec![], vec![]);
    c.devices.push(v2);
    c
}

/// A real tapped process: `srtone` plays a tone into `soundrouter:v2` while the engine adds and
/// removes the app source, so the tap aggregate is built, started, swapped and destroyed each time.
#[test]
#[ignore = "needs the SoundRouter driver installed; changes the driver device list; needs clang"]
fn tap_aggregate_loops_do_not_leak() {
    let _one = DRIVER.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = std::env::temp_dir().join(format!("sr-churn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let player = dir.canonicalize().unwrap().join("srtone"); // the tap keys the process by its real path
    let src_c = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/srtone.c");
    let built = std::process::Command::new("clang")
        .args([
            "-O2",
            "-framework",
            "CoreAudio",
            "-framework",
            "CoreFoundation",
            "-o",
        ])
        .arg(&player)
        .arg(src_c)
        .status()
        .unwrap();
    assert!(built.success(), "clang failed");
    let mut e = Engine::new().unwrap();
    e.apply(&tap_cfg(false, "")).unwrap();
    let mut tone = std::process::Command::new(&player)
        .args(["soundrouter:v2", "600", "0.2", "0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let p = player.to_str().unwrap();
    let worst = grow("add/remove a real tapped app", 60, |i| {
        e.apply(&tap_cfg(i % 2 == 0, p)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150)); // the tap aggregate starts
    });
    // The last state: tapped and running, with every counter clean.
    e.apply(&tap_cfg(true, p)).unwrap();
    let t0 = std::time::Instant::now();
    while e.tap_delay("v1").is_none() {
        assert!(t0.elapsed().as_secs() < 10, "the tap aggregate never ran");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert_eq!(e.mismatches("v1"), 0);
    println!("tap counters {:?}", e.tap_counters("v1"));
    let _ = tone.kill();
    let _ = tone.wait();
    e.apply(&cfg(false, 0.0, false)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(worst < 8 * 1024, "rss grew by {worst} KiB in one loop");
}
