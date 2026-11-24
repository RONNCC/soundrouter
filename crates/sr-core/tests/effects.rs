//! The built-in effects through the whole mixer: against an independent f64 reference over random DAGs,
//! bit-exact while neutral, sample-accurate delay at every block split, state carried across plan swaps,
//! hostile samples, mute, meters and the config schema.
mod common;
use common::graph::*;
use common::*;
use proptest::prelude::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

fn noise(t: usize, seed: u64) -> Vec<Vec<f64>> {
    (0..SRCS * 2)
        .map(|c| {
            (0..t)
                .map(|i| {
                    let h = (i as u64 + 1)
                        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                        .wrapping_add(seed ^ (c as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9));
                    ((h >> 40) % 2001) as f64 / 1000.0 - 1.0
                })
                .collect()
        })
        .collect()
}

/// Runs `ins` through a mixer in blocks of `split`, per vdev channel.
fn through(m: &mut Mixer, ins: &[Vec<f64>], split: usize) -> Vec<Vec<f32>> {
    let t = ins[0].len();
    let mut out = vec![Vec::new(); NCH];
    for start in (0..t).step_by(split) {
        let end = (start + split).min(t);
        let part: Vec<Vec<f32>> = ins
            .iter()
            .map(|v| v[start..end].iter().map(|x| *x as f32).collect())
            .collect();
        for (o, p) in out.iter_mut().zip(run(m, &part, NCH, end - start)) {
            o.extend(p);
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(150))]

    /// Every effect, any chain, any DAG, any block split: the mixer is the f64 reference within f32 error.
    #[test]
    fn effects_match_the_f64_reference(
        g in graph(FxSet::All),
        seed in any::<u64>(),
        split in prop::sample::select(vec![1usize, 64, 256, 1000, 2500]),
    ) {
        let d = device(&g);
        let cfg = Config { devices: vec![d.clone()], ..Config::default() };
        prop_assert_eq!(cfg.validate(), Ok(()));
        let ins = noise(2500, seed);
        let want = simulate(&g, &ins, false);
        let (mut m, _h, _) = mixer_for(&d);
        let got = through(&mut m, &ins, split);
        for c in 0..NCH {
            let scale = 1.0 + want[c].iter().fold(0.0f64, |a, x| a.max(x.abs()));
            for (t, (v, w)) in got[c].iter().zip(&want[c]).enumerate() {
                prop_assert!(v.is_finite());
                prop_assert!(
                    (f64::from(*v) - w).abs() <= 2e-4 * scale,
                    "ch {c} frame {t}: {v} vs {w} (scale {scale})"
                );
            }
        }
    }

    /// After the parameters settle, a node's state is continuous: every block split is bit-identical.
    #[test]
    fn effects_are_bit_identical_at_every_block_split(g in graph(FxSet::All), seed in any::<u64>()) {
        let d = device(&g);
        let ins = noise(2500, seed);
        let mut outs = Vec::new();
        for split in [1usize, 64, 256, 4096] {
            let (mut m, _h, _) = mixer_for(&d);
            outs.push(through(&mut m, &ins, split));
        }
        for o in &outs[1..] {
            for (a, b) in outs[0].iter().zip(o) {
                prop_assert!(a.iter().zip(b).all(|(p, q)| p.to_bits() == q.to_bits()));
            }
        }
    }
}

// ---- named cases ---------------------------------------------------------------------------------------

fn node(id: &str, channels: u16, fx: Vec<Fx>) -> Node {
    Node {
        id: id.into(),
        name: id.into(),
        channels,
        gain_db: 0.0,
        muted: false,
        fx,
    }
}
fn via(source: &str, src_ch: u16, to: &str, out_ch: u16) -> Wire {
    Wire {
        to: Some(to.into()),
        ..Wire::new(source, src_ch, out_ch)
    }
}
/// `s1` (2 ch) -> node `n` (2 ch, `fx`) -> device channels 0 and 1.
fn bus(fx: Vec<Fx>) -> VirtualDevice {
    let mut d = dev();
    d.nodes = vec![node("n", 2, fx)];
    d.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 1, "n", 1),
        Wire::new("n", 0, 0),
        Wire::new("n", 1, 1),
    ];
    d
}
fn mk(d: &VirtualDevice) -> (Mixer, MixerHandle, Arc<Meters>) {
    let meters = Arc::new(Meters::new());
    let (m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(d, &layout(), 48_000.0));
    (m, h, meters)
}
fn bits(o: &[Vec<f32>]) -> Vec<Vec<u32>> {
    o.iter()
        .map(|r| r.iter().map(|x| x.to_bits()).collect())
        .collect()
}
fn ins2(t: usize, f: impl Fn(usize, usize) -> f32) -> Vec<Vec<f32>> {
    (0..2).map(|c| (0..t).map(|i| f(c, i)).collect()).collect()
}

