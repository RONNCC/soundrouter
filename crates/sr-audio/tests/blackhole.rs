//! End-to-end engine test through BlackHole 16ch: srtone -> process tap -> engine mixer -> BlackHole.
//! Silent by design: the 1 kHz tone goes only into BlackHole (channels 8-9, tapped by process), never to a
//! physical output. Run with `cargo test -p sr-audio --test blackhole -- --ignored`.
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sr_audio::loopback::Probe;
use sr_audio::{Engine, IoState};
use sr_core::backend::Backend;
use sr_core::config::*;

const AMP: f32 = 0.1;
/// The player writes BlackHole channels 8-9. The engine's routed mix goes to channels 0-1 and the probe
/// reads only those, so the player never leaks into the measurement and nothing loops back.
const PLAYER_FIRST_CH: &str = "8";

/// Build `packaging/srtone.c`: it plays into a virtual device only (refuses anything but soundrouter:* or
/// BlackHole), so this test can never make sound on speakers. Returns the binary's physical path, which is
/// also how the engine's process tap finds it (`path:<exe>`).
fn build_srtone() -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packaging/srtone.c");
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("srtone");
    let st = Command::new("clang")
        .args([
            "-O2",
            "-framework",
            "CoreAudio",
            "-framework",
            "CoreFoundation",
            "-o",
        ])
        .arg(&out)
        .arg(src)
        .status()
        .expect("clang (Xcode command line tools) is needed to build srtone");
    assert!(st.success(), "building srtone failed");
    out.canonicalize().unwrap()
}

struct Player(Child);
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cfg(player_key: &str, volume_db: f32, muted: bool, wires: &[(u16, u16)]) -> Config {
    Config {
        version: 1,
        devices: vec![VirtualDevice {
            id: "e2e".into(),
            name: "e2e".into(),
            enabled: true,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            sources: vec![Source {
                id: "s1".into(),
                kind: SourceKind::App {
                    bundle_id: player_key.into(),
                },
                channels: 2,
                volume_db,
                muted,
                mute_original: false,
                name: String::new(),
            }],
            wires: wires
                .iter()
                .map(|&(s, o)| Wire {
                    source: "s1".into(),
                    src_ch: s,
                    out_ch: o,
                })
                .collect(),
            monitors: vec![],
        }],
    }
}

/// Poll `f` until it returns Some or the deadline passes.
fn wait_for<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Peaks of BlackHole channels 0 and 1 over roughly the last 150 ms.
fn peaks(p: &Probe) -> [f32; 2] {
    p.stats.take_peaks(2);
    std::thread::sleep(Duration::from_millis(150));
    let v = p.stats.take_peaks(2);
    [v[0], v[1]]
}

fn near(got: f32, want: f32) -> bool {
    (got - want).abs() <= want.max(1e-6) * 0.01 + 1e-5
}

/// Apply `c` (twice, to prove re-apply is stable) and wait until the output peaks match `want`.
fn expect(e: &mut Engine, p: &Probe, c: &Config, what: &str, want: [f32; 2]) {
    e.apply(c).unwrap();
    e.apply(c).unwrap();
    let mut last = [0.0; 2];
    wait_for(what, 15, || {
        last = peaks(p);
        (near(last[0], want[0]) && near(last[1], want[1])).then_some(())
    });
    assert_eq!(e.mismatches("e2e"), 0, "{what}: buffer shape mismatches");
    println!("{what}: peaks {last:?} (want {want:?})");
}

#[test]
#[ignore = "needs BlackHole 16ch installed + audio capture permission"]
fn srtone_through_engine_into_blackhole() {
    // Stage lines go to stderr unbuffered, so a CI log shows where a hang (e.g. a TCC prompt) sits.
    eprintln!(
        "stage: start; TCC audio capture {:?}, microphone {:?}",
        sr_audio::tcc::audio_capture_status(),
        sr_audio::tcc::microphone_status()
    );
    let mut e = Engine::new().unwrap();
    let Some(bh) = e
        .hardware()
        .into_iter()
        .find(|d| d.name.contains("BlackHole 16ch"))
    else {
        println!("SKIP: BlackHole 16ch not installed");
        return;
    };
    // SAFETY: the only test in this binary, so no other thread reads the environment concurrently.
    unsafe { std::env::set_var("SR_MAIN_UID_OVERRIDE", &bh.uid) };

    let srtone = build_srtone();
    eprintln!("stage: srtone built; starting player on {}", bh.uid);
    let key = format!("path:{}", srtone.display());
    let c = |volume_db: f32, muted: bool, wires: &[(u16, u16)]| cfg(&key, volume_db, muted, wires);
    // Only ever BlackHole (found by name above); srtone itself also refuses any non-virtual device.
    let mut child = Command::new(&srtone)
        .args([
            &bh.uid,
            "60",
            &AMP.to_string(),
            &AMP.to_string(),
            PLAYER_FIRST_CH,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut banner = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().unwrap()),
        &mut banner,
    )
    .unwrap();
    let player = Player(child);
    assert!(
        banner.contains(&format!("({}) ch 8-9", bh.uid)),
        "srtone did not open BlackHole channels 8-9: {banner:?}"
    );

    eprintln!("stage: player up; applying the route (creates the process tap)");
    e.apply(&c(0.0, false, &[(0, 0), (1, 1)])).unwrap();
    wait_for("IoState::Running", 30, || match e.io_state("e2e") {
        Some(IoState::Failed(s)) => panic!("IO failed: {s}"),
        Some(IoState::Running) => Some(()),
        _ => None,
    });
    eprintln!("stage: engine IO running; opening the probe (BlackHole input)");
    let probe = Probe::start(&bh.uid, false).unwrap();

    // The stereo-mixdown tap scales a many-channel player device (BlackHole 16ch measured 1/8), so take the
    // unity level from the first stable non-silent reading and check every case relative to it.
    let unit = wait_for("tone through the tap", 15, || {
        let (a, b) = (peaks(&probe), peaks(&probe));
        (a[0] > 1e-3 && near(a[0], a[1]) && near(a[0], b[0])).then_some(a[0])
    });
    assert!(
        (AMP / 32.0..=AMP * 1.01).contains(&unit),
        "implausible tap gain {}",
        unit / AMP
    );
    println!("tap gain {:.4} (unit peak {unit})", unit / AMP);

    let both = [(0, 0), (1, 1)];
    expect(&mut e, &probe, &c(0.0, false, &both), "unity", [unit, unit]);
    expect(&mut e, &probe, &c(0.0, true, &both), "muted", [0.0, 0.0]);
    expect(
        &mut e,
        &probe,
        &c(-6.0, false, &both),
        "-6 dB",
        [unit * 0.501_187, unit * 0.501_187],
    );
    expect(
        &mut e,
        &probe,
        &c(0.0, false, &[(0, 1)]),
        "ch0->out1 only",
        [0.0, unit],
    );

    // Source process dies: tap goes away, output silent, engine keeps running and accepts applies.
    drop(player);
    wait_for("silence after srtone exit", 15, || {
        let p = peaks(&probe);
        (p == [0.0, 0.0]).then_some(())
    });
    e.apply(&c(0.0, false, &both)).unwrap();
    assert_eq!(e.io_state("e2e"), Some(IoState::Running));
    assert_eq!(e.mismatches("e2e"), 0);

    drop(probe);
    drop(e);
}
