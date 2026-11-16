//! End-to-end engine test through BlackHole 16ch: afplay -> process tap -> engine mixer -> BlackHole.
//! Plays a short, quiet 440 Hz tone through the speakers. Run with
//! `cargo test -p sr-audio --test blackhole -- --ignored`.
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sr_audio::loopback::Probe;
use sr_audio::{Engine, IoState};
use sr_core::backend::Backend;
use sr_core::config::*;

const AMP: f32 = 0.1;
const AFPLAY: &str = "path:/usr/bin/afplay";

/// 16-bit mono 48 kHz sine WAV, written by hand.
fn write_tone(path: &std::path::Path, secs: u32) {
    let rate = 48_000u32;
    let n = rate * secs;
    let mut w = Vec::with_capacity(44 + 2 * n as usize);
    w.extend(b"RIFF");
    w.extend((36 + 2 * n).to_le_bytes());
    w.extend(b"WAVEfmt ");
    w.extend(16u32.to_le_bytes());
    w.extend(1u16.to_le_bytes()); // PCM
    w.extend(1u16.to_le_bytes()); // mono
    w.extend(rate.to_le_bytes());
    w.extend((rate * 2).to_le_bytes());
    w.extend(2u16.to_le_bytes());
    w.extend(16u16.to_le_bytes());
    w.extend(b"data");
    w.extend((2 * n).to_le_bytes());
    for i in 0..n {
        let s = (f64::from(i) * 440.0 * std::f64::consts::TAU / f64::from(rate)).sin();
        w.extend(((s * f64::from(AMP) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, w).unwrap();
}

struct Player(Child);
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cfg(volume_db: f32, muted: bool, wires: &[(u16, u16)]) -> Config {
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
                    bundle_id: AFPLAY.into(),
                },
                channels: 2,
                volume_db,
                muted,
                mute_original: false,
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
fn afplay_through_engine_into_blackhole() {
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

    let wav = std::env::temp_dir().join(format!("sr-e2e-{}.wav", std::process::id()));
    write_tone(&wav, 60);
    let player = Player(
        Command::new("/usr/bin/afplay")
            .arg(&wav)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );

    e.apply(&cfg(0.0, false, &[(0, 0), (1, 1)])).unwrap();
    wait_for("IoState::Running", 30, || match e.io_state("e2e") {
        Some(IoState::Failed(s)) => panic!("IO failed: {s}"),
        Some(IoState::Running) => Some(()),
        _ => None,
    });
    let probe = Probe::start(&bh.uid, false).unwrap();

    let both = [(0, 0), (1, 1)];
    expect(&mut e, &probe, &cfg(0.0, false, &both), "unity", [AMP, AMP]);
    expect(&mut e, &probe, &cfg(0.0, true, &both), "muted", [0.0, 0.0]);
    expect(
        &mut e,
        &probe,
        &cfg(-6.0, false, &both),
        "-6 dB",
        [AMP * 0.501_187, AMP * 0.501_187],
    );
    expect(
        &mut e,
        &probe,
        &cfg(0.0, false, &[(0, 1)]),
        "ch0->out1 only",
        [0.0, AMP],
    );

    // Source process dies: tap goes away, output silent, engine keeps running and accepts applies.
    drop(player);
    wait_for("silence after afplay exit", 15, || {
        let p = peaks(&probe);
        (p == [0.0, 0.0]).then_some(())
    });
    e.apply(&cfg(0.0, false, &both)).unwrap();
    assert_eq!(e.io_state("e2e"), Some(IoState::Running));
    assert_eq!(e.mismatches("e2e"), 0);

    drop(probe);
    drop(e);
    let _ = std::fs::remove_file(wav);
}