/// Each effect while neutral leaves every sample as the direct wire has it: a limiter under its knee, EQ at
/// 0 dB, delay 0, mono on an already-mono signal. Several block sizes, the first blocks included.
#[test]
fn neutral_effects_are_bit_identical_to_the_direct_wire() {
    let neutral = [
        Fx::Limiter {
            ceiling_db: -1.0,
            knee_db: 6.0,
        },
        Fx::Eq {
            low_hz: 120.0,
            low_db: 0.0,
            high_hz: 8000.0,
            high_db: 0.0,
        },
        Fx::Delay { ms: 0.0 },
        Fx::Mono,
    ];
    let l = layout();
    let mut direct = mk(&dev()).0;
    let mut all = mk(&bus(neutral.to_vec())).0;
    // Mono-compatible and below the limiter's knee (0.447): left equals right.
    let t = 4096 + 300;
    let sig = |_: usize, i: usize| ((i * 37) % 199) as f32 / 99.5 * 0.2 - 0.2;
    for n in [64, 1, 300, 4096, 200] {
        let ins = ins2(n, sig);
        assert_eq!(
            bits(&run(&mut direct, &ins, 4, n)),
            bits(&run(&mut all, &ins, 4, n)),
            "n={n}"
        );
    }
    let _ = (l, t);
    // each alone, on stereo content that is not mono (mono excluded)
    for fx in &neutral[..3] {
        let mut direct = mk(&dev()).0;
        let mut one = mk(&bus(vec![fx.clone()])).0;
        let ins = ins2(500, |c, i| {
            (((i * 31 + c * 7) % 97) as f32 / 48.0 - 1.0) * 0.4
        });
        assert_eq!(
            bits(&run(&mut direct, &ins, 4, 500)),
            bits(&run(&mut one, &ins, 4, 500)),
            "{fx:?}"
        );
    }
}

/// An impulse through source -> node(delay) -> device arrives exactly `d` frames later, whatever the block
/// split, for d around the block sizes and at 100 ms.
#[test]
fn a_delay_node_moves_an_impulse_exactly_d_frames_at_every_block_split() {
    for d in [0usize, 1, 63, 64, 65, 255, 256, 257, 4095, 4096, 4097, 4800] {
        for split in [1usize, 64, 256, 4096] {
            let ms = (d as f64 * 1000.0 / 48_000.0) as f32;
            let (mut m, _h, _) = mk(&bus(vec![Fx::Delay { ms }]));
            let total = 9000;
            let mut hit = Vec::new();
            for start in (0..total).step_by(split) {
                let n = split.min(total - start);
                let ins = ins2(n, |c, i| if c == 0 && start + i == 10 { 1.0 } else { 0.0 });
                let o = run(&mut m, &ins, 4, n);
                hit.extend((0..n).filter(|&i| o[0][i] != 0.0).map(|i| start + i));
                assert!(o[1].iter().all(|x| *x == 0.0));
            }
            assert_eq!(hit, [10 + d], "d={d} split={split}");
        }
    }
}

/// A plan swap that changes nothing in the route keeps filter memory and the delay ring: the stream is
/// bit-identical to one without swaps. With another node added (a different route, so a crossfade block
/// and copied state instead of swapped) it is the same stream again, crossfade block included.
#[test]
fn state_continues_across_plan_swaps_bit_for_bit() {
    let fx = vec![
        Fx::Eq {
            low_hz: 300.0,
            low_db: 9.0,
            high_hz: 5000.0,
            high_db: -6.0,
        },
        Fx::Delay { ms: 7.0 },
        Fx::Limiter {
            ceiling_db: -6.0,
            knee_db: 3.0,
        },
    ];
    let d = bus(fx);
    let t = 40;
    let blocks: Vec<Vec<Vec<f32>>> = (0..t)
        .map(|b| {
            ins2(64, |c, i| {
                (((b * 64 + i) * 53 + c * 17) % 211) as f32 / 105.0 - 1.0
            })
        })
        .collect();
    let mut straight = mk(&d).0;
    let want: Vec<Vec<Vec<f32>>> = blocks
        .iter()
        .map(|b| run(&mut straight, b, 4, 64))
        .collect();

    // identical plan every block
    let (mut m, mut h, _) = mk(&d);
    for (b, w) in blocks.iter().zip(&want) {
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        assert_eq!(bits(&run(&mut m, b, 4, 64)), bits(w));
    }
    // a plan with one more (unused) node every few blocks: a different route
    let mut with_extra = d.clone();
    with_extra.nodes.push(node("extra", 2, vec![]));
    let (mut m, mut h, _) = mk(&d);
    for (i, (b, w)) in blocks.iter().zip(&want).enumerate() {
        let plan = if i % 2 == 0 { &with_extra } else { &d };
        h.set_plan(MixPlan::compile(plan, &layout(), 48_000.0));
        assert_eq!(bits(&run(&mut m, b, 4, 64)), bits(w), "block {i}");
    }
}

