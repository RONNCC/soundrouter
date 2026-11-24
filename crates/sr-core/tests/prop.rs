mod common;
use common::*;
use proptest::prelude::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

fn db() -> impl Strategy<Value = f32> {
    prop_oneof![-200.0f32..=24.0, Just(0.0)]
}

fn arb_source() -> impl Strategy<Value = Source> {
    (
        "[a-z0-9 é]{0,6}",
        prop_oneof![
            "[a-z.]{0,12}".prop_map(|bundle_id| SourceKind::App { bundle_id }),
            "\\PC{0,12}".prop_map(|uid| SourceKind::Device { uid }),
            Just(SourceKind::PassThru),
        ],
        any::<u16>(),
        db(),
        any::<bool>(),
    )
        .prop_map(|(id, kind, channels, volume_db, muted)| Source {
            id,
            kind,
            channels,
            volume_db,
            muted,
            mute_original: false,
            name: String::new(),
        })
}

fn arb_device() -> impl Strategy<Value = VirtualDevice> {
    let wire = (
        "[a-z0-9]{0,4}",
        any::<u16>(),
        any::<u16>(),
        db(),
        any::<bool>(),
    )
        .prop_map(|(source, src_ch, out_ch, gain_db, invert)| Wire {
            gain_db,
            invert,
            ..Wire::new(source, src_ch, out_ch)
        });
    let mon = (
        "\\PC{0,10}",
        any::<bool>(),
        db(),
        prop::collection::vec(
            (any::<u16>(), any::<u16>())
                .prop_map(|(out_ch, dst_ch)| MonitorWire { out_ch, dst_ch }),
            0..4,
        ),
    )
        .prop_map(|(device_uid, enabled, volume_db, wires)| Monitor {
            device_uid,
            enabled,
            volume_db,
            wires,
            name: String::new(),
        });
    (
        "[a-z0-9]{0,4}",
        "\\PC{0,20}",
        any::<bool>(),
        any::<u16>(),
        db(),
        any::<bool>(),
        prop::collection::vec(arb_source(), 0..4),
        prop::collection::vec(wire, 0..6),
        prop::collection::vec(mon, 0..3),
    )
        .prop_map(
            |(id, name, enabled, channels, volume_db, muted, sources, wires, monitors)| {
                VirtualDevice {
                    id,
                    name,
                    enabled,
                    channels,
                    volume_db,
                    muted,
                    sources,
                    wires,
                    monitors,
                }
            },
        )
}

/// A routable random world: device, layout (one input slot per source channel), and wires.
#[derive(Debug, Clone)]
struct World {
    dev: VirtualDevice,
    layout_in: Vec<(String, u16)>,
    nch: usize,
}
impl World {
    fn layout(&self) -> IoLayout {
        IoLayout {
            inputs: self.layout_in.iter().cloned().map(Some).collect(),
            outputs: (0..self.nch as u16)
                .map(|c| Some(OutPort::Device(c)))
                .collect(),
        }
    }
}

fn arb_world() -> impl Strategy<Value = World> {
    (
        1usize..=4,
        prop::collection::vec((1u16..=4, -60.0f32..=6.0, any::<bool>()), 1..4),
        -30.0f32..=6.0,
        any::<bool>(),
    )
        .prop_flat_map(|(half, srcs, dev_db, dev_muted)| {
            let nch = half * 2;
            let sources: Vec<Source> = srcs
                .iter()
                .enumerate()
                .map(|(i, &(ch, db, muted))| src(&format!("s{i}"), ch, db, muted))
                .collect();
            let all: Vec<(usize, u16, u16)> = sources
                .iter()
                .enumerate()
                .flat_map(|(i, s)| {
                    (0..s.channels).flat_map(move |c| (0..nch as u16).map(move |o| (i, c, o)))
                })
                .collect();
            let trim = (prop_oneof![Just(0.0f32), -40.0f32..=12.0], any::<bool>());
            prop::collection::vec((prop::sample::select(all), trim), 0..12).prop_map(move |ws| {
                let wires = ws
                    .into_iter()
                    .map(|((i, src_ch, out_ch), (gain_db, invert))| Wire {
                        gain_db,
                        invert,
                        ..Wire::new(format!("s{i}"), src_ch, out_ch)
                    })
                    .collect();
                let layout_in = sources
                    .iter()
                    .flat_map(|s| (0..s.channels).map(|c| (s.id.clone(), c)))
                    .collect();
                World {
                    dev: VirtualDevice {
                        channels: nch as u16,
                        volume_db: dev_db,
                        muted: dev_muted,
                        sources: sources.clone(),
                        wires,
                        ..dev()
                    },
                    layout_in,
                    nch,
                }
            })
        })
}

