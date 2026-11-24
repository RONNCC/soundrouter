//! Not a pass/fail test: prints the ns cost of one `Mixer::process` call.
//! `cargo test --release -p sr-core --test bench_mix -- --ignored --nocapture`
mod common;
use common::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

/// `ch` channels in, `ch` out to the vdev and `ch` to one monitor; identity wires.
fn wide(ch: u16) -> (VirtualDevice, IoLayout) {
    let mut d = dev();
    d.channels = ch;
    d.sources = vec![src("s1", ch, 0.0, false)];
    d.wires = (0..ch).map(|i| Wire::new("s1", i, i)).collect();
    let mut m = mon("spk", true, -3.0);
    m.wires = (0..ch)
        .map(|i| MonitorWire {
            out_ch: i,
            dst_ch: i,
        })
        .collect();
    d.monitors = vec![m];
    let l = IoLayout {
        inputs: (0..ch).map(|i| Some(("s1".into(), i))).collect(),
        outputs: (0..ch)
            .map(|i| Some(OutPort::Device(i)))
            .chain((0..ch).map(|i| {
                Some(OutPort::Monitor {
                    uid: "spk".into(),
                    ch: i,
                })
            }))
            .collect(),
    };
    (d, l)
}

/// 32 channels in and out like `wide(32)`, but every channel pair goes through its own stereo bus:
/// 16 nodes, 32 node channels (the cap), one tap in and one out per channel.
fn buses16() -> (VirtualDevice, IoLayout) {
    let (mut d, l) = wide(32);
    d.monitors.clear();
    d.nodes = (0..16)
        .map(|i| Node {
            id: format!("n{i}"),
            name: String::new(),
            channels: 2,
            gain_db: -1.0,
            muted: false,
            fx: Vec::new(),
        })
        .collect();
    d.wires = (0..32u16)
        .flat_map(|c| {
            let node = format!("n{}", c / 2);
            [
                Wire {
                    to: Some(node.clone()),
                    ..Wire::new("s1", c, c % 2)
                },
                Wire::new(node, c % 2, c),
            ]
        })
        .collect();
    (d, l)
}

/// `buses16` with the effects of the spec's cost gate: every node has mono, an EQ and a limiter, and four
/// of them (the delay-line budget, 8 channels) a 5 ms delay as well.
fn buses16_fx() -> (VirtualDevice, IoLayout) {
    let (mut d, l) = buses16();
    for (i, n) in d.nodes.iter_mut().enumerate() {
        n.fx = vec![
            Fx::Mono,
            Fx::Eq {
                low_hz: 120.0,
                low_db: 3.0,
                high_hz: 8000.0,
                high_db: -3.0,
            },
            Fx::Limiter {
                ceiling_db: -1.0,
                knee_db: 6.0,
            },
        ];
        if i < 4 {
            n.fx.push(Fx::Delay { ms: 5.0 });
        }
    }
    (d, l)
}

fn ns_per_call(d: &VirtualDevice, l: &IoLayout, frames: usize) -> f64 {
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(d, l, 48_000.0));
    let ins: Vec<Vec<f32>> = (0..l.inputs.len())
        .map(|c| {
            (0..frames)
                .map(|i| ((i * 7 + c * 13) % 101) as f32 / 101.0 - 0.5)
                .collect()
        })
        .collect();
    let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut outs = vec![vec![0.0f32; frames]; l.outputs.len()];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    for _ in 0..2000 {
        m.process(&ins, &mut refs, frames);
    }
    let iters = 20_000;
    // Best of 5 rounds: the minimum is the least scheduler-noisy estimate.
    (0..5)
        .map(|_| {
            let t = Instant::now();
            for _ in 0..iters {
                m.process(black_box(&ins), &mut refs, frames);
            }
            t.elapsed().as_nanos() as f64 / iters as f64
        })
        .fold(f64::MAX, f64::min)
}

#[test]
#[ignore = "benchmark"]
fn bench_mix() {
    let (d2, l2) = (dev(), layout());
    let (d64, l64) = wide(64);
    let (d32, l32) = wide(32);
    let (db, lb) = buses16();
    let (dx, lx) = buses16_fx();
    for frames in [64, 128, 256] {
        println!(
            "frames {frames:>3}: stereo {:>8.0} ns   64ch {:>8.0} ns   32ch {:>8.0} ns   16 buses (32 node ch) {:>8.0} ns   16 buses + all effects {:>8.0} ns",
            ns_per_call(&d2, &l2, frames),
            ns_per_call(&d64, &l64, frames),
            ns_per_call(&d32, &l32, frames),
            ns_per_call(&db, &lb, frames),
            ns_per_call(&dx, &lx, frames)
        );
    }
}