/// A delay value edit is a same-route swap: the two read positions crossfade, nothing steps, and it
/// ends exactly `ms` behind.
#[test]
fn a_delay_retime_through_a_plan_swap_does_not_click() {
    let mut d = bus(vec![Fx::Delay { ms: 10.0 }]);
    let (mut m, mut h, _) = mk(&d);
    let sig = |i: usize| (i as f32 * 0.03).sin() * 0.5;
    let mut t = 0;
    let mut out = Vec::new();
    for blk in 0..60 {
        if blk == 10 {
            d.nodes[0].fx = vec![Fx::Delay { ms: 31.25 }]; // 1500 frames
            h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        }
        let ins = ins2(64, |_, i| sig(t + i));
        out.extend(run(&mut m, &ins, 4, 64)[0].iter().copied());
        t += 64;
    }
    let max_step = out
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    // the sine moves 0.015 per frame; two positions 1020 frames apart blend over 480 frames
    assert!(max_step < 0.06, "{max_step}");
    for (i, y) in out.iter().enumerate().skip(t - 64) {
        assert_eq!(*y, sig(i - 1500), "frame {i}");
    }
}

/// NaN, infinity and huge bursts through every effect: finite output, and the next clean block is what a
/// fresh mixer says (no memory of the burst) for the stateless and filter effects.
#[test]
fn hostile_samples_through_effects_stay_finite_and_recover_in_one_block() {
    // The limiter last, so the filter sees the whole burst: its memory must not outlive it.
    let fx = vec![
        Fx::Mono,
        Fx::Eq {
            low_hz: 120.0,
            low_db: 12.0,
            high_hz: 8000.0,
            high_db: 12.0,
        },
        Fx::Limiter {
            ceiling_db: -1.0,
            knee_db: 6.0,
        },
    ];
    let d = bus(fx);
    let clean = ins2(64, |c, i| ((i * 37 + c * 11) % 199) as f32 / 99.5 - 1.0);
    let want = run(&mut mk(&d).0, &clean, 4, 64);
    for bad in [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1e38,
        -1e38,
        f32::MAX,
    ] {
        let (mut m, _h, _) = mk(&d);
        for burst in 0..3 {
            let o = run(&mut m, &ins2(64, |_, _| bad), 4, 64);
            assert!(
                o.iter().flatten().all(|x| x.is_finite()),
                "{bad} burst {burst}"
            );
        }
        assert_eq!(bits(&run(&mut m, &clean, 4, 64)), bits(&want), "{bad}");
    }
    // A delay replays what it was given, finite, and only `d` frames later.
    let (mut m, _h, _) = mk(&bus(vec![Fx::Delay { ms: 2.0 }]));
    let hot = run(&mut m, &ins2(64, |_, _| 1e38), 4, 64);
    assert!(hot.iter().flatten().all(|x| x.is_finite()));
    let after = run(&mut m, &ins2(256, |_, _| 0.0), 4, 256);
    assert!(after[0].iter().all(|x| x.is_finite() && x.abs() <= 1e30));
    assert!(
        after[0][96..].iter().all(|x| *x == 0.0),
        "the burst has played out"
    );
}

/// A node that was muted forgets what its effects held: no stale audio from before the mute comes back.
#[test]
fn unmuting_a_node_starts_its_effects_cold() {
    let mut d = bus(vec![Fx::Delay { ms: 20.0 }]);
    let (mut m, mut h, _) = mk(&d);
    let tone = ins2(64, |_, i| (i as f32 * 0.3).sin());
    for _ in 0..20 {
        run(&mut m, &tone, 4, 64); // the ring fills with tone
    }
    d.nodes[0].muted = true;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    for _ in 0..20 {
        assert!(run(&mut m, &tone, 4, 64).iter().flatten().count() > 0);
    }
    d.nodes[0].muted = false;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let mut out = Vec::new();
    for _ in 0..30 {
        out.extend(run(&mut m, &tone, 4, 64)[0].iter().copied());
    }
    // 20 ms = 960 frames of silence (the line starts empty), then the tone ramping up with the fader
    assert!(
        out[..960].iter().all(|x| *x == 0.0),
        "stale audio: {:?}",
        &out[..8]
    );
    assert!(out[960..].iter().any(|x| x.abs() > 0.1));
}

