//! Mixer nodes (buses): a naive f64 oracle over random DAGs, bit-exact neutrality, topological order,
//! meters, mute, hostile samples, block-size invariance, and swaps between graphs.
mod common;
use common::*;
use proptest::prelude::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

const SRCS: usize = 3; // stereo sources s0..s2
const NCH: usize = 4; // vdev channels

/// A signal in the graph: a source channel or a node row.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Sig {
    Src(usize, usize),
    Node(usize, usize),
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum Target {
    Node(usize, usize),
    Vdev(usize),
}
#[derive(Clone, Debug)]
struct W {
    from: Sig,
    to: Target,
    db: f32,
    invert: bool,
}
#[derive(Clone, Debug)]
struct G {
    src_db: Vec<(f32, bool)>,
    nodes: Vec<(u16, f32, bool)>, // channels, fader dB, muted
    wires: Vec<W>,
}

/// Random DAG: `keys` order the nodes (a wire goes from the lower to the higher key), which is unrelated
/// to their config order, so the plan has to find the topological order itself.
fn graph() -> impl Strategy<Value = G> {
    (
        prop::collection::vec((-12.0f32..=6.0, any::<bool>()), SRCS),
        prop::collection::vec((1u16..=3, -12.0f32..=6.0, any::<bool>()), 0..=6),
        prop::collection::vec(any::<u8>(), 6),
        prop::collection::vec((any::<u32>(), -12.0f32..=6.0, any::<bool>()), 0..24),
    )
        .prop_map(|(src_db, nodes, keys, picks)| {
            let mut sigs: Vec<Sig> = (0..SRCS)
                .flat_map(|s| (0..2).map(move |c| Sig::Src(s, c)))
                .collect();
            for (n, (ch, ..)) in nodes.iter().enumerate() {
                sigs.extend((0..usize::from(*ch)).map(|c| Sig::Node(n, c)));
            }
            let rank = |n: usize| (keys[n], n);
            let mut cands: Vec<(Sig, Target)> = Vec::new();
            for &s in &sigs {
                for c in 0..NCH {
                    cands.push((s, Target::Vdev(c)));
                }
                for (b, (ch, ..)) in nodes.iter().enumerate() {
                    let ok = match s {
                        Sig::Src(..) => true,
                        Sig::Node(a, _) => rank(a) < rank(b),
                    };
                    if ok {
                        cands.extend((0..usize::from(*ch)).map(|c| (s, Target::Node(b, c))));
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

fn lin(db: f32, off: bool) -> f64 {
    if off {
        0.0
    } else {
        10f64.powf(f64::from(db) / 20.0)
    }
}

/// Steady output per vdev channel for constant inputs `x` (6 slots), summed naively in f64. `flat`:
/// every gain 1, nothing muted or inverted: an upper bound of any level for `x >= 0` and gains <= 1.
fn oracle(g: &G, x: &[f64], flat: bool) -> Vec<f64> {
    fn val(g: &G, x: &[f64], flat: bool, memo: &mut Vec<Vec<Option<f64>>>, s: Sig) -> f64 {
        match s {
            Sig::Src(i, c) => {
                x[i * 2 + c]
                    * if flat {
                        1.0
                    } else {
                        lin(g.src_db[i].0, g.src_db[i].1)
                    }
            }
            Sig::Node(n, c) => {
                let row = match memo[n][c] {
                    Some(v) => v,
                    None => {
                        let v = sum(g, x, flat, memo, Target::Node(n, c));
                        memo[n][c] = Some(v);
                        v
                    }
                };
                let (_, db, muted) = g.nodes[n];
                row * if flat { 1.0 } else { lin(db, muted) }
            }
        }
    }
    fn sum(g: &G, x: &[f64], flat: bool, memo: &mut Vec<Vec<Option<f64>>>, t: Target) -> f64 {
        g.wires
            .iter()
            .filter(|w| w.to == t)
            .map(|w| {
                let trim = if flat {
                    1.0
                } else {
                    lin(w.db, false) * if w.invert { -1.0 } else { 1.0 }
                };
                trim * val(g, x, flat, memo, w.from)
            })
            .sum()
    }
    let mut memo: Vec<Vec<Option<f64>>> = g
        .nodes
        .iter()
        .map(|(ch, ..)| vec![None; usize::from(*ch)])
        .collect();
    (0..NCH)
        .map(|c| sum(g, x, flat, &mut memo, Target::Vdev(c)))
        .collect()
}

fn device(g: &G) -> VirtualDevice {
    let mut d = dev();
    d.channels = NCH as u16;
    d.sources = (0..SRCS)
        .map(|i| src(&format!("s{i}"), 2, g.src_db[i].0, g.src_db[i].1))
        .collect();
    d.nodes = g
        .nodes
        .iter()
        .enumerate()
        .map(|(i, &(channels, gain_db, muted))| Node {
            id: format!("n{i}"),
            name: format!("Node {i}"),
            channels,
            gain_db,
            muted,
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

fn layout6() -> IoLayout {
    IoLayout {
        inputs: (0..SRCS * 2)
            .map(|i| Some((format!("s{}", i / 2), (i % 2) as u16)))
            .collect(),
        outputs: (0..NCH as u16).map(|c| Some(OutPort::Device(c))).collect(),
    }
}

fn mixer_for(d: &VirtualDevice) -> (Mixer, MixerHandle, Arc<Meters>) {
    let meters = Arc::new(Meters::new());
    let (m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(d, &layout6(), 48_000.0));
    (m, h, meters)
}

fn block(m: &mut Mixer, x: &[f64], n: usize) -> Vec<Vec<f32>> {
    let ins: Vec<Vec<f32>> = x.iter().map(|v| vec![*v as f32; n]).collect();
    run(m, &ins, NCH, n)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// Whatever the DAG (node order in the config unrelated to the signal order), the settled output is the
    /// naive sum of gains along every path.
    #[test]
    fn output_is_the_sum_of_gains_over_every_path(
        g in graph(),
        n in 1usize..200,
        x in prop::collection::vec(-1.0f64..=1.0, SRCS * 2),
    ) {
        let d = device(&g);
        let cfg = Config { devices: vec![d.clone()], ..Config::default() };
        prop_assert!(cfg.validate().is_ok());
        let (mut m, _h, _) = mixer_for(&d);
        let want = oracle(&g, &x, false);
        let got = block(&mut m, &x, n);
        for c in 0..NCH {
            prop_assert!(
                got[c].iter().all(|v| (f64::from(*v) - want[c]).abs() <= 1e-4 * (1.0 + want[c].abs())),
                "ch {c}: {} vs {}", got[c][0], want[c]
            );
        }
    }

    /// Settled gains make every block split bit-identical: nothing in a node depends on where blocks end.
    #[test]
    fn block_splits_do_not_change_a_sample(g in graph(), seed in any::<u64>()) {
        let d = device(&g);
        let total = 600;
        let ins: Vec<Vec<f32>> = (0..SRCS * 2)
            .map(|c| (0..total).map(|i| (((i as u64 * 2654435761).wrapping_add(seed ^ c as u64)) % 2001) as f32 / 1000.0 - 1.0).collect())
            .collect();
        let mut outs: Vec<Vec<Vec<f32>>> = Vec::new();
        for split in [1usize, 64, 256, 4096] {
            let (mut m, _h, _) = mixer_for(&d);
            let mut out = vec![Vec::new(); NCH];
            for start in (0..total).step_by(split) {
                let end = (start + split).min(total);
                let part: Vec<Vec<f32>> = ins.iter().map(|v| v[start..end].to_vec()).collect();
                for (o, p) in out.iter_mut().zip(run(&mut m, &part, NCH, end - start)) {
                    o.extend(p);
                }
            }
            outs.push(out);
        }
        for o in &outs[1..] {
            for (a, b) in outs[0].iter().zip(o) {
                prop_assert!(a.iter().zip(b).all(|(p, q)| p.to_bits() == q.to_bits()));
            }
        }
    }

    /// Swaps between random graphs (a node or wire appears, goes away, is re-pointed, a fader moves): the
    /// swap block crossfades and the rest are gain slews, never a step of the whole level, and 2000 frames
    /// later the output is the new graph's oracle.
    #[test]
    fn swaps_between_graphs_neither_click_nor_drift(
        seq in prop::collection::vec((graph(), 16usize..200), 2..6),
        x in prop::collection::vec(0.1f64..=1.0, SRCS * 2),
    ) {
        let mut seq = seq;
        // The tests' levels stay below 1 per stage so the flat graph bounds every path.
        for (g, _) in &mut seq {
            g.src_db.iter_mut().for_each(|s| s.0 = s.0.min(0.0));
            g.nodes.iter_mut().for_each(|n| n.1 = n.1.min(0.0));
            g.wires.iter_mut().for_each(|w| w.db = w.db.min(0.0));
        }
        let (mut m, mut h, _) = mixer_for(&device(&seq[0].0));
        block(&mut m, &x, seq[0].1);
        let mut last = oracle(&seq[0].0, &x, false);
        let mut prev = seq[0].0.clone();
        for (g, n) in &seq[1..] {
            h.set_plan(MixPlan::compile(&device(g), &layout6(), 48_000.0));
            let want = oracle(g, &x, false);
            let level = bound(&prev, g, &x);
            for b in 0..=2000usize.div_ceil(*n) {
                let o = block(&mut m, &x, *n);
                for c in 0..NCH {
                    // up to ~16 gain stages slew at once; the first block also crossfades
                    let step = 16.0 * level[c] / 480.0 + if b == 0 { 5.0 * level[c] / *n as f64 } else { 0.0 } + 1e-4;
                    for &v in &o[c] {
                        prop_assert!(v.is_finite());
                        prop_assert!(
                            (f64::from(v) - last[c]).abs() <= step,
                            "ch {c} block {b}: {} -> {v} exceeds {step} (n={n})", last[c]
                        );
                        last[c] = f64::from(v);
                    }
                }
            }
            let o = block(&mut m, &x, *n);
            for c in 0..NCH {
                prop_assert!(
                    o[c].iter().all(|v| (f64::from(*v) - want[c]).abs() <= 1e-3 * (1.0 + want[c].abs())),
                    "ch {c} settled at {}, oracle {}", o[c][0], want[c]
                );
            }
            last = want;
            prev = g.clone();
        }
    }
}

/// Largest level a vdev channel can reach moving between two graphs: every path of either, at full gain.
fn bound(a: &G, b: &G, x: &[f64]) -> Vec<f64> {
    let (oa, ob) = (oracle(a, x, true), oracle(b, x, true));
    oa.iter().zip(&ob).map(|(p, q)| p + q).collect()
}

// ---- named cases ---------------------------------------------------------------------------------------

fn node(id: &str, channels: u16, gain_db: f32, muted: bool) -> Node {
    Node {
        id: id.into(),
        name: id.into(),
        channels,
        gain_db,
        muted,
    }
}
fn via(source: &str, src_ch: u16, to: &str, out_ch: u16) -> Wire {
    Wire {
        to: Some(to.into()),
        ..Wire::new(source, src_ch, out_ch)
    }
}
fn two_ch() -> (VirtualDevice, IoLayout) {
    (dev(), layout())
}
fn signal(n: usize) -> Vec<Vec<f32>> {
    (0..2)
        .map(|c| {
            (0..n)
                .map(|i| ((i * 37 + c * 11) % 199) as f32 / 99.5 - 1.0)
                .collect()
        })
        .collect()
}
fn bits(o: &[Vec<f32>]) -> Vec<Vec<u32>> {
    o.iter()
        .map(|r| r.iter().map(|x| x.to_bits()).collect())
        .collect()
}

/// A bus with no effects, 0 dB wires and a 0 dB fader is the direct wire, bit for bit.
#[test]
fn a_neutral_bus_is_bit_identical_to_the_direct_wire() {
    let (direct, l) = two_ch();
    let mut bus = dev();
    bus.nodes = vec![node("n", 2, 0.0, false)];
    bus.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 1, "n", 1),
        Wire::new("n", 0, 0),
        Wire::new("n", 1, 1),
    ];
    let ins = signal(300);
    let (mut a, _h1, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&direct, &l, 48_000.0));
        (m, h, meters)
    };
    let (mut b, _h2, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&bus, &l, 48_000.0));
        (m, h, meters)
    };
    for n in [300, 64, 1, 4096] {
        let ins: Vec<Vec<f32>> = ins
            .iter()
            .map(|v| v.iter().cycle().take(n).copied().collect())
            .collect();
        assert_eq!(
            bits(&run(&mut a, &ins, 4, n)),
            bits(&run(&mut b, &ins, 4, n)),
            "n={n}"
        );
    }
}

/// Several sources into one bus channel sum in tap order and equal the same sources wired straight to the
/// output, bit for bit, with source volumes that are not unity.
#[test]
fn a_neutral_bus_summing_several_sources_matches_the_direct_sum() {
    let srcs = ["a", "b", "c"];
    let mut direct = dev();
    direct.sources = vec![
        src("a", 2, -3.3, false),
        src("b", 2, 2.7, false),
        src("c", 2, 0.0, false),
    ];
    let mut bus = direct.clone();
    direct.wires = srcs
        .iter()
        .flat_map(|s| [Wire::new(*s, 0, 0), Wire::new(*s, 1, 1)])
        .collect();
    bus.nodes = vec![node("n", 2, 0.0, false)];
    bus.wires = srcs
        .iter()
        .flat_map(|s| [via(s, 0, "n", 0), via(s, 1, "n", 1)])
        .chain([Wire::new("n", 0, 0), Wire::new("n", 1, 1)])
        .collect();
    let l = IoLayout {
        inputs: srcs
            .iter()
            .flat_map(|s| [Some((s.to_string(), 0)), Some((s.to_string(), 1))])
            .collect(),
        outputs: vec![Some(OutPort::Device(0)), Some(OutPort::Device(1))],
    };
    let ins: Vec<Vec<f32>> = (0..6)
        .map(|c| {
            (0..300)
                .map(|i| ((i * 37 + c * 11) % 199) as f32 / 99.5 - 1.0)
                .collect()
        })
        .collect();
    let mk = |d: &VirtualDevice| {
        let (m, mut h) = mixer(Arc::new(Meters::new()));
        h.set_plan(MixPlan::compile(d, &l, 48_000.0));
        (m, h)
    };
    let ((mut a, _ha), (mut b, _hb)) = (mk(&direct), mk(&bus));
    assert_eq!(
        bits(&run(&mut a, &ins, 2, 300)),
        bits(&run(&mut b, &ins, 2, 300))
    );
}

/// Node order in the config is the reverse of the signal order: n2 -> n1 -> n0 -> out.
#[test]
fn nodes_run_in_signal_order_not_config_order() {
    let mut d = dev();
    d.nodes = vec![
        node("n0", 2, -6.0, false),
        node("n1", 2, 0.0, false),
        node("n2", 2, 6.0, false),
    ];
    d.wires = vec![
        Wire::new("n0", 0, 0),
        via("n1", 0, "n0", 0),
        via("n2", 0, "n1", 0),
        via("s1", 0, "n2", 0),
    ];
    let (mut m, _h, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        (m, h, meters)
    };
    let o = run(&mut m, &[vec![0.1; 64], vec![0.0; 64]], 4, 64);
    // +6 dB and -6 dB cancel (both 10^(+-6/20)): 0.1 comes out in the first block, nothing a block late.
    assert!((o[0][0] - 0.1).abs() < 1e-6, "{}", o[0][0]);
    assert!(o[1].iter().all(|x| *x == 0.0));
}

#[test]
fn a_node_fans_out_and_fans_in() {
    let mut d = dev();
    d.nodes = vec![node("a", 1, 0.0, false), node("b", 1, -6.0206, false)];
    d.wires = vec![
        via("s1", 0, "a", 0),
        via("s1", 1, "a", 0), // both source channels into one row: they sum
        Wire::new("a", 0, 0),
        via("a", 0, "b", 0), // a also feeds b, which goes to output 1
        Wire::new("b", 0, 1),
    ];
    let (mut m, _h, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        (m, h, meters)
    };
    let o = run(&mut m, &[vec![0.5; 32], vec![0.25; 32]], 4, 32);
    assert!((o[0][31] - 0.75).abs() < 1e-6);
    assert!((o[1][31] - 0.375).abs() < 1e-4, "{}", o[1][31]);
}

#[test]
fn a_muted_node_is_silent_and_unmutes_with_a_ramp() {
    let mut d = dev();
    d.nodes = vec![node("n", 2, 0.0, true)];
    d.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 1, "n", 1),
        Wire::new("n", 0, 0),
        Wire::new("n", 1, 1),
    ];
    let meters = Arc::new(Meters::new());
    let (mut m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let ins = vec![vec![1.0; 64], vec![1.0; 64]];
    assert!(run(&mut m, &ins, 4, 64).iter().flatten().all(|x| *x == 0.0));
    let r = meters.take(&layout(), &d);
    assert_eq!(r.nodes, [("n".to_string(), vec![0.0, 0.0])]);
    d.nodes[0].muted = false;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, 64);
    // 64 of 480 frames: a ramp, ending a seventh of the way up, not a step to unity.
    assert!(
        o[0][0] < 0.01 && (o[0][63] - 64.0 / 480.0).abs() < 1e-4,
        "{} {}",
        o[0][0],
        o[0][63]
    );
    for _ in 0..8 {
        run(&mut m, &ins, 4, 64);
    }
    assert_eq!(run(&mut m, &ins, 4, 64)[0][63], 1.0);
}

#[test]
fn node_meters_read_post_fader_peaks_by_channel_in_config_order() {
    let mut d = dev();
    d.channels = 4;
    d.sources = vec![src("s1", 2, 0.0, false)];
    d.nodes = vec![node("a", 1, 0.0, false), node("b", 2, -6.0206, false)];
    d.wires = vec![
        via("s1", 0, "a", 0),
        Wire::new("a", 0, 0),
        via("s1", 0, "b", 0),
        via("s1", 1, "b", 1),
        Wire::new("b", 0, 1),
    ];
    let l = IoLayout {
        inputs: vec![Some(("s1".into(), 0)), Some(("s1".into(), 1))],
        outputs: (0..4).map(|c| Some(OutPort::Device(c))).collect(),
    };
    let meters = Arc::new(Meters::new());
    let (mut m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
    run(&mut m, &[vec![0.8; 64], vec![0.4; 64]], 4, 64);
    let r = meters.take(&l, &d);
    assert_eq!(r.nodes.len(), 2);
    assert_eq!(r.nodes[0], ("a".to_string(), vec![0.8]));
    let b = &r.nodes[1].1;
    assert!(
        (b[0] - 0.4).abs() < 1e-4 && (b[1] - 0.2).abs() < 1e-4,
        "{b:?}"
    );
    assert!(
        meters
            .take(&l, &d)
            .nodes
            .iter()
            .all(|(_, p)| p.iter().all(|x| *x == 0.0)),
        "take resets"
    );
}

/// `Config::validate` refuses a loop; `MixPlan::compile` must still not hang or panic on one, and the
/// acyclic part keeps playing.
#[test]
fn a_loop_of_nodes_is_rejected_by_validate_and_survived_by_compile() {
    let mut d = dev();
    d.nodes = vec![node("a", 2, 0.0, false), node("b", 2, 0.0, false)];
    d.wires = vec![
        via("s1", 0, "a", 0),
        via("a", 0, "b", 0),
        via("b", 0, "a", 0), // closes the loop
        Wire::new("b", 0, 0),
        via("a", 1, "a", 1), // a self wire
        Wire::new("a", 1, 1),
    ];
    let cfg = Config {
        devices: vec![d.clone()],
        ..Config::default()
    };
    let e = cfg.validate().unwrap_err();
    assert!(
        e.contains("loop") && (e.contains("'a'") || e.contains("'b'")),
        "{e}"
    );
    let mut c1 = cfg.clone();
    c1.devices[0].wires.remove(2);
    assert!(
        c1.validate().unwrap_err().contains("self") || c1.validate().unwrap_err().contains("loop")
    );
    let (mut m, _h, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        (m, h, meters)
    };
    let o = run(&mut m, &[vec![0.5; 64], vec![0.5; 64]], 4, 64);
    assert!(o.iter().flatten().all(|x| x.is_finite()));
    assert!(
        (o[0][63] - 0.5).abs() < 1e-6,
        "the forward edges still play: {}",
        o[0][63]
    );
    assert!(o[1].iter().all(|x| *x == 0.0), "the self wire is dropped");
}

/// A hand-built plan with a bad node never panics: duplicate ids, a wire to a node that does not exist or
/// to a source, rows beyond the node, and counts beyond the caps.
#[test]
fn compile_survives_what_validate_would_refuse() {
    let mut d = dev();
    d.nodes = vec![
        node("n", 2, 0.0, false),
        node("n", 2, 0.0, false),  // duplicate id
        node("s1", 2, 0.0, false), // a source's id
        node("z", 0, 0.0, false),  // no channels
        node("w", 200, 0.0, false),
    ];
    d.nodes
        .extend((0..30).map(|i| node(&format!("x{i}"), 8, 0.0, false)));
    d.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 0, "n", 5),     // row beyond the node
        via("s1", 0, "ghost", 0), // no such node
        via("s1", 0, "s1", 0),    // a source, not a node
        via("ghost", 0, "n", 0),
        via("n", 9, "n", 0),
        Wire::new("n", 0, 0),
        Wire::new("n", 0, 99), // beyond the device
    ];
    let (mut m, _h, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        (m, h, meters)
    };
    let o = run(&mut m, &[vec![0.5; 64], vec![0.0; 64]], 4, 64);
    assert!((o[0][63] - 0.5).abs() < 1e-6);
    assert!(o[1..].iter().flatten().all(|x| *x == 0.0));
}

