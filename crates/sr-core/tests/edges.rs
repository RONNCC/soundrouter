mod common;
use common::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

fn setup(d: &VirtualDevice, l: &IoLayout) -> (Mixer, MixerHandle, Arc<Meters>) {
    let meters = Arc::new(Meters::new());
    let (m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(d, l));
    (m, h, meters)
}

fn all_finite(o: &[Vec<f32>]) -> bool {
    o.iter().flatten().all(|x| x.is_finite())
}

const NASTY_DB: [f32; 8] = [
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::NAN,
    1000.0,
    -1000.0,
    f32::MAX,
    f32::MIN,
    24.0001,
];

#[test]
fn extreme_volumes_give_finite_bounded_output() {
    for db in NASTY_DB {
        for which in 0..3 {
            let mut d = dev();
            d.monitors.push(mon("spk", true, 0.0));
            match which {
                0 => d.volume_db = db,
                1 => d.sources[0].volume_db = db,
                _ => d.monitors[0].volume_db = db,
            }
            let (mut m, _h, meters) = setup(&d, &layout());
            let ins = vec![vec![0.5; 32], vec![0.5; 32]];
            let o = run(&mut m, &ins, 4, 32);
            assert!(all_finite(&o), "db {db} slot {which}");
            // clamped at +24 dB (x15.85) on a single stage
            assert!(o.iter().flatten().all(|x| x.abs() <= 0.5 * 15.9), "db {db}");
            let r = meters.take(&layout(), &d);
            assert!(r.channels.iter().all(|x| x.is_finite()), "db {db}");
            if db.is_nan() || db < -200.0 {
                let silent = if which == 2 { &o[2..] } else { &o[..] };
                assert!(
                    silent.iter().flatten().all(|x| *x == 0.0),
                    "db {db} slot {which}"
                );
            }
        }
    }
}

#[test]
fn over_limit_volume_clamps_to_plus_24_db() {
    let mut d = dev();
    d.volume_db = 1000.0;
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &[vec![0.5; 8], vec![0.5; 8]], 4, 8);
    let want = 0.5 * 10f32.powf(24.0 / 20.0);
    assert!((o[0][7] - want).abs() < 1e-4, "{} vs {want}", o[0][7]);
}

#[test]
fn max_width_and_max_block() {
    const N: usize = 64;
    let mut d = dev();
    d.channels = N as u16;
    d.sources = vec![src("s1", N as u16, 0.0, false)];
    // reverse permutation: input i -> output N-1-i
    d.wires = (0..N as u16)
        .map(|i| Wire {
            source: "s1".into(),
            src_ch: i,
            out_ch: N as u16 - 1 - i,
        })
        .collect();
    let l = IoLayout {
        inputs: (0..N as u16).map(|c| Some(("s1".into(), c))).collect(),
        outputs: (0..N as u16).map(|c| Some(OutPort::Device(c))).collect(),
    };
    for frames in [MAX_FRAMES, MAX_FRAMES + 1, 2 * MAX_FRAMES] {
        let (mut m, _h, meters) = setup(&d, &l);
        let ins: Vec<Vec<f32>> = (0..N)
            .map(|c| {
                (0..frames)
                    .map(|i| (c * 1000 + i % 1000) as f32 / 64000.0)
                    .collect()
            })
            .collect();
        let o = run(&mut m, &ins, N, frames);
        for c in 0..N {
            assert_eq!(o[N - 1 - c], ins[c], "frames {frames} ch {c}");
        }
        assert_eq!(meters.mismatch_count(), 0);
        let r = meters.take(&l, &d);
        assert!(r.channels.iter().all(|p| *p > 0.0));
    }
}

#[test]
fn non_finite_input_cannot_poison_the_device() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, 0.0));
    d.sources.push(src("bad", 1, 0.0, false));
    d.wires.push(Wire {
        source: "bad".into(),
        src_ch: 0,
        out_ch: 0,
    });
    let mut l = layout();
    l.inputs.push(Some(("bad".into(), 0)));
    let (mut m, mut h, meters) = setup(&d, &l);
    let n = 16;
    let mut bad = vec![0.0; n];
    bad[1] = f32::NAN;
    bad[2] = f32::INFINITY;
    bad[3] = f32::NEG_INFINITY;
    let ins = vec![vec![0.25; n], vec![0.5; n], bad];
    for block in 0..3 {
        let o = run(&mut m, &ins, 4, n);
        assert!(all_finite(&o), "block {block}");
        // the healthy source still passes through next to the bad one
        assert!(o[0].iter().all(|x| *x == 0.25), "{:?}", o[0]);
        assert!(o[1].iter().all(|x| *x == 0.5));
        assert!(o[2].iter().all(|x| *x == 0.25), "monitor mirrors vdev ch0");
        if block == 1 {
            // a ramping plan swap must not smear NaN either
            d.sources[1].volume_db = -6.0;
            h.set_plan(MixPlan::compile(&d, &l));
        }
    }
    let r = meters.take(&l, &d);
    assert!(
        r.channels
            .iter()
            .chain(r.sources.iter().flat_map(|(_, c)| c))
            .all(|x| x.is_finite())
    );
    assert_eq!(
        r.sources[1].1[0], 0.0,
        "non-finite samples do not register on meters"
    );
    // all-NaN input is just silence
    let ins = vec![vec![f32::NAN; n], vec![f32::INFINITY; n], vec![f32::NAN; n]];
    assert!(run(&mut m, &ins, 4, n).iter().flatten().all(|x| *x == 0.0));
}