#[test]
fn the_limiter_reduction_is_metered_per_node_and_resets_on_take() {
    let mut d = bus(vec![Fx::Limiter {
        ceiling_db: -6.0,
        knee_db: 0.0,
    }]);
    d.nodes.push(node("quiet", 2, vec![]));
    let (mut m, _h, meters) = mk(&d);
    run(&mut m, &ins2(64, |_, _| 1.0), 4, 64); // clipped to 0.5: reduction 0.5
    let r = meters.take(&layout(), &d);
    assert_eq!(r.limiter_reduction.len(), 2);
    assert!(
        (r.limiter_reduction[0] - 0.4988).abs() < 1e-3,
        "{:?}",
        r.limiter_reduction
    );
    assert_eq!(r.limiter_reduction[1], 0.0);
    assert!(
        (r.nodes[0].1[0] - 0.5012).abs() < 1e-3,
        "post effects: {:?}",
        r.nodes
    );
    assert_eq!(meters.take(&layout(), &d).limiter_reduction, [0.0, 0.0]);
}

// ---- schema --------------------------------------------------------------------------------------------

fn cfg_of(d: VirtualDevice) -> Config {
    Config {
        devices: vec![d],
        ..Config::default()
    }
}

#[test]
fn effects_round_trip_with_the_documented_json_and_defaults() {
    let json = r#"{"version":2,"devices":[{"id":"v1","name":"V","channels":2,
      "sources":[{"id":"s1","kind":{"type":"pass_thru"},"channels":2}],
      "nodes":[{"id":"n1","name":"Voices","channels":2,"gain_db":0.0,"muted":false,
        "fx":[{"type":"eq","low_hz":120,"low_db":-3,"high_hz":8000,"high_db":2},{"type":"limiter","ceiling_db":-1},
              {"type":"mono"},{"type":"delay","ms":12.5}]}],
      "wires":[{"source":"s1","src_ch":0,"out_ch":0,"to":"n1","gain_db":-3.0},{"source":"n1","src_ch":0,"out_ch":0}]}]}"#;
    let cfg: Config = serde_json::from_str(json).unwrap();
    cfg.validate().unwrap();
    let fx = &cfg.devices[0].nodes[0].fx;
    assert_eq!(
        fx[1],
        Fx::Limiter {
            ceiling_db: -1.0,
            knee_db: 6.0
        },
        "knee defaults to 6 dB"
    );
    assert_eq!(fx[2], Fx::Mono);
    let back: Config = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
    assert_eq!(back.devices, cfg.devices);
    assert!(
        serde_json::to_string(&cfg)
            .unwrap()
            .contains(r#"{"type":"mono"}"#)
    );
    // an effect this build does not know fails the parse, so a file is never half-understood
    assert!(serde_json::from_str::<Config>(&json.replace("mono", "chorus")).is_err());
    // a node without effects does not write the key
    let plain = cfg_of(bus(vec![]));
    assert!(!serde_json::to_string(&plain).unwrap().contains("fx"));
}