fn node_graph_dev() -> VirtualDevice {
    let mut d = dev();
    d.nodes = vec![node("a", 2, 24.0, false), node("b", 2, 24.0, false)];
    d.wires = vec![
        Wire {
            gain_db: 24.0,
            ..via("s1", 0, "a", 0)
        },
        Wire {
            gain_db: 24.0,
            ..via("s1", 1, "a", 1)
        },
        via("a", 0, "b", 0),
        via("a", 1, "b", 1),
        Wire::new("b", 0, 0),
        Wire::new("b", 1, 1),
    ];
    d
}

/// NaN, inf and huge samples through every hop: the output stays finite, and the next clean block is
/// exactly what a fresh mixer says (nothing is remembered between blocks).
#[test]
fn hostile_samples_through_nodes_stay_finite_and_recover_in_one_block() {
    let d = node_graph_dev();
    let l = layout();
    let mk = || {
        let (m, mut h) = mixer(Arc::new(Meters::new()));
        h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
        (m, h)
    };
    let clean = signal(64);
    let (mut fresh, _hf) = mk();
    let want = run(&mut fresh, &clean, 4, 64);
    for bad in [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1e38,
        -1e38,
        f32::MAX,
    ] {
        let (mut m, _h) = mk();
        for burst in 0..3 {
            let hostile = vec![vec![bad; 64], vec![bad; 64]];
            let o = run(&mut m, &hostile, 4, 64);
            assert!(
                o.iter().flatten().all(|x| x.is_finite()),
                "{bad} burst {burst}"
            );
        }
        assert_eq!(bits(&run(&mut m, &clean, 4, 64)), bits(&want), "{bad}");
    }
}