#[test]
fn meters_take_tolerates_duplicate_ids() {
    let mut d = dev();
    d.sources.push(src("s1", 2, 0.0, false)); // duplicate id: invalid config, must not panic
    let mut l = layout();
    l.inputs.push(Some(("s1".into(), 0))); // same (source, ch) twice in the layout
    let (mut m, _h, meters) = setup(&d, &l);
    run(&mut m, &[vec![0.3; 8], vec![0.1; 8], vec![0.7; 8]], 4, 8);
    let r = meters.take(&l, &d);
    assert_eq!(r.sources.len(), 2);
    for (id, chans) in &r.sources {
        assert_eq!(id, "s1");
        assert_eq!(chans, &vec![0.7, 0.1], "max over duplicate slots");
    }
}

#[test]
fn meters_take_ignores_layout_entries_beyond_source_channels() {
    let d = dev();
    let mut l = layout();
    l.inputs.push(Some(("s1".into(), 9))); // channel the source does not have
    l.inputs.push(Some(("ghost".into(), 0)));
    let (mut m, _h, meters) = setup(&d, &l);
    run(
        &mut m,
        &[vec![0.3; 4], vec![0.1; 4], vec![0.9; 4], vec![0.9; 4]],
        4,
        4,
    );
    let r = meters.take(&l, &d);
    assert_eq!(r.sources, vec![("s1".to_string(), vec![0.3, 0.1])]);
}

fn named(name: &str) -> Config {
    let mut d = dev();
    d.name = name.into();
    Config {
        devices: vec![d],
        ..Config::default()
    }
}

#[test]
fn name_rules() {
    for bad in [
        "",
        " ",
        "\t\n",
        "\u{3000}\u{a0}",
        &"x".repeat(65),
        &"é".repeat(65),
        &"x".repeat(100_000),
    ] {
        assert!(
            named(bad).validate().is_err(),
            "accepted {:?}",
            bad.chars().take(10).collect::<String>()
        );
    }
    for good in [
        "a",
        " a ",
        "Mic + Music",
        "日本語",
        "😀",
        &"x".repeat(64),
        &"😀".repeat(64),
    ] {
        named(good)
            .validate()
            .unwrap_or_else(|e| panic!("{good:?}: {e}"));
    }
    // 64 chars, not bytes: matches the driver's chars().take(64)
    assert!("😀".repeat(64).len() > 64);
}

#[test]
fn version_rules() {
    let json = |v: &str| format!(r#"{{"version":{v},"devices":[]}}"#);
    let parse = |v: &str| serde_json::from_str::<Config>(&json(v));
    assert!(parse("1").unwrap().validate().is_ok());
    assert!(
        parse("2")
            .unwrap()
            .validate()
            .unwrap_err()
            .contains("version")
    );
    assert!(parse("0").unwrap().validate().is_err());
    assert!(parse("4294967295").unwrap().validate().is_err());
    assert!(parse("-1").is_err());
    assert!(parse("\"1\"").is_err());
    // missing version means the current one
    let c: Config = serde_json::from_str(r#"{"devices":[]}"#).unwrap();
    assert_eq!(c.version, 1);
}

#[test]
fn minimal_json_is_usable_end_to_end() {
    let c: Config = serde_json::from_str(
        r#"{"version":1,"devices":[{"id":"v","name":"Min","sources":[{"id":"s","kind":{"type":"device","uid":"u"}}],
            "wires":[{"source":"s","src_ch":0,"out_ch":0},{"source":"s","src_ch":1,"out_ch":1}]}]}"#,
    )
    .unwrap();
    c.validate().unwrap();
    let d = &c.devices[0];
    assert!(d.enabled && d.channels == 2 && d.sources[0].channels == 2);
    let l = IoLayout {
        inputs: vec![Some(("s".into(), 0)), Some(("s".into(), 1))],
        outputs: vec![Some(OutPort::Device(0)), Some(OutPort::Device(1))],
    };
    let (mut m, _h, _) = setup(d, &l);
    let o = run(&mut m, &[vec![0.5; 4], vec![0.25; 4]], 2, 4);
    assert_eq!(
        (o[0][3], o[1][3]),
        (0.5, 0.25),
        "defaults are unity gain, unmuted"
    );
    // unknown future fields are tolerated on read
    assert!(serde_json::from_str::<Config>(r#"{"version":1,"devices":[],"future":true}"#).is_ok());
    // volume bounds survive the JSON trip as errors
    let mut bad = c.clone();
    bad.devices[0].volume_db = 1000.0;
    assert!(bad.validate().is_err());
}

#[test]
fn huge_finite_samples_cannot_overflow_to_inf() {
    let mut d = dev();
    d.volume_db = 24.0;
    d.sources[0].volume_db = 24.0;
    d.monitors.push(mon("spk", true, 24.0));
    let (mut m, _h, _) = setup(&d, &layout());
    let ins = vec![vec![f32::MAX; 16], vec![f32::MIN; 16]];
    let o = run(&mut m, &ins, 4, 16);
    assert!(all_finite(&o));
}
