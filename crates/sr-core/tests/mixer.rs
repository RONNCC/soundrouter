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

fn ones(n: usize) -> Vec<Vec<f32>> {
    vec![vec![1.0; n], vec![0.5; n]]
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn no_plan_is_silence() {
    let (mut m, _h) = mixer(Arc::new(Meters::new()));
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(o.iter().flatten().all(|x| *x == 0.0));
}

#[test]
fn unity_wire() {
    let (mut m, _h, _) = setup(&dev(), &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(o[0].iter().all(|x| *x == 1.0));
    assert!(o[1].iter().all(|x| *x == 0.5));
    assert!(o[2].iter().all(|x| *x == 0.0), "no monitor configured");
}

#[test]
fn minus_6_db() {
    let mut d = dev();
    d.sources[0].volume_db = -6.0;
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!((o[0][0] - 0.501).abs() < 1e-3, "{}", o[0][0]);
}

#[test]
fn source_mute_and_device_mute() {
    let mut d = dev();
    d.sources[0].muted = true;
    let (mut m, _h, _) = setup(&d, &layout());
    assert!(
        run(&mut m, &ones(16), 4, 16)
            .iter()
            .flatten()
            .all(|x| *x == 0.0)
    );
    let mut d = dev();
    d.muted = true;
    let (mut m, _h, _) = setup(&d, &layout());
    assert!(
        run(&mut m, &ones(16), 4, 16)
            .iter()
            .flatten()
            .all(|x| *x == 0.0)
    );
}

#[test]
fn device_volume() {
    let mut d = dev();
    d.volume_db = -6.0;
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!((o[0][15] - 0.501).abs() < 1e-3);
}

#[test]
fn fan_out_and_sum() {
    let mut d = dev();
    d.sources.push(src("s2", 1, 0.0, false));
    d.wires = vec![
        Wire {
            source: "s1".into(),
            src_ch: 0,
            out_ch: 0,
        },
        Wire {
            source: "s2".into(),
            src_ch: 0,
            out_ch: 0,
        },
        Wire {
            source: "s2".into(),
            src_ch: 0,
            out_ch: 1,
        },
    ];
    let mut l = layout();
    l.inputs.push(Some(("s2".into(), 0)));
    let (mut m, _h, _) = setup(&d, &l);
    let ins = vec![vec![1.0; 8], vec![7.0; 8], vec![0.25; 8]];
    let o = run(&mut m, &ins, 4, 8);
    assert!(o[0].iter().all(|x| *x == 1.25));
    assert!(o[1].iter().all(|x| *x == 0.25));
}

#[test]
fn monitor_map_and_volume() {
    let mut d = dev();
    let mut mo = mon("spk", true, -6.0);
    mo.wires = vec![
        MonitorWire {
            out_ch: 0,
            dst_ch: 1,
        },
        MonitorWire {
            out_ch: 1,
            dst_ch: 0,
        },
    ];
    d.monitors.push(mo);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(
        o[0].iter().all(|x| *x == 1.0),
        "vdev out unaffected by monitor"
    );
    assert!(
        (o[2][0] - 0.5 * 0.501).abs() < 1e-3,
        "swapped: vdev ch1 -> spk ch0"
    );
    assert!((o[3][0] - 0.501).abs() < 1e-3);
}

#[test]
fn disabled_monitor_silent() {
    let mut d = dev();
    d.monitors.push(mon("spk", false, 0.0));
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(o[2].iter().chain(&o[3]).all(|x| *x == 0.0));
    assert!(o[0].iter().all(|x| *x == 1.0));
}

#[test]
fn monitor_for_other_device_ignored() {
    let mut d = dev();
    d.monitors.push(mon("elsewhere", true, 0.0));
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(o[2].iter().chain(&o[3]).all(|x| *x == 0.0));
}

#[test]
fn none_slots_ignored() {
    let mut l = layout();
    l.inputs.insert(0, None); // e.g. vdev input
    l.outputs.push(None);
    let (mut m, _h, _) = setup(&dev(), &l);
    let ins = vec![vec![99.0; 8], vec![1.0; 8], vec![0.5; 8]];
    let o = run(&mut m, &ins, 5, 8);
    assert!(o[0].iter().all(|x| *x == 1.0));
    assert!(o[1].iter().all(|x| *x == 0.5));
    assert!(o[4].iter().all(|x| *x == 0.0));
}

#[test]
fn plan_swap_applies_next_block() {
    let (mut m, mut h, _) = setup(&dev(), &layout());
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(o[0].iter().all(|x| *x == 1.0));
    let mut d = dev();
    d.wires = vec![Wire {
        source: "s1".into(),
        src_ch: 1,
        out_ch: 0,
    }]; // different routing
    h.set_plan(MixPlan::compile(&d, &layout()));
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(o[0].iter().all(|x| *x == 0.5));
    assert!(o[1].iter().all(|x| *x == 0.0));
    h.flush(); // old plans come back and drop on the control side
}

#[test]
fn many_swaps_and_full_queue() {
    let (mut m, mut h, _) = setup(&dev(), &layout());
    for i in 0..20 {
        let mut d = dev();
        d.sources[0].volume_db = -(i as f32);
        h.set_plan(MixPlan::compile(&d, &layout())); // audio thread never runs: queue fills
    }
    let mut d = dev();
    d.sources[0].muted = true;
    h.set_plan(MixPlan::compile(&d, &layout())); // newest wins even when queue was full
    h.flush();
    for _ in 0..8 {
        h.flush();
        run(&mut m, &ones(8), 4, 8); // each block drains up to 4 queued plans
    }
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(o[0].iter().all(|x| *x == 0.0), "{:?}", o[0]);
}

#[test]
fn ramp_has_no_step() {
    let (mut m, mut h, _) = setup(&dev(), &layout());
    let n = 64;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    let mut d = dev();
    d.sources[0].muted = true;
    h.set_plan(MixPlan::compile(&d, &layout()));
    let o = run(&mut m, &ins, 4, n);
    let step = 1.0 / n as f32;
    let mut prev = 1.0;
    for x in &o[0] {
        assert!(prev - x <= step + 1e-5 && prev >= *x, "jump {prev} -> {x}");
        prev = *x;
    }
    assert!(o[0][n - 1].abs() < 1e-5);
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 0.0));
    // unmute: ramps 0 -> 1
    h.set_plan(MixPlan::compile(&dev(), &layout()));
    let o = run(&mut m, &ins, 4, n);
    assert!(o[0][0] <= step + 1e-5 && close(o[0][n - 1], 1.0));
}

