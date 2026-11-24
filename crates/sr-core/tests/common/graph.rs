//! Random node graphs with an f64 time-domain reference (`simulate`), shared by `nodes.rs` and
//! `effects.rs`. The reference is written from the specification (`docs/NODES.md`), not from `sr_core`:
//! it uses its own cookbook shelves in direct form I, its own soft clip and a plain shifted copy for the
//! delay.
#![allow(dead_code)]
use super::{dev, run, src};
use proptest::prelude::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

pub const SRCS: usize = 3; // stereo sources s0..s2
pub const NCH: usize = 4; // vdev channels
pub const RATE: f64 = 48_000.0;

/// A signal in the graph: a source channel or a node row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sig {
    Src(usize, usize),
    Node(usize, usize),
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Node(usize, usize),
    Vdev(usize),
}
#[derive(Clone, Debug)]
pub struct W {
    pub from: Sig,
    pub to: Target,
    pub db: f32,
    pub invert: bool,
}
#[derive(Clone, Debug)]
pub struct NodeSpec {
    pub ch: u16,
    pub db: f32,
    pub muted: bool,
    pub fx: Vec<Fx>,
}
#[derive(Clone, Debug)]
pub struct G {
    pub src_db: Vec<(f32, bool)>,
    pub nodes: Vec<NodeSpec>,
    pub wires: Vec<W>,
}

/// Which effects a random node may carry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FxSet {
    /// Buses only.
    None,
    /// Mono, limiter and cutting EQs: levels never exceed the flat graph's, and a constant input
    /// settles within 2000 frames.
    Steady,
    /// Everything, boosts and delays included.
    All,
}

fn fx_strategy(set: FxSet) -> BoxedStrategy<Fx> {
    let mono = Just(Fx::Mono);
    let limiter = (-24.0f32..=0.0, 0.0f32..=12.0).prop_map(|(ceiling_db, knee_db)| Fx::Limiter {
        ceiling_db,
        knee_db,
    });
    let eq = |lo: f32, hi: f32, low_hz: f32| {
        (low_hz..=2000.0, lo..=hi, 1000.0f32..=20000.0, lo..=hi).prop_map(
            |(low_hz, low_db, high_hz, high_db)| Fx::Eq {
                low_hz,
                low_db,
                high_hz,
                high_db,
            },
        )
    };
    let delay = (0u32..2000).prop_map(|d| Fx::Delay {
        ms: (f64::from(d) * 1000.0 / RATE) as f32,
    });
    match set {
        FxSet::None => Just(Fx::Mono).boxed(), // never drawn: `graph` skips fx for this set
        FxSet::Steady => prop_oneof![mono, limiter, eq(-18.0, 0.0, 100.0)].boxed(),
        FxSet::All => prop_oneof![mono, limiter, eq(-18.0, 18.0, 20.0), delay].boxed(),
    }
}