#[test]
fn a_swap_that_adds_a_node_crossfades_and_takes_no_step() {
    let (mut m, mut h, _) = {
        let meters = Arc::new(Meters::new());
        let (m, mut h) = mixer(meters.clone());
        h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
        (m, h, meters)
    };
    let n = 64;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    // The same level through a node that attenuates by 6 dB: a different route, so a crossfade.
    let mut d = dev();
    d.nodes = vec![node("n", 2, -6.0206, false)];
    d.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 1, "n", 1),
        Wire::new("n", 0, 0),
        Wire::new("n", 1, 1),
    ];
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, n);
    assert!(o[0][0] > 0.95, "starts on the old routing: {}", o[0][0]);
    assert!(
        (o[0][n - 1] - 0.5).abs() < 0.02,
        "ends on the new: {}",
        o[0][n - 1]
    );
    assert!(
        o[0].windows(2)
            .all(|w| (w[0] - w[1]).abs() <= 1.5 / n as f32)
    );
}

/// A fader move on an existing node is a plan with the same route: it ramps, no crossfade.
#[test]
fn a_fader_edit_ramps_inside_one_plan() {
    let mut d = dev();
    d.nodes = vec![node("n", 2, 0.0, false)];
    d.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 1, "n", 1),
        Wire::new("n", 0, 0),
        Wire::new("n", 1, 1),
    ];
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let ins = vec![vec![1.0; 64], vec![1.0; 64]];
    run(&mut m, &ins, 4, 64);
    d.nodes[0].gain_db = -60.0;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, 64);
    assert!(o[0][0] < 1.0 && o[0][0] > 0.99, "{}", o[0][0]);
    assert!(
        (o[0][63] - (1.0 - 64.0 / 480.0)).abs() < 1e-4,
        "{}",
        o[0][63]
    );
}