#[test]
fn monitor_enable_ramps() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, 0.0));
    let (mut m, mut h, _) = setup(&d, &layout());
    let ins = vec![vec![1.0; 32], vec![1.0; 32]];
    run(&mut m, &ins, 4, 32);
    d.monitors[0].enabled = false;
    h.set_plan(MixPlan::compile(&d, &layout()));
    let o = run(&mut m, &ins, 4, 32);
    assert!(o[2][0] > 0.9 && o[2][31].abs() < 1e-5);
}

#[test]
fn chunks_over_max_frames() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, 0.0));
    let (mut m, _h, meters) = setup(&d, &layout());
    let n = MAX_FRAMES * 2 + 123;
    let a: Vec<f32> = (0..n).map(|i| (i % 100) as f32 / 100.0).collect();
    let o = run(&mut m, &[a.clone(), a.clone()], 4, n);
    assert_eq!(o[0], a);
    assert_eq!(o[3], a);
    assert_eq!(meters.mismatch_count(), 0);
}

#[test]
fn mismatch_is_silence_and_counted() {
    let (mut m, _h, meters) = setup(&dev(), &layout());
    let o = run(&mut m, &ones(8)[..1], 4, 8); // one input too few
    assert!(o.iter().flatten().all(|x| *x == 0.0));
    assert_eq!(meters.mismatch_count(), 1);
    let o = run(&mut m, &ones(8), 3, 8); // one output too few
    assert!(o.iter().flatten().all(|x| *x == 0.0));
    assert_eq!(meters.mismatch_count(), 2);
    // short buffer (frames > slice len) also counts and does not panic
    let ins = ones(4);
    let ins: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut outs = vec![vec![1.0f32; 8]; 4];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    m.process(&ins, &mut refs, 8);
    assert_eq!(meters.mismatch_count(), 3);
    assert!(outs.iter().flatten().all(|x| *x == 0.0));
    // recovers
    assert!(run(&mut m, &ones(8), 4, 8)[0].iter().all(|x| *x == 1.0));
    assert_eq!(meters.mismatch_count(), 3);
    // zero frames: no-op
    run(&mut m, &ones(0), 4, 0);
}

#[test]
fn meters_peak_and_take_reset() {
    let mut d = dev();
    d.sources.push(src("idle", 2, 0.0, false));
    d.volume_db = -6.0;
    let l = layout();
    let (mut m, _h, meters) = setup(&d, &l);
    let mut a = vec![0.0; 16];
    a[5] = -0.8;
    a[9] = 0.4;
    run(&mut m, &[a, vec![0.25; 16]], 4, 16);
    let r = meters.take(&l, &d);
    assert_eq!(r.sources[0].0, "s1");
    assert_eq!(r.sources[0].1, vec![0.8, 0.25]);
    assert_eq!(r.sources[1].1, vec![0.0, 0.0]);
    assert!((r.channels[0] - 0.8 * 0.501).abs() < 1e-3, "post gain");
    assert_eq!(r.channels.len(), 2);
    let r = meters.take(&l, &d);
    assert!(r.sources[0].1.iter().chain(&r.channels).all(|x| *x == 0.0));
}

#[test]
fn meters_hold_max_across_blocks_and_ignore_nan() {
    let (mut m, _h, meters) = setup(&dev(), &layout());
    run(&mut m, &[vec![0.9; 4], vec![0.0; 4]], 4, 4);
    run(&mut m, &[vec![f32::NAN; 4], vec![0.1; 4]], 4, 4);
    let r = meters.take(&layout(), &dev());
    assert_eq!(r.sources[0].1[0], 0.9);
    assert_eq!(r.sources[0].1[1], 0.1);
}