fn samples(n: usize, slots: usize) -> impl Strategy<Value = Vec<Vec<f32>>> {
    prop::collection::vec(prop::collection::vec(-1.0f32..=1.0, n), slots)
}

fn lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn mixer_for(w: &World) -> (Mixer, MixerHandle, Arc<Meters>) {
    let meters = Arc::new(Meters::new());
    let (m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(&w.dev, &w.layout()));
    (m, h, meters)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn config_json_round_trips(devices in prop::collection::vec(arb_device(), 0..3), buffer_frames in any::<u32>()) {
        // The writer picks the version from the content, so that is what comes back.
        let mut cfg = Config { version: 1, devices, buffer_frames };
        cfg.version = cfg.written_version();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&cfg, &back);
        prop_assert_eq!(cfg.validate(), back.validate());
        // pretty form (what the app writes to disk) parses to the same value
        let pretty: Config = serde_json::from_str(&serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
        prop_assert_eq!(cfg, pretty);
    }

    /// Any validate-accepted config is within the limits the mixer and driver rely on.
    #[test]
    fn validated_configs_respect_limits(devices in prop::collection::vec(arb_device(), 0..3)) {
        // Repair the structural fields so validate mostly accepts; names/volumes stay random.
        let devices = devices.into_iter().enumerate().map(|(i, mut d)| {
            d.id = format!("v{i}");
            d.channels = d.channels % 33 * 2 + 2 * u16::from(d.channels % 33 == 0);
            d.wires.clear();
            d.monitors.iter_mut().for_each(|m| m.wires.clear());
            for (j, s) in d.sources.iter_mut().enumerate() {
                s.id = format!("s{j}");
                s.channels = match s.kind {
                    SourceKind::PassThru => d.channels,
                    _ => 1 + s.channels % 64,
                };
            }
            d
        }).collect();
        let cfg = Config { devices, ..Config::default() };
        if cfg.validate().is_ok() {
            for d in &cfg.devices {
                prop_assert!(!d.name.trim().is_empty() && d.name.chars().count() <= MAX_NAME_CHARS);
                prop_assert!(d.volume_db.is_finite() && d.volume_db <= MAX_DB);
                prop_assert!(d.channels % 2 == 0 && (2..=64).contains(&d.channels));
                for s in &d.sources {
                    prop_assert!(s.volume_db.is_finite() && s.volume_db <= MAX_DB);
                    if matches!(s.kind, SourceKind::App { .. }) {
                        prop_assert_eq!(s.channels, 2);
                    }
                }
            }
        }
    }

    #[test]
    fn unity_gain_any_block_size(
        sizes in prop::collection::vec(1usize..10000, 1..4),
        seed in any::<u64>(),
    ) {
        let (mut m, _h, _) = {
            let meters = Arc::new(Meters::new());
            let (m, mut h) = mixer(meters.clone());
            h.set_plan(MixPlan::compile(&dev(), &layout()));
            (m, h, meters)
        };
        for n in sizes {
            let ins: Vec<Vec<f32>> = (0..2u64)
                .map(|c| (0..n as u64).map(|i| ((i.wrapping_mul(2654435761).wrapping_add(seed ^ c)) % 2001) as f32 / 1000.0 - 1.0).collect())
                .collect();
            let o = run(&mut m, &ins, 4, n);
            prop_assert_eq!(&o[0], &ins[0]);
            prop_assert_eq!(&o[1], &ins[1]);
            prop_assert!(o[2].iter().chain(&o[3]).all(|x| *x == 0.0));
        }
    }

    #[test]
    fn output_is_sum_of_gain_times_input(w in arb_world(), n in 1usize..300, data in samples(300, 16)) {
        let (mut m, _h, _) = mixer_for(&w);
        let ins: Vec<Vec<f32>> = data[..w.layout_in.len()].iter().map(|v| v[..n].to_vec()).collect();
        let o = run(&mut m, &ins, w.nch, n);
        let dev_gain = if w.dev.muted { 0.0 } else { lin(w.dev.volume_db) };
        // a wire listed twice is one wire, the first one (its trim included)
        let mut wires: Vec<&Wire> = Vec::new();
        for wire in &w.dev.wires {
            if !wires.iter().any(|x| (&x.source, x.src_ch, x.out_ch) == (&wire.source, wire.src_ch, wire.out_ch)) {
                wires.push(wire);
            }
        }
        for (c, got) in o.iter().enumerate() {
            for i in 0..n {
                let mut want = 0.0f32;
                for wire in wires.iter().filter(|x| usize::from(x.out_ch) == c) {
                    let s = w.dev.sources.iter().find(|s| s.id == wire.source).unwrap();
                    let slot = w.layout_in.iter().position(|(id, ch)| *id == wire.source && *ch == wire.src_ch).unwrap();
                    let g = if s.muted { 0.0 } else { lin(s.volume_db) };
                    let trim = if wire.invert { -lin(wire.gain_db) } else { lin(wire.gain_db) };
                    want += g * trim * ins[slot][i];
                }
                want *= dev_gain;
                prop_assert!((got[i] - want).abs() <= 1e-4 * (1.0 + want.abs()), "ch {c} i {i}: {} vs {want}", got[i]);
            }
        }
    }

    #[test]
    fn muted_is_exact_zero(w in arb_world(), n in 1usize..200, data in samples(200, 16), which in 0u8..2) {
        let mut w = w;
        if which == 0 {
            w.dev.muted = true;
        } else {
            w.dev.sources.iter_mut().for_each(|s| s.muted = true);
        }
        let (mut m, _h, _) = mixer_for(&w);
        let ins: Vec<Vec<f32>> = data[..w.layout_in.len()].iter().map(|v| v[..n].to_vec()).collect();
        let o = run(&mut m, &ins, w.nch, n);
        prop_assert!(o.iter().flatten().all(|x| *x == 0.0));
    }

    /// Constant input: output is gain*A, so consecutive samples may differ only by the ramp step,
    /// across every block boundary and however many plans were published between blocks.
    #[test]
    fn ramp_never_steps(
        n in 1usize..600,
        amp in 0.1f32..=1.0,
        start in (-60.0f32..=0.0, any::<bool>()),
        device_stage in any::<bool>(),
        blocks in prop::collection::vec(prop::collection::vec((-60.0f32..=0.0, any::<bool>()), 0..4), 1..12),
    ) {
        let gain = |(db, muted): (f32, bool)| if muted { 0.0 } else { lin(db) };
        let plan = |g: (f32, bool)| {
            let mut d = dev();
            // the same unity-elsewhere ramp through the source stage or the device stage
            if device_stage {
                (d.volume_db, d.muted) = g;
            } else {
                (d.sources[0].volume_db, d.sources[0].muted) = g;
            }
            MixPlan::compile(&d, &layout())
        };
        let meters = Arc::new(Meters::new());
        let (mut m, mut h) = mixer(meters);
        h.set_plan(plan(start));
        let ins = vec![vec![amp; n], vec![amp; n]];
        let mut applied = gain(start);
        let mut target = applied; // the newest plan's gain; the applied one keeps moving toward it
        let mut prev = applied * amp;
        // The oracle of how far a gain gets in one block: at most 1/480 per frame (10 ms at 48 kHz).
        let slew = |cur: f32, target: f32| {
            let reach = n as f32 / 480.0;
            if (target - cur).abs() <= reach { target } else { cur + reach.copysign(target - cur) }
        };
        run(&mut m, &ins, 4, n);
        for plans in blocks {
            for &p in &plans {
                h.set_plan(plan(p));
            }
            if let Some(&p) = plans.last() {
                target = gain(p); // newest plan wins
            }
            let next = slew(applied, target);
            let o = run(&mut m, &ins, 4, n);
            let bound = (next - applied).abs() * amp / n as f32 + 1e-5;
            for &x in &o[0] {
                prop_assert!((x - prev).abs() <= bound, "step {prev} -> {x} > {bound}");
                prev = x;
            }
            prop_assert!((prev - next * amp).abs() <= 1e-4, "ended at {prev}, want {}", next * amp);
            applied = next;
        }
    }

    #[test]
    fn meters_cover_true_peak_and_reset(w in arb_world(), n in 1usize..200, data in samples(200, 16), reps in 1usize..4) {
        let (mut m, _h, meters) = mixer_for(&w);
        let l = w.layout();
        let ins: Vec<Vec<f32>> = data[..w.layout_in.len()].iter().map(|v| v[..n].to_vec()).collect();
        let mut out_peak = vec![0.0f32; w.nch];
        for r in 0..reps {
            // scale per repetition so the maximum is not always in the last block
            let scaled: Vec<Vec<f32>> = ins.iter().map(|v| v.iter().map(|x| x * (1.0 - r as f32 * 0.25)).collect()).collect();
            let o = run(&mut m, &scaled, w.nch, n);
            for (p, ch) in out_peak.iter_mut().zip(&o) {
                *p = ch.iter().fold(*p, |a, x| a.max(x.abs()));
            }
            // input meters are checked against the true input peak below
        }
        let r = meters.take(&l, &w.dev);
        for (s, chans) in &r.sources {
            let slot_of = |c: usize| w.layout_in.iter().position(|(id, ch)| id == s && usize::from(*ch) == c).unwrap();
            for (c, got) in chans.iter().enumerate() {
                let want = ins[slot_of(c)].iter().fold(0.0f32, |a, x| a.max(x.abs()));
                prop_assert!(*got >= want, "source {s} ch {c}: meter {got} < peak {want}");
                prop_assert!(*got <= want + 1e-6, "source {s} ch {c}: meter {got} inflated vs {want}");
            }
        }
        for (c, got) in r.channels.iter().enumerate() {
            prop_assert!(*got >= out_peak[c], "out ch {c}: meter {got} < peak {}", out_peak[c]);
            prop_assert!(*got <= out_peak[c] + 1e-6);
        }
        let r = meters.take(&l, &w.dev);
        prop_assert!(r.sources.iter().flat_map(|(_, c)| c).chain(&r.channels).all(|x| *x == 0.0));
    }
}