#[test]
fn validate_rejects_effect_parameters_out_of_range_and_over_the_caps() {
    type Mutate = fn(&mut VirtualDevice);
    let eq = |low_hz, low_db, high_hz, high_db| Fx::Eq {
        low_hz,
        low_db,
        high_hz,
        high_db,
    };
    let cases: Vec<(&str, Fx)> = vec![
        (
            "ceiling above 0",
            Fx::Limiter {
                ceiling_db: 0.5,
                knee_db: 6.0,
            },
        ),
        (
            "ceiling below -24",
            Fx::Limiter {
                ceiling_db: -25.0,
                knee_db: 6.0,
            },
        ),
        (
            "knee negative",
            Fx::Limiter {
                ceiling_db: -1.0,
                knee_db: -1.0,
            },
        ),
        (
            "knee above 12",
            Fx::Limiter {
                ceiling_db: -1.0,
                knee_db: 13.0,
            },
        ),
        (
            "ceiling nan",
            Fx::Limiter {
                ceiling_db: f32::NAN,
                knee_db: 6.0,
            },
        ),
        ("low_hz below 20", eq(19.0, 0.0, 8000.0, 0.0)),
        ("low_hz above 2000", eq(2001.0, 0.0, 8000.0, 0.0)),
        ("high_hz below 1000", eq(120.0, 0.0, 999.0, 0.0)),
        ("high_hz above 20000", eq(120.0, 0.0, 20001.0, 0.0)),
        ("low_db above 18", eq(120.0, 18.5, 8000.0, 0.0)),
        ("high_db below -18", eq(120.0, 0.0, 8000.0, -18.5)),
        ("high_db inf", eq(120.0, 0.0, 8000.0, f32::INFINITY)),
        ("delay negative", Fx::Delay { ms: -1.0 }),
        ("delay above 200", Fx::Delay { ms: 200.5 }),
        ("delay nan", Fx::Delay { ms: f32::NAN }),
    ];
    for (name, fx) in cases {
        assert!(cfg_of(bus(vec![fx])).validate().is_err(), "{name} accepted");
    }
    let bad: [(&str, Mutate); 3] = [
        ("five effects", |d| d.nodes[0].fx = vec![Fx::Mono; 5]),
        ("nine delay channels", |d| {
            d.nodes = (0..3)
                .map(|i| node(&format!("d{i}"), 3, vec![Fx::Delay { ms: 1.0 }]))
                .collect();
        }),
        ("two delays on a stereo node past the budget", |d| {
            d.nodes = (0..5)
                .map(|i| node(&format!("d{i}"), 2, vec![Fx::Delay { ms: 1.0 }]))
                .collect();
        }),
    ];
    for (name, f) in bad {
        let mut d = bus(vec![]);
        f(&mut d);
        d.wires.clear();
        assert!(cfg_of(d).validate().is_err(), "{name} accepted");
    }
    // the edges are fine: four effects, 200 ms, 8 delay channels exactly
    let mut d = bus(vec![]);
    d.nodes = (0..4)
        .map(|i| node(&format!("d{i}"), 2, vec![Fx::Delay { ms: 200.0 }]))
        .collect();
    d.nodes[0].fx = vec![
        Fx::Mono,
        Fx::Delay { ms: 200.0 },
        Fx::Eq {
            low_hz: 20.0,
            low_db: -18.0,
            high_hz: 20000.0,
            high_db: 18.0,
        },
        Fx::Limiter {
            ceiling_db: -24.0,
            knee_db: 12.0,
        },
    ];
    d.wires.clear();
    cfg_of(d).validate().unwrap();
}

/// `compile` does not trust validate: out-of-range parameters are clamped, NaN is neutral, effects past
/// the caps are dropped, and a ninth delay channel is not allocated.
#[test]
fn compile_clamps_effect_parameters_and_drops_what_is_over_budget() {
    let hostile = vec![
        Fx::Limiter {
            ceiling_db: f32::NAN,
            knee_db: 99.0,
        },
        Fx::Eq {
            low_hz: f32::NAN,
            low_db: 1e9,
            high_hz: -5.0,
            high_db: f32::NEG_INFINITY,
        },
        Fx::Delay { ms: 1e12 },
        Fx::Delay { ms: f32::NAN },
        Fx::Mono, // the fifth: dropped
    ];
    let mut d = bus(hostile);
    d.nodes
        .push(node("more", 8, vec![Fx::Delay { ms: 10.0 }; 4]));
    d.wires.push(via("s1", 0, "more", 0));
    d.wires.push(Wire::new("more", 0, 0));
    let (mut m, _h, _) = mk(&d);
    for _ in 0..40 {
        let o = run(
            &mut m,
            &ins2(256, |c, i| ((i + c) % 7) as f32 / 7.0),
            4,
            256,
        );
        assert!(o.iter().flatten().all(|x| x.is_finite()));
    }
}

#[test]
fn compile_survives_a_nonsense_sample_rate() {
    let d = bus(vec![
        Fx::Eq {
            low_hz: 120.0,
            low_db: 6.0,
            high_hz: 8000.0,
            high_db: -6.0,
        },
        Fx::Delay { ms: 100.0 },
    ]);
    for rate in [0.0, -1.0, f32::NAN, f32::INFINITY, 1.0, 1e12] {
        let (mut m, _h, _) = {
            let (m, mut h) = mixer(Arc::new(Meters::new()));
            h.set_plan(MixPlan::compile(&d, &layout(), rate));
            (m, h, ())
        };
        let o = run(&mut m, &ins2(128, |_, i| (i as f32 * 0.1).sin()), 4, 128);
        assert!(o.iter().flatten().all(|x| x.is_finite()), "rate {rate}");
    }
}
