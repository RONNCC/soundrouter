//! Live memory soak of the engine's rebuild paths against the INSTALLED driver (soundrouter:v1).
//! Silent: nothing plays. It changes the driver's device list, so run it only when no SoundRouter app
//! is running. Run with `cargo test -p sr-audio --test churn -- --ignored --nocapture`.
use sr_audio::Engine;
use sr_core::backend::Backend;
use sr_core::config::*;

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
    let mut wires = vec![Wire {
        source: "pt".into(),
        src_ch: 0,
        out_ch: 0,
    }];
    if with_app {
        // A running audio process is not needed: an App source with no matching process just waits.
        sources.push(src(
            "a",
            SourceKind::App {
                bundle_id: "com.apple.Safari".into(),
            },
        ));
        wires.push(Wire {
            source: "a".into(),
            src_ch: 0,
            out_ch: 1,
        });
    }
    Config {
        version: 1,
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
    // Whole aggregate torn down and rebuilt.
    worst = worst.max(grow("disable/enable (full rebuild)", 50, |i| {
        e.apply(&cfg(i % 2 == 0, 0.0, false)).unwrap();
    }));
    e.apply(&cfg(false, 0.0, false)).unwrap();
    // Allocator noise is a few MiB; a per-cycle leak of even 100 KiB would show as 5+ MiB here.
    assert!(worst < 8 * 1024, "rss grew by {worst} KiB in one loop");
}
