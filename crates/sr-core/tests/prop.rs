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
        })
}

fn arb_device() -> impl Strategy<Value = VirtualDevice> {
    let wire =
        ("[a-z0-9]{0,4}", any::<u16>(), any::<u16>()).prop_map(|(source, src_ch, out_ch)| Wire {
            source,
            src_ch,
            out_ch,
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
            prop::collection::vec(prop::sample::select(all), 0..12).prop_map(move |ws| {
                let wires = ws
                    .into_iter()
                    .map(|(i, src_ch, out_ch)| Wire {
                        source: format!("s{i}"),
                        src_ch,
                        out_ch,
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
    fn config_json_round_trips(devices in prop::collection::vec(arb_device(), 0..3), version in any::<u32>()) {
        let cfg = Config { version, devices };
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
        let cfg = Config { version: 1, devices };
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
        for (c, got) in o.iter().enumerate() {
            for i in 0..n {
                let mut want = 0.0f32;
                for wire in w.dev.wires.iter().filter(|x| usize::from(x.out_ch) == c) {
                    let s = w.dev.sources.iter().find(|s| s.id == wire.source).unwrap();
                    let slot = w.layout_in.iter().position(|(id, ch)| *id == wire.source && *ch == wire.src_ch).unwrap();
                    let g = if s.muted { 0.0 } else { lin(s.volume_db) };
                    want += g * ins[slot][i];
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
        blocks in prop::collection::vec(prop::collection::vec((-60.0f32..=0.0, any::<bool>()), 0..4), 1..12),
    ) {
        let gain = |(db, muted): (f32, bool)| if muted { 0.0 } else { lin(db) };
        let plan = |g: (f32, bool)| {
            let mut d = dev();
            d.sources[0].volume_db = g.0;
            d.sources[0].muted = g.1;
            MixPlan::compile(&d, &layout())
        };
        let meters = Arc::new(Meters::new());
        let (mut m, mut h) = mixer(meters);
        h.set_plan(plan(start));
        let ins = vec![vec![amp; n], vec![amp; n]];
        let mut applied = gain(start);
        let mut prev = applied * amp;
        run(&mut m, &ins, 4, n);
        for plans in blocks {
            for &p in &plans {
                h.set_plan(plan(p));
            }
            let target = plans.last().map_or(applied, |&p| gain(p)); // newest plan wins
            let o = run(&mut m, &ins, 4, n);
            let bound = (target - applied).abs() * amp / n as f32 + 1e-5;
            for &x in &o[0] {
                prop_assert!((x - prev).abs() <= bound, "step {prev} -> {x} > {bound}");
                prev = x;
            }
            prop_assert!((prev - target * amp).abs() <= 1e-4, "ended at {prev}, want {}", target * amp);
            applied = target;
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
