//! A plan swap must not make the audio thread fault pages in. The scratch a plan mixes in is fresh zeroed
//! memory, which the kernel maps on first write: with the plan built on the control thread and first used
//! by the audio thread, every swap of a wide device cost it up to 64 page faults.
//!
//! This is the only test in its binary so that nothing else in the process faults meanwhile.
mod common;
use common::*;
use sr_core::config::{Fx, Node, Wire};
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

fn minor_faults() -> i64 {
    // SAFETY: getrusage fills a plain-data struct.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_minflt as i64
}

#[test]
fn a_plan_swap_does_not_fault_pages_in_on_the_audio_thread() {
    let mut faults = 0;
    // Every width from 33 up has a different scratch size (>= 528 KiB), so none is served from a recycled block.
    for n in 33..=64usize {
        let mut d = dev();
        d.channels = n as u16;
        d.sources = vec![src("s1", n as u16, 0.0, false)];
        d.wires = (0..n as u16).map(|i| Wire::new("s1", i, i)).collect();
        let l = IoLayout {
            inputs: (0..n as u16).map(|c| Some(("s1".into(), c))).collect(),
            outputs: (0..n as u16).map(|c| Some(OutPort::Device(c))).collect(),
        };
        let ins = vec![vec![0.1f32; 64]; n];
        let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
        let mut outs = vec![vec![0.0f32; 64]; n];
        let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
        let (mut m, mut h) = mixer(Arc::new(Meters::new()));
        for _ in 0..3 {
            h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
            let before = minor_faults();
            m.process(&ins, &mut refs, 64);
            faults += minor_faults() - before;
        }
        // The same device through 32 node channels (the cap): their scratch is as big as the vdev's.
        let mut nodes = d.clone();
        nodes.nodes = (0..4)
            .map(|i| Node {
                id: format!("n{i}"),
                name: String::new(),
                channels: 8,
                gain_db: 0.0,
                muted: false,
                fx: vec![Fx::Delay { ms: 20.0 }],
            })
            .collect();
        nodes.wires = (0..n as u16)
            .flat_map(|i| {
                let node = format!("n{}", usize::from(i) % 4);
                let row = i / 4 % 8;
                [
                    Wire {
                        to: Some(node.clone()),
                        ..Wire::new("s1", i, row)
                    },
                    Wire::new(node, row, i),
                ]
            })
            .collect();
        for _ in 0..3 {
            h.set_plan(MixPlan::compile(&nodes, &l, 48_000.0));
            let before = minor_faults();
            m.process(&ins, &mut refs, 64);
            faults += minor_faults() - before;
        }
    }
    assert_eq!(
        faults, 0,
        "the audio thread faulted pages in after plan swaps"
    );
}