// ---- hostile input -------------------------------------------------------------------------------------

/// Any f32 bit pattern: NaN payloads, infinities, subnormals, f32::MAX.
fn any_f32() -> impl Strategy<Value = f32> {
    any::<u32>().prop_map(f32::from_bits)
}

/// Audio samples as a hostile source could deliver them: mostly sane, sometimes anything at all.
fn nasty_sample() -> impl Strategy<Value = f32> {
    prop_oneof![8 => -1.0f32..=1.0, 1 => any_f32(), 1 => Just(f32::NAN), 1 => Just(f32::INFINITY), 1 => Just(f32::MIN)]
}

/// Replace node number `*k` (preorder) of `v` with `new`; counts down `*k` as it walks.
fn replace_node(v: &mut serde_json::Value, k: &mut usize, new: &serde_json::Value) {
    if *k == 0 {
        *v = new.clone();
        *k = usize::MAX;
        return;
    }
    *k -= 1;
    let kids: Vec<&mut serde_json::Value> = match v {
        serde_json::Value::Array(a) => a.iter_mut().collect(),
        serde_json::Value::Object(o) => o.values_mut().collect(),
        _ => return,
    };
    for kid in kids {
        replace_node(kid, k, new);
        if *k == usize::MAX {
            return;
        }
    }
}

