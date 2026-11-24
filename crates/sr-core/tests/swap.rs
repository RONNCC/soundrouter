//! Plan swaps against a naive f64 oracle: a swap that adds or removes a wire, a source or a monitor
//! must not step the output (clicks), and the output after the swap must be what the oracle says.
mod common;
use common::*;
use proptest::prelude::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

const SRCS: usize = 3; // stereo sources s0..s2
const NCH: usize = 4; // vdev channels
/// Outputs: vdev ch 0..4, then monitor "spk" ch 0 and 1.
const OUTS: usize = NCH + 2;

#[derive(Clone, Debug)]
struct Cfg {
    wires: Vec<bool>, // [src][ch][out]
    src_db: Vec<(f32, bool)>,
    dev_db: f32,
    mon: Option<(f32, Vec<bool>)>, // volume, [out][dst]
}

fn cfg() -> impl Strategy<Value = Cfg> {
    (
        prop::collection::vec(any::<bool>(), SRCS * 2 * NCH),
        prop::collection::vec((-20.0f32..=6.0, any::<bool>()), SRCS),
        -12.0f32..=0.0,
        prop::option::of((
            -12.0f32..=0.0,
            prop::collection::vec(any::<bool>(), NCH * 2),
        )),
    )
        .prop_map(|(wires, src_db, dev_db, mon)| Cfg {
            wires,
            src_db,
            dev_db,
            mon,
        })
}

fn device(c: &Cfg) -> VirtualDevice {
    let mut d = dev();
    d.channels = NCH as u16;
    d.volume_db = c.dev_db;
    d.sources = (0..SRCS)
        .map(|i| src(&format!("s{i}"), 2, c.src_db[i].0, c.src_db[i].1))
        .collect();
    d.wires = (0..SRCS * 2 * NCH)
        .filter(|&k| c.wires[k])
        .map(|k| {
            Wire::new(
                format!("s{}", k / (2 * NCH)),
                ((k / NCH) % 2) as u16,
                (k % NCH) as u16,
            )
        })
        .collect();
    d.monitors = c
        .mon
        .iter()
        .map(|(db, w)| Monitor {
            device_uid: "spk".into(),
            enabled: true,
            volume_db: *db,
            wires: (0..NCH * 2)
                .filter(|&k| w[k])
                .map(|k| MonitorWire {
                    out_ch: (k / 2) as u16,
                    dst_ch: (k % 2) as u16,
                })
                .collect(),
            name: String::new(),
        })
        .collect();
    d
}

fn layout6() -> IoLayout {
    IoLayout {
        inputs: (0..SRCS * 2)
            .map(|i| Some((format!("s{}", i / 2), (i % 2) as u16)))
            .collect(),
        outputs: (0..NCH as u16)
            .map(|c| Some(OutPort::Device(c)))
            .chain((0..2).map(|ch| {
                Some(OutPort::Monitor {
                    uid: "spk".into(),
                    ch,
                })
            }))
            .collect(),
    }
}

fn lin(db: f32, off: bool) -> f64 {
    if off {
        0.0
    } else {
        10f64.powf(f64::from(db) / 20.0)
    }
}

/// Steady-state output per slot for constant inputs `x`, summed naively in f64. Monitor wires that share
/// a destination average when `avg` (what the mixer does); without it they add, an upper bound for `x >= 0`.
fn oracle(c: &Cfg, x: &[f64], avg: bool) -> Vec<f64> {
    let mut acc = [0.0f64; NCH];
    for k in 0..SRCS * 2 * NCH {
        if c.wires[k] {
            let (s, ch, out) = (k / (2 * NCH), (k / NCH) % 2, k % NCH);
            acc[out] += x[s * 2 + ch] * lin(c.src_db[s].0, c.src_db[s].1);
        }
    }
    let dev = lin(c.dev_db, false);
    let mut out: Vec<f64> = acc.iter().map(|a| a * dev).collect();
    let mut mon = [0.0f64; 2];
    if let Some((db, w)) = &c.mon {
        for d in 0..2 {
            let n = (0..NCH).filter(|o| w[o * 2 + d]).count();
            let div = if avg { n.max(1) as f64 } else { 1.0 };
            for o in (0..NCH).filter(|o| w[o * 2 + d]) {
                mon[d] += out[o] * lin(*db, false) / div;
            }
        }
    }
    out.extend(mon);
    out
}