// ---- schema and validate -------------------------------------------------------------------------------

fn cfg_of(d: VirtualDevice) -> Config {
    Config {
        devices: vec![d],
        ..Config::default()
    }
}
fn bus_dev() -> VirtualDevice {
    let mut d = dev();
    d.nodes = vec![node("n", 2, -3.0, false)];
    d.wires = vec![
        via("s1", 0, "n", 0),
        via("s1", 1, "n", 1),
        Wire::new("n", 0, 0),
        Wire::new("n", 1, 1),
    ];
    d
}

#[test]
fn nodes_round_trip_as_version_2_and_plain_configs_stay_version_1() {
    let plain = cfg_of(dev());
    assert!(
        serde_json::to_string(&plain)
            .unwrap()
            .starts_with(r#"{"version":1,"#)
    );
    assert!(!serde_json::to_string(&plain).unwrap().contains("nodes"));
    let cfg = cfg_of(bus_dev());
    cfg.validate().unwrap();
    let json = serde_json::to_string(&cfg).unwrap();
    assert!(json.starts_with(r#"{"version":2,"#), "{json}");
    assert!(
        json.contains(
            r#""nodes":[{"id":"n","name":"n","channels":2,"gain_db":-3.0,"muted":false}]"#
        )
    );
    assert!(json.contains(r#"{"source":"s1","src_ch":0,"out_ch":0,"to":"n"}"#));
    let mut back: Config = serde_json::from_str(&json).unwrap();
    assert_eq!(back.devices, cfg.devices);
    back.validate().unwrap();
    // A node alone (no wire to it) is already version 2.
    let mut lone = dev();
    lone.nodes = vec![node("n", 1, 0.0, false)];
    assert_eq!(cfg_of(lone).written_version(), 2);
    // A v1-labelled file with nodes still loads.
    back.version = 1;
    let relabelled = json.replacen(r#""version":2"#, r#""version":1"#, 1);
    assert_eq!(
        serde_json::from_str::<Config>(&relabelled).unwrap().devices,
        cfg.devices
    );
}

#[test]
fn validate_rejects_bad_nodes_and_wires() {
    type Mutate = fn(&mut VirtualDevice);
    let bad: [(&str, Mutate); 16] = [
        ("dup node id", |d| d.nodes.push(d.nodes[0].clone())),
        ("node id of a source", |d| d.nodes[0].id = "s1".into()),
        ("empty node id", |d| d.nodes[0].id.clear()),
        ("long node name", |d| d.nodes[0].name = "x".repeat(65)),
        ("zero channels", |d| d.nodes[0].channels = 0),
        ("nine channels", |d| d.nodes[0].channels = 9),
        ("loud fader", |d| d.nodes[0].gain_db = 25.0),
        ("nan fader", |d| d.nodes[0].gain_db = f32::NAN),
        ("17 nodes", |d| {
            d.nodes = (0..17)
                .map(|i| node(&format!("x{i}"), 1, 0.0, false))
                .collect();
        }),
        ("33 node channels", |d| {
            d.nodes = (0..5)
                .map(|i| node(&format!("x{i}"), 8, 0.0, false))
                .collect();
        }),
        ("wire to unknown node", |d| {
            d.wires[0].to = Some("ghost".into())
        }),
        ("wire to a source", |d| d.wires[0].to = Some("s1".into())),
        ("node input out of range", |d| d.wires[0].out_ch = 2),
        ("node output out of range", |d| d.wires[2].src_ch = 2),
        ("self wire", |d| d.wires.push(via("n", 0, "n", 0))),
        ("loop", |d| {
            d.nodes.push(node("m", 2, 0.0, false));
            d.wires.extend([via("n", 0, "m", 0), via("m", 0, "n", 1)]);
        }),
    ];
    for (name, f) in bad {
        let mut d = bus_dev();
        f(&mut d);
        assert!(cfg_of(d).validate().is_err(), "{name} accepted");
    }
    // Rows past the node are fine for the device (it has 2 channels) only when the wire is not into the node.
    let mut d = bus_dev();
    d.channels = 4;
    d.wires.push(Wire::new("n", 1, 3)); // node output 1 onto device channel 3
    cfg_of(d).validate().unwrap();
    let mut d = bus_dev();
    d.wires.push(Wire::new("s1", 0, 2)); // device has only 2 channels
    assert!(cfg_of(d).validate().is_err());
    // 16 nodes, 32 channels in total is the cap, exactly.
    let mut d = dev();
    d.nodes = (0..16)
        .map(|i| node(&format!("x{i}"), 2, 0.0, false))
        .collect();
    cfg_of(d).validate().unwrap();
}

#[test]
fn presets_carry_nodes() {
    use sr_core::preset::PresetFile;
    let mut f = PresetFile::default();
    f.save_as("Bus", &cfg_of(bus_dev())).unwrap();
    assert_eq!(f.presets[0].devices[0].nodes.len(), 1);
    assert_eq!(f.written_version(), 2);
    let back: PresetFile = serde_json::from_str(&serde_json::to_string(&f).unwrap()).unwrap();
    assert_eq!(back.presets, f.presets);
}