fn hostile_leaf() -> impl Strategy<Value = serde_json::Value> {
    use serde_json::Value;
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
        prop::sample::select(vec![-1i64, 0, 1, 2, 3, 63, 64, 65, 255, 65535, 65536])
            .prop_map(Value::from),
        prop::sample::select(vec![1e39f64, -1e39, 1e308, 24.000001, f64::MIN_POSITIVE])
            .prop_map(Value::from),
        ".{0,12}".prop_map(Value::from),
        prop::sample::select(vec!["app", "device", "pass_thru", "", "v1", "s1"])
            .prop_map(Value::from),
        Just(Value::Array(vec![])),
        Just(Value::Object(Default::default())),
    ]
}

/// Mixes one block of a validate-accepted device through its own natural layout.
fn mix_once(d: &VirtualDevice, ins: &[f32]) -> Vec<Vec<f32>> {
    let mut inputs = vec![];
    for s in &d.sources {
        for c in 0..s.channels {
            inputs.push(Some((s.id.clone(), c)));
        }
    }
    let mut outputs: Vec<_> = (0..d.channels).map(|c| Some(OutPort::Device(c))).collect();
    for m in &d.monitors {
        for ch in 0..4 {
            outputs.push(Some(OutPort::Monitor {
                uid: m.device_uid.clone(),
                ch,
            }));
        }
    }
    let layout = IoLayout { inputs, outputs };
    let meters = Arc::new(Meters::new());
    let (mut m, mut h) = mixer(meters);
    h.set_plan(MixPlan::compile(d, &layout));
    let data: Vec<Vec<f32>> = (0..layout.inputs.len())
        .map(|i| ins.iter().cycle().skip(i).take(32).copied().collect())
        .collect();
    run(&mut m, &data, layout.outputs.len(), 32)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Whatever bytes land in config.json, parsing and validating returns Ok or Err, never panics.
    #[test]
    fn arbitrary_bytes_never_panic_the_config_parser(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        if let Ok(cfg) = serde_json::from_slice::<Config>(&bytes) {
            let _ = cfg.validate();
        }
    }

    /// A valid config with bytes dropped, flipped or repeated (a torn or bit-rotted file) is Ok or Err, never a panic.
    #[test]
    fn torn_config_files_never_panic(
        devices in prop::collection::vec(arb_device(), 0..3),
        cut in any::<prop::sample::Index>(),
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
    ) {
        let mut json = serde_json::to_vec(&Config { devices, ..Config::default() }).unwrap();
        for (at, b) in flips {
            let i = at.index(json.len());
            json[i] = b;
        }
        json.truncate(cut.index(json.len() + 1));
        if let Ok(cfg) = serde_json::from_slice::<Config>(&json) {
            let _ = cfg.validate();
        }
    }

    /// Type- and range-hostile edits of a valid document: whatever validate accepts, the mixer runs on
    /// without a panic and with finite output, even for NaN/inf/huge samples.
    #[test]
    fn mutated_documents_either_fail_cleanly_or_mix_finitely(
        w in arb_world(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), hostile_leaf()), 0..4),
        ins in prop::collection::vec(nasty_sample(), 1..40),
    ) {
        // A valid base (arb_world is routable), so many edits still validate and reach the mixer.
        let mut d = w.dev;
        d.monitors.push(mon("spk", true, 0.0));
        let mut doc = serde_json::to_value(Config { devices: vec![d], ..Config::default() }).unwrap();
        for (at, leaf) in &edits {
            let mut n = 0;
            fn count(v: &serde_json::Value, n: &mut usize) {
                *n += 1;
                match v {
                    serde_json::Value::Array(a) => a.iter().for_each(|x| count(x, n)),
                    serde_json::Value::Object(o) => o.values().for_each(|x| count(x, n)),
                    _ => {}
                }
            }
            count(&doc, &mut n);
            replace_node(&mut doc, &mut at.index(n), leaf);
        }
        if let Ok(cfg) = serde_json::from_value::<Config>(doc)
            && cfg.validate().is_ok()
        {
            for d in &cfg.devices {
                for out in mix_once(d, &ins) {
                    prop_assert!(out.iter().all(|x| x.is_finite()), "non-finite output from a validated device");
                }
            }
        }
    }

    /// Any gain bit pattern on any stage, any samples (NaN, inf, f32::MAX), a plan swap mid-stream:
    /// the output and the meters stay finite.
    #[test]
    fn any_gain_and_any_samples_give_finite_output(
        g in prop::collection::vec(any_f32(), 6),
        flags in prop::collection::vec(any::<bool>(), 3),
        a in prop::collection::vec(nasty_sample(), 48),
        b in prop::collection::vec(nasty_sample(), 48),
    ) {
        let build = |g: &[f32], muted: bool, off: bool| {
            let mut d = dev();
            d.volume_db = g[0];
            d.sources[0].volume_db = g[1];
            d.muted = muted;
            d.monitors.push(mon("spk", !off, g[2]));
            d
        };
        let meters = Arc::new(Meters::new());
        let (mut m, mut h) = mixer(meters.clone());
        let l = layout();
        h.set_plan(MixPlan::compile(&build(&g[..3], flags[0], flags[1]), &l));
        let blk = |v: &[f32]| vec![v[..24].to_vec(), v[24..].to_vec()];
        let o = run(&mut m, &blk(&a), 4, 24);
        prop_assert!(o.iter().flatten().all(|x| x.is_finite()), "block 1");
        h.set_plan(MixPlan::compile(&build(&g[3..], flags[2], false), &l));
        let o = run(&mut m, &blk(&b), 4, 24);
        prop_assert!(o.iter().flatten().all(|x| x.is_finite()), "block 2 after a plan swap");
        let r = meters.take(&l, &build(&g[3..], false, false));
        prop_assert!(r.channels.iter().chain(r.sources.iter().flat_map(|(_, c)| c)).all(|x| x.is_finite()));
    }
}