/// Random DAG: `keys` order the nodes (a wire goes from the lower to the higher key), which is unrelated
/// to their config order, so the plan has to find the topological order itself.
pub fn graph(set: FxSet) -> impl Strategy<Value = G> {
    let fx = if set == FxSet::None {
        Just(vec![]).boxed()
    } else {
        prop::collection::vec(fx_strategy(set), 0..=2).boxed()
    };
    (
        prop::collection::vec((-12.0f32..=6.0, any::<bool>()), SRCS),
        prop::collection::vec((1u16..=3, -12.0f32..=6.0, any::<bool>(), fx), 0..=6),
        prop::collection::vec(any::<u8>(), 6),
        prop::collection::vec((any::<u32>(), -12.0f32..=6.0, any::<bool>()), 0..24),
    )
        .prop_map(|(src_db, nodes, keys, picks)| {
            let mut budget = MAX_DELAY_CH; // the device's delay lines, spent in config order like `compile`
            let nodes: Vec<NodeSpec> = nodes
                .into_iter()
                .map(|(ch, db, muted, fx)| NodeSpec {
                    ch,
                    db,
                    muted,
                    fx: fx
                        .into_iter()
                        .filter(|f| {
                            !matches!(f, Fx::Delay { .. })
                                || (usize::from(ch) <= budget && {
                                    budget -= usize::from(ch);
                                    true
                                })
                        })
                        .collect(),
                })
                .collect();
            let mut sigs: Vec<Sig> = (0..SRCS)
                .flat_map(|s| (0..2).map(move |c| Sig::Src(s, c)))
                .collect();
            for (n, spec) in nodes.iter().enumerate() {
                sigs.extend((0..usize::from(spec.ch)).map(|c| Sig::Node(n, c)));
            }
            let rank = |n: usize| (keys[n], n);
            let mut cands: Vec<(Sig, Target)> = Vec::new();
            for &s in &sigs {
                for c in 0..NCH {
                    cands.push((s, Target::Vdev(c)));
                }
                for (b, spec) in nodes.iter().enumerate() {
                    let ok = match s {
                        Sig::Src(..) => true,
                        Sig::Node(a, _) => rank(a) < rank(b),
                    };
                    if ok {
                        cands.extend((0..usize::from(spec.ch)).map(|c| (s, Target::Node(b, c))));
                    }
                }
            }
            let mut taken = vec![false; cands.len()];
            let mut wires = Vec::new();
            for (i, db, invert) in picks {
                let i = i as usize % cands.len();
                if !std::mem::replace(&mut taken[i], true) {
                    wires.push(W {
                        from: cands[i].0,
                        to: cands[i].1,
                        db,
                        invert,
                    });
                }
            }
            G {
                src_db,
                nodes,
                wires,
            }
        })
}

pub fn device(g: &G) -> VirtualDevice {
    let mut d = dev();
    d.channels = NCH as u16;
    d.sources = (0..SRCS)
        .map(|i| src(&format!("s{i}"), 2, g.src_db[i].0, g.src_db[i].1))
        .collect();
    d.nodes = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| Node {
            id: format!("n{i}"),
            name: format!("Node {i}"),
            channels: n.ch,
            gain_db: n.db,
            muted: n.muted,
            fx: n.fx.clone(),
        })
        .collect();
    d.wires = g
        .wires
        .iter()
        .map(|w| {
            let (source, src_ch) = match w.from {
                Sig::Src(i, c) => (format!("s{i}"), c),
                Sig::Node(i, c) => (format!("n{i}"), c),
            };
            let (to, out_ch) = match w.to {
                Target::Node(i, c) => (Some(format!("n{i}")), c),
                Target::Vdev(c) => (None, c),
            };
            Wire {
                gain_db: w.db,
                invert: w.invert,
                to,
                ..Wire::new(source, src_ch as u16, out_ch as u16)
            }
        })
        .collect();
    d
}

pub fn layout6() -> IoLayout {
    IoLayout {
        inputs: (0..SRCS * 2)
            .map(|i| Some((format!("s{}", i / 2), (i % 2) as u16)))
            .collect(),
        outputs: (0..NCH as u16).map(|c| Some(OutPort::Device(c))).collect(),
    }
}

pub fn mixer_for(d: &VirtualDevice) -> (Mixer, MixerHandle, Arc<Meters>) {
    let meters = Arc::new(Meters::new());
    let (m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(d, &layout6(), RATE as f32));
    (m, h, meters)
}

pub fn block(m: &mut Mixer, x: &[f64], n: usize) -> Vec<Vec<f32>> {
    let ins: Vec<Vec<f32>> = x.iter().map(|v| vec![*v as f32; n]).collect();
    run(m, &ins, NCH, n)
}

pub fn lin(db: f32, off: bool) -> f64 {
    if off {
        0.0
    } else {
        10f64.powf(f64::from(db) / 20.0)
    }
}

// ---- the reference ---------------------------------------------------------------------------------------