/// Upper bound of any slot's level while moving from `a` to `b`: the oracle on the union of both
/// routings with the larger gain at every stage (`flat`: every stage at its ceiling, +6 dB per source,
/// 0 dB device and monitor: what a slewing gain, which moves 1/480 per frame whatever its level, scales).
fn reach(a: &Cfg, b: &Cfg, x: &[f64], flat: bool) -> Vec<f64> {
    let u = Cfg {
        wires: a
            .wires
            .iter()
            .zip(&b.wires)
            .map(|(p, q)| *p || *q)
            .collect(),
        src_db: (0..SRCS)
            .map(|i| {
                let (ga, gb) = (
                    lin(a.src_db[i].0, a.src_db[i].1),
                    lin(b.src_db[i].0, b.src_db[i].1),
                );
                (20.0 * ga.max(gb).max(1e-9).log10() as f32, false)
            })
            .collect(),
        dev_db: a.dev_db.max(b.dev_db),
        mon: match (&a.mon, &b.mon) {
            (Some((da, wa)), Some((db, wb))) => Some((
                da.max(*db),
                wa.iter().zip(wb).map(|(p, q)| *p || *q).collect(),
            )),
            (Some(m), None) | (None, Some(m)) => Some(m.clone()),
            (None, None) => None,
        },
    };
    if flat {
        let mut u = u;
        u.src_db = vec![(6.0, false); SRCS];
        u.dev_db = 0.0;
        u.mon = u.mon.map(|(_, w)| (0.0, w));
        return oracle(&u, x, false);
    }
    oracle(&u, x, false)
}

fn block(m: &mut Mixer, x: &[f64], n: usize) -> Vec<Vec<f32>> {
    let ins: Vec<Vec<f32>> = x.iter().map(|v| vec![*v as f32; n]).collect();
    run(m, &ins, OUTS, n)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Swaps between random routings: no output steps by more than a crossfade over the swap block (or a
    /// gain slew after it) allows (a hard step of the whole level change is a click), and 2000 frames
    /// later every slot sits on the oracle's steady value.
    #[test]
    fn swaps_neither_click_nor_drift_from_the_oracle(
        seq in prop::collection::vec((cfg(), 16usize..200), 2..8),
        x in prop::collection::vec(0.1f64..=1.0, SRCS * 2),
    ) {
        let (mut m, mut h) = mixer(Arc::new(Meters::new()));
        h.set_plan(MixPlan::compile(&device(&seq[0].0), &layout6(), 48_000.0));
        block(&mut m, &x, seq[0].1);
        let mut last = oracle(&seq[0].0, &x, true);
        let mut prevc = seq[0].0.clone();
        for (c, n) in &seq[1..] {
            h.set_plan(MixPlan::compile(&device(c), &layout6(), 48_000.0));
            let want = oracle(c, &x, true);
            let top = reach(&prevc, c, &x, false);
            let flat = reach(&prevc, c, &x, true);
            // The swap block, then blocks until the gains have settled (+24 dB to silence is 1800 frames).
            for b in 0..=2000usize.div_ceil(*n) {
                let o = block(&mut m, &x, *n);
                for s in 0..OUTS {
                    // the crossfade, the source, device and monitor gain ramps can all be moving at once;
                    // after the swap block only the gain slews are
                    let bound = if b == 0 { 5.0 * top[s] / *n as f64 + 8.0 * flat[s] / 480.0 } else { 8.0 * flat[s] / 480.0 } + 1e-4;
                    for &v in &o[s] {
                        prop_assert!(
                            (f64::from(v) - last[s]).abs() <= bound,
                            "slot {s} block {b}: {} -> {v} exceeds {bound} (n={n})",
                            last[s]
                        );
                        last[s] = f64::from(v);
                    }
                }
            }
            let o = block(&mut m, &x, *n);
            for s in 0..OUTS {
                prop_assert!(
                    o[s].iter().all(|v| (f64::from(*v) - want[s]).abs() <= 1e-3 * (1.0 + want[s].abs())),
                    "slot {s} settled at {}, oracle {}", o[s][0], want[s]
                );
            }
            last = want;
            prevc = c.clone();
        }
    }
}

/// The plain case, named: a wire taken away mid-stream fades out instead of dropping to zero.
#[test]
fn a_removed_wire_fades_out_and_an_added_one_fades_in() {
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    let n = 64;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    run(&mut m, &ins, 4, n);
    let mut d = dev();
    d.wires.pop(); // ch 1 -> out 1 goes away
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, n);
    assert!(o[1][0] > 0.9, "starts where it was: {}", o[1][0]);
    assert!(
        o[1][n - 1].abs() < 1e-5,
        "lands on silence: {}",
        o[1][n - 1]
    );
    assert!(
        o[1].windows(2)
            .all(|w| (w[0] - w[1]).abs() <= 2.0 / n as f32),
        "{:?}",
        o[1]
    );
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, n);
    assert!(o[1][0] < 0.1, "fades in from silence: {}", o[1][0]);
    assert!((o[1][n - 1] - 1.0).abs() < 1e-5);
}