/// Speaker output ch 0 (slot 2) of `dev()` with its monitor wired as `pairs`, for L and R input.
fn speaker_ch0(pairs: &[(u16, u16)], vol_db: f32, l: &[f32], r: &[f32]) -> Vec<f32> {
    let mut d = dev();
    let mut m = mon("spk", true, vol_db);
    m.wires = pairs
        .iter()
        .map(|&(out_ch, dst_ch)| MonitorWire { out_ch, dst_ch })
        .collect();
    d.monitors.push(m);
    let (mut mx, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&d, &layout()));
    run(&mut mx, &[l.to_vec(), r.to_vec()], 4, l.len()).swap_remove(2)
}

proptest! {
    /// A mono monitor output (both vdev channels wired to dst 0) carries the stereo output's average.
    #[test]
    fn mono_monitor_is_the_average_of_the_stereo_monitor(
        db in -24.0f32..=6.0,
        lr in prop::collection::vec((-1.0f32..=1.0, -1.0f32..=1.0), 1..48),
    ) {
        let (l, r): (Vec<f32>, Vec<f32>) = lr.into_iter().unzip();
        let mono = speaker_ch0(&[(0, 0), (1, 0)], db, &l, &r);
        let left = speaker_ch0(&[(0, 0)], db, &l, &r);
        let right = speaker_ch0(&[(1, 0)], db, &l, &r);
        for k in 0..l.len() {
            prop_assert!((mono[k] - (left[k] + right[k]) / 2.0).abs() <= 1e-6, "frame {k}");
        }
    }

    /// A mono source duplicated to L and R reaches a mono monitor at the level it reaches a stereo one.
    #[test]
    fn duplicated_mono_has_the_same_level_on_a_mono_monitor(
        db in -24.0f32..=6.0,
        x in prop::collection::vec(-1.0f32..=1.0, 1..48),
    ) {
        let mono = speaker_ch0(&[(0, 0), (1, 0)], db, &x, &x);
        let stereo = speaker_ch0(&[(0, 0), (1, 1)], db, &x, &x);
        for k in 0..x.len() {
            prop_assert!((mono[k] - stereo[k]).abs() <= 1e-6, "frame {k}");
        }
    }
}