/// RBJ cookbook shelf (slope 1) as b0, b1, b2, a1, a2 with a0 divided out.
fn shelf(high: bool, hz: f64, db: f64) -> [f64; 5] {
    let a = 10f64.powf(db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * hz / RATE;
    let alpha = w0.sin() / 2.0 * 2f64.sqrt();
    let cs = w0.cos();
    let r = 2.0 * a.sqrt() * alpha;
    let (b0, b1, b2, a0, a1, a2) = if high {
        (
            a * ((a + 1.0) + (a - 1.0) * cs + r),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cs),
            a * ((a + 1.0) + (a - 1.0) * cs - r),
            (a + 1.0) - (a - 1.0) * cs + r,
            2.0 * ((a - 1.0) - (a + 1.0) * cs),
            (a + 1.0) - (a - 1.0) * cs - r,
        )
    } else {
        (
            a * ((a + 1.0) - (a - 1.0) * cs + r),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cs),
            a * ((a + 1.0) - (a - 1.0) * cs - r),
            (a + 1.0) + (a - 1.0) * cs + r,
            -2.0 * ((a - 1.0) + (a + 1.0) * cs),
            (a + 1.0) + (a - 1.0) * cs - r,
        )
    };
    [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

/// Direct form I, zero history.
fn biquad(x: &[f64], [b0, b1, b2, a1, a2]: [f64; 5]) -> Vec<f64> {
    let mut y = vec![0.0; x.len()];
    for t in 0..x.len() {
        let g = |v: &[f64], k: usize| if t >= k { v[t - k] } else { 0.0 };
        y[t] = b0 * x[t] + b1 * g(x, 1) + b2 * g(x, 2) - a1 * g(&y, 1) - a2 * g(&y, 2);
    }
    y
}

fn soft_clip(x: f64, ceiling_db: f64, knee_db: f64) -> f64 {
    let c = 10f64.powf(ceiling_db / 20.0);
    let k = c * 10f64.powf(-knee_db / 20.0);
    if x.abs() <= k {
        return x;
    }
    let u = ((x.abs() - k) / (c - k)).min(3.0);
    (k + (c - k) * u * (27.0 + u * u) / (27.0 + 9.0 * u * u)).copysign(x)
}

/// One effect over a node's rows.
fn apply(fx: &Fx, rows: &mut [Vec<f64>]) {
    match *fx {
        Fx::Mono => {
            for p in 0..rows.len() / 2 {
                for t in 0..rows[0].len() {
                    let m = 0.5 * (rows[2 * p][t] + rows[2 * p + 1][t]);
                    rows[2 * p][t] = m;
                    rows[2 * p + 1][t] = m;
                }
            }
        }
        Fx::Limiter {
            ceiling_db,
            knee_db,
        } => rows
            .iter_mut()
            .flatten()
            .for_each(|x| *x = soft_clip(*x, f64::from(ceiling_db), f64::from(knee_db))),
        Fx::Eq {
            low_hz,
            low_db,
            high_hz,
            high_db,
        } => {
            for r in rows.iter_mut() {
                if low_db != 0.0 {
                    *r = biquad(r, shelf(false, f64::from(low_hz), f64::from(low_db)));
                }
                if high_db != 0.0 {
                    *r = biquad(r, shelf(true, f64::from(high_hz), f64::from(high_db)));
                }
            }
        }
        Fx::Delay { ms } => {
            let d = (f64::from(ms) * RATE / 1000.0).round() as usize;
            for r in rows.iter_mut() {
                let d = d.min(r.len());
                let mut shifted = vec![0.0; r.len()];
                shifted[d..].copy_from_slice(&r[..r.len() - d]);
                *r = shifted;
            }
        }
    }
}

/// The vdev channels for the source channels `ins` (6 time series), naively in f64. `flat`: every gain 1,
/// nothing muted or inverted, only mono kept (as a sum): an upper bound of any level for `ins >= 0` and
/// gains <= 1.
pub fn simulate(g: &G, ins: &[Vec<f64>], flat: bool) -> Vec<Vec<f64>> {
    simulate_with(g, ins, flat, 0.0)
}

/// `simulate` with `inject` added to every node row before its effects: stands for what filter memory and
/// delay lines inherited from an earlier plan can hold.
pub fn simulate_with(g: &G, ins: &[Vec<f64>], flat: bool, inject: f64) -> Vec<Vec<f64>> {
    type Rows = Vec<Vec<f64>>;
    fn sig(
        g: &G,
        ins: &[Vec<f64>],
        flat: bool,
        inject: f64,
        memo: &mut Vec<Option<Rows>>,
        s: Sig,
    ) -> Vec<f64> {
        match s {
            Sig::Src(i, c) => {
                let v = if flat {
                    1.0
                } else {
                    lin(g.src_db[i].0, g.src_db[i].1)
                };
                ins[i * 2 + c].iter().map(|x| x * v).collect()
            }
            Sig::Node(n, c) => {
                if memo[n].is_none() {
                    let mut rows: Rows = (0..usize::from(g.nodes[n].ch))
                        .map(|c| {
                            let mut r = sum(g, ins, flat, inject, memo, Target::Node(n, c));
                            r.iter_mut().for_each(|x| *x += inject);
                            r
                        })
                        .collect();
                    for f in &g.nodes[n].fx {
                        if !flat {
                            apply(f, &mut rows);
                        } else if *f == Fx::Mono {
                            // The bound: a pair's energy may land on either channel, so each takes the sum.
                            for p in 0..rows.len() / 2 {
                                for t in 0..rows[0].len() {
                                    let m = rows[2 * p][t] + rows[2 * p + 1][t];
                                    (rows[2 * p][t], rows[2 * p + 1][t]) = (m, m);
                                }
                            }
                        }
                    }
                    memo[n] = Some(rows);
                }
                let v = if flat {
                    1.0
                } else {
                    lin(g.nodes[n].db, g.nodes[n].muted)
                };
                memo[n].as_ref().unwrap()[c].iter().map(|x| x * v).collect()
            }
        }
    }
    fn sum(
        g: &G,
        ins: &[Vec<f64>],
        flat: bool,
        inject: f64,
        memo: &mut Vec<Option<Rows>>,
        t: Target,
    ) -> Vec<f64> {
        let mut acc = vec![0.0; ins[0].len()];
        for w in g.wires.iter().filter(|w| w.to == t) {
            let trim = if flat {
                1.0
            } else {
                lin(w.db, false) * if w.invert { -1.0 } else { 1.0 }
            };
            for (a, x) in acc.iter_mut().zip(sig(g, ins, flat, inject, memo, w.from)) {
                *a += trim * x;
            }
        }
        acc
    }
    let mut memo: Vec<Option<Rows>> = vec![None; g.nodes.len()];
    (0..NCH)
        .map(|c| sum(g, ins, flat, inject, &mut memo, Target::Vdev(c)))
        .collect()
}

/// `simulate` for constant inputs `x`, `t` frames.
pub fn constant(g: &G, x: &[f64], t: usize, flat: bool) -> Vec<Vec<f64>> {
    let ins: Vec<Vec<f64>> = x.iter().map(|v| vec![*v; t]).collect();
    simulate(g, &ins, flat)
}

/// Steady output per vdev channel for constant inputs: 3000 frames in, the last one out.
pub fn steady(g: &G, x: &[f64], flat: bool) -> Vec<f64> {
    constant(g, x, 3000, flat)
        .iter()
        .map(|c| *c.last().unwrap())
        .collect()
}

/// Upper bound of any node row's level for constant inputs `x` (flat gains): every row, summed.
pub fn row_bound(g: &G, x: &[f64]) -> f64 {
    let mut all = g.clone();
    for (n, spec) in g.nodes.iter().enumerate() {
        for c in 0..usize::from(spec.ch) {
            all.wires.push(W {
                from: Sig::Node(n, c),
                to: Target::Vdev(0),
                db: 0.0,
                invert: false,
            });
        }
    }
    steady(&all, x, true)[0]
}

/// Largest level a vdev channel can reach while moving from `a` to `g`: every path of either at full
/// gain, plus whatever the nodes' effect memory and delay lines that `g` inherits from `a` can hold
/// (bounded by the rows of either graph, fed into every node of `g`).
pub fn swap_bound(a: &G, g: &G, x: &[f64]) -> Vec<f64> {
    let inject = row_bound(a, x) + row_bound(g, x);
    let ins: Vec<Vec<f64>> = x.iter().map(|v| vec![*v; 3000]).collect();
    steady(a, x, true)
        .iter()
        .zip(simulate_with(g, &ins, true, inject))
        .map(|(p, q)| p + q.last().unwrap())
        .collect()
}
