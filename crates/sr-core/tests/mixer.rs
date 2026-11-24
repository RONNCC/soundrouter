mod common;
use common::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

fn setup(d: &VirtualDevice, l: &IoLayout) -> (Mixer, MixerHandle, Arc<Meters>) {
    let meters = Arc::new(Meters::new());
    let (m, mut h) = mixer(meters.clone());
    h.set_plan(MixPlan::compile(d, l, 48_000.0));
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
        Wire::new("s1", 0, 0),
        Wire::new("s2", 0, 0),
        Wire::new("s2", 0, 1),
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
    d.wires = vec![Wire::new("s1", 1, 0)]; // different routing
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    // The new routing is crossfaded in over this block (a step would click) and is exact after it.
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(
        o[0][0] < 1.0 && o[0][0] > 0.5 && close(o[0][7], 0.5),
        "{:?}",
        o[0]
    );
    assert!(o[1][0] > 0.0 && close(o[1][7], 0.0), "{:?}", o[1]);
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
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0)); // audio thread never runs: queue fills
    }
    let mut d = dev();
    d.sources[0].muted = true;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0)); // newest wins even when queue was full
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
    let n = 512; // a block of at least RAMP_FRAMES (480) ramps across the whole block
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    let mut d = dev();
    d.sources[0].muted = true;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
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
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, n);
    assert!(o[0][0] <= step + 1e-5 && close(o[0][n - 1], 1.0));
}

#[test]
fn monitor_enable_ramps() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, 0.0));
    let (mut m, mut h, _) = setup(&d, &layout());
    let ins = vec![vec![1.0; 512], vec![1.0; 512]];
    run(&mut m, &ins, 4, 512);
    d.monitors[0].enabled = false;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, 512);
    assert!(o[2][0] > 0.9 && o[2][511].abs() < 1e-5);
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

#[test]
fn newest_plan_wins_after_audio_thread_was_away() {
    // Audio thread not consuming (TCC prompt, shape mismatch): plans pile up, the newest must win.
    let meters = Arc::new(Meters::new());
    let (mut m, mut h) = mixer(meters);
    for db in [0.0, -6.0, -12.0, -18.0, -24.0, -30.0] {
        let mut d = dev();
        d.sources[0].volume_db = db;
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    }
    let n = 512;
    let o = run(&mut m, &vec![vec![1.0; n]; 2], 4, n);
    assert!(
        close(o[0][n - 1], 10f32.powf(-30.0 / 20.0)),
        "{}",
        o[0][n - 1]
    );
    // ... and a plan published while the old ones are still outstanding replaces them too
    let mut d = dev();
    d.sources[0].muted = true;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    run(&mut m, &vec![vec![1.0; n]; 2], 4, n);
    let o = run(&mut m, &vec![vec![1.0; n]; 2], 4, n);
    assert!(o[0].iter().all(|x| *x == 1.0));
}

#[test]
fn dropping_either_side_with_plans_pending_is_clean() {
    let (m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    drop(m);
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    h.flush();
    drop(h);
    let (m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&dev(), &layout(), 48_000.0));
    drop(h);
    drop(m);
}

/// `cargo test -p sr-core --release -- --ignored`: sanitizing non-finite samples must stay cheap.
#[test]
#[ignore = "timing"]
fn process_is_far_below_realtime_budget() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, -3.0));
    let (mut m, _h, _) = setup(&d, &layout());
    let n = 512;
    let ins: Vec<Vec<f32>> = vec![vec![0.25; n]; 2];
    run(&mut m, &ins, 4, n);
    let t = std::time::Instant::now();
    for _ in 0..2000 {
        run(&mut m, &ins, 4, n);
    }
    let per_block = t.elapsed() / 2000;
    // 512 frames at 48 kHz = 10.6 ms of audio; demand < 1% of it (the harness allocates too).
    assert!(
        per_block < std::time::Duration::from_micros(100),
        "{per_block:?}"
    );
}

#[test]
fn device_mute_ramps_without_a_step_and_holds_across_blocks() {
    let (mut m, mut h, _) = setup(&dev(), &layout());
    let n = 512;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    let mut d = dev();
    d.muted = true;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let step = 1.0 / n as f32;
    let mut prev = 1.0;
    for x in &run(&mut m, &ins, 4, n)[0] {
        assert!(prev - x <= step + 1e-5 && prev >= *x, "jump {prev} -> {x}");
        prev = *x;
    }
    assert!(prev.abs() < 1e-5, "ramp must land on the target");
    // The ramp is finished: later blocks are exactly silent, not ramping again from 1.
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 0.0));
    // A plan that keeps the device muted continues from the applied gain: no jump back to 1 and down again.
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 0.0));
}

#[test]
fn a_plan_swap_continues_the_device_ramp_from_what_was_applied() {
    let mut d = dev();
    d.muted = true;
    let (mut m, mut h, _) = setup(&d, &layout());
    let n = 512;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    d.muted = false;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, n);
    assert!(
        o[0][0] <= 1.0 / n as f32 + 1e-5,
        "ramp must start from silence, got {}",
        o[0][0]
    );
    assert!(close(o[0][n - 1], 1.0));
}

#[test]
fn a_long_ramp_spans_exactly_the_first_chunk() {
    let (mut m, mut h, _) = setup(&dev(), &layout());
    let n = MAX_FRAMES + 904;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    let mut d = dev();
    d.sources[0].volume_db = -200.0; // ~0
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, n);
    assert!(
        o[0][MAX_FRAMES - 2] > 1e-6,
        "the ramp must still be running one frame before the chunk end"
    );
    assert!(o[0][MAX_FRAMES - 1] < 1e-6 && o[0][MAX_FRAMES] < 1e-6);
}

#[test]
fn a_wire_to_a_channel_past_the_device_is_dropped_not_indexed() {
    let mut d = dev();
    d.wires.push(Wire::new("s1", 0, 2)); // the device has channels 0 and 1
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(8), 4, 8);
    assert!(o[0].iter().all(|x| close(*x, 1.0)) && o[1].iter().all(|x| close(*x, 0.5)));
}

fn app_dev() -> VirtualDevice {
    let mut d = dev();
    d.sources[0].kind = SourceKind::App {
        bundle_id: "com.example.app".into(),
    };
    d
}

#[test]
fn system_gain_scales_app_sources_from_the_first_sample_and_leaves_other_sources_alone() {
    let (mut m, h, _) = setup(&app_dev(), &layout());
    h.sys_gain().set("s1", 0.5);
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(
        o[0].iter().all(|x| *x == 0.5) && o[1].iter().all(|x| *x == 0.25),
        "app: no fade-in"
    );
    let (mut m, h, _) = setup(&dev(), &layout());
    h.sys_gain().set("s1", 0.0);
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(
        o[0].iter().all(|x| *x == 1.0),
        "a device source does not follow V's volume"
    );
}

#[test]
fn system_gain_changes_ramp_without_a_step_and_survive_a_plan_swap() {
    let (mut m, mut h, _) = setup(&app_dev(), &layout());
    let n = 512;
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    run(&mut m, &ins, 4, n);
    h.sys_gain().set("s1", 0.0);
    let step = 1.0 / n as f32;
    let mut prev = 1.0;
    for x in &run(&mut m, &ins, 4, n)[0] {
        assert!(prev - x <= step + 1e-5 && prev >= *x, "jump {prev} -> {x}");
        prev = *x;
    }
    assert!(prev.abs() < 1e-5, "ramp must land on the target");
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 0.0));
    // A new plan keeps the applied level: no jump back to unity and down again.
    h.set_plan(MixPlan::compile(&app_dev(), &layout(), 48_000.0));
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 0.0));
    // Unmute ramps up from silence, and the source's own gain still multiplies.
    h.sys_gain().set("s1", 1.0);
    let o = run(&mut m, &ins, 4, n);
    assert!(o[0][0] <= step + 1e-5 && close(o[0][n - 1], 1.0));
    // Garbage published by the control side cannot blow up the output.
    h.sys_gain().set("s1", f32::NAN);
    run(&mut m, &ins, 4, n);
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 1.0));
    h.sys_gain().set("s1", 1e9);
    run(&mut m, &ins, 4, n);
    assert!(run(&mut m, &ins, 4, n)[0].iter().all(|x| *x == 1.0));
}

#[test]
fn system_gain_is_per_source() {
    let (mut m, h, _) = setup(&app_dev(), &layout());
    h.sys_gain().set("other", 0.0);
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(
        o[0].iter().all(|x| *x == 1.0),
        "another source's gain is not this source's"
    );
    h.sys_gain().set("s1", 0.5);
    h.sys_gain().set("s1", 1.0);
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(
        o[0].iter().all(|x| close(*x, 1.0)),
        "back to unity: the app plays elsewhere"
    );
}

#[test]
fn a_wire_listed_twice_is_one_wire_not_double_level() {
    let mut d = dev();
    d.wires.extend(d.wires.clone());
    d.monitors.push(mon("spk", true, 0.0));
    let mw = d.monitors[0].wires.clone();
    d.monitors[0].wires.extend(mw);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(o[0].iter().all(|x| *x == 1.0), "{:?}", o[0]);
    assert!(o[1].iter().all(|x| *x == 0.5), "{:?}", o[1]);
    assert!(
        o[2].iter().all(|x| *x == 1.0),
        "monitor wire counted twice: {:?}",
        o[2]
    );
}

/// Runs `blocks` blocks of `n` frames of constant input 1.0 and returns channel 0 end to end.
fn series(m: &mut Mixer, n: usize, blocks: usize) -> Vec<f32> {
    let ins = vec![vec![1.0; n], vec![1.0; n]];
    (0..blocks)
        .flat_map(|_| run(m, &ins, 4, n).swap_remove(0))
        .collect()
}

/// A mute or unmute takes the same 480 frames (10 ms at 48 kHz) whatever the block size: a 64-frame
/// buffer used to fade in 1.3 ms, which is a click on low frequencies.
#[test]
fn mute_and_unmute_fade_over_the_same_frames_at_any_block_size() {
    for n in [1, 3, 33, 64, 128, 256, 480, 512] {
        let blocks = 600usize.div_ceil(n) + 1;
        let (mut m, mut h, _) = setup(&dev(), &layout());
        series(&mut m, n, 2);
        let mut d = dev();
        d.muted = true;
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        let down = series(&mut m, n, blocks);
        let len = n.max(480); // frames the fade takes: 480, or the whole block when that is longer
        let step = 1.0 / len as f32 + 1e-6;
        assert!(
            down.windows(2).all(|w| w[0] >= w[1] && w[0] - w[1] <= step),
            "{n}: steps"
        );
        assert!(down[0] >= 1.0 - step, "{n}: starts at {}", down[0]);
        let first_zero = down
            .iter()
            .position(|x| *x == 0.0)
            .expect("reaches exact silence");
        assert!(
            first_zero.abs_diff(len - 1) <= n,
            "{n}: silent after {first_zero} frames"
        );
        assert!(down[first_zero..].iter().all(|x| *x == 0.0), "{n}");
        d.muted = false;
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        let up = series(&mut m, n, blocks);
        assert!(
            up.windows(2).all(|w| w[1] >= w[0] && w[1] - w[0] <= step),
            "{n}: up steps"
        );
        assert_eq!(*up.last().unwrap(), 1.0, "{n}");
        assert!(up[0] <= step, "{n}: starts from silence");
    }
}

/// A reversal in mid-fade continues from where the fade was, not from either end.
#[test]
fn a_reversal_mid_fade_continues_from_where_it_was() {
    let (mut m, mut h, _) = setup(&dev(), &layout());
    series(&mut m, 64, 1);
    let mut d = dev();
    d.muted = true;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let down = series(&mut m, 64, 3); // 192 of 480 frames
    let at = *down.last().unwrap();
    assert!((at - (1.0 - 192.0 / 480.0)).abs() < 1e-4, "{at}");
    d.muted = false;
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let up = series(&mut m, 64, 1);
    assert!(
        (up[0] - at).abs() <= 1.0 / 480.0 + 1e-5 && up[0] > at,
        "{} after {at}",
        up[0]
    );
}

/// A gain ramp must land exactly on its target, however many changes came before and whatever the block
/// size: if `cur` ever stopped one ulp short (an f32 ramp end `from + step * n` is not exactly `to`), a
/// steady device would sit at 0.9999 of the level the user set for as long as it runs. Thousands of random
/// volume/mute changes, each followed by the fade (blocks of random lengths) and a settled block.
#[test]
fn after_any_number_of_gain_changes_a_settled_block_is_bit_exact() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u32
    };
    let (mut m, mut h, _) = setup(&dev(), &layout());
    let gain = |db: f32| 10f32.powf(db.min(MAX_DB) / 20.0);
    for i in 0..5000 {
        let mut d = dev();
        let db = |r: u32| (r % 8400) as f32 / 100.0 - 60.0; // -60.00 ..= +23.99 dB
        let (src_db, dev_db) = (db(next()), db(next()));
        let (muted, dev_muted) = (next() % 7 == 0, next() % 11 == 0);
        d.sources[0].volume_db = src_db;
        d.sources[0].muted = muted;
        d.volume_db = dev_db;
        d.muted = dev_muted;
        h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
        let ins = vec![vec![1.0; 300], vec![1.0; 300]];
        // The fade crosses one unit of gain per 480 frames: 30 x 300 frames cover the widest step (+24 dB).
        for _ in 0..30 {
            run(&mut m, &ins, 4, 1 + next() as usize % 300);
        }
        let n = 1 + next() as usize % 300;
        let want =
            (if muted { 0.0 } else { gain(src_db) }) * if dev_muted { 0.0 } else { gain(dev_db) };
        for x in &run(&mut m, &ins, 4, n)[0] {
            assert_eq!(
                *x, want,
                "change {i}: src {src_db} dB (muted {muted}), device {dev_db} dB (muted {dev_muted})"
            );
        }
    }
}

/// `dev()` with the speaker's wires replaced.
fn dev_with_monitor_wires(pairs: &[(u16, u16)]) -> VirtualDevice {
    let mut d = dev();
    let mut m = mon("spk", true, 0.0);
    m.wires = pairs
        .iter()
        .map(|&(out_ch, dst_ch)| MonitorWire { out_ch, dst_ch })
        .collect();
    d.monitors.push(m);
    d
}

#[test]
fn monitor_at_unity_is_bit_exact_for_stereo_and_mono_to_stereo() {
    // stereo -> stereo
    let (mut m, _h, _) = setup(&dev_with_monitor_wires(&[(0, 0), (1, 1)]), &layout());
    let ins = vec![vec![0.3; 16], vec![-0.7; 16]];
    let o = run(&mut m, &ins, 4, 16);
    assert!(o[2].iter().all(|x| *x == 0.3));
    assert!(o[3].iter().all(|x| *x == -0.7));
    // mono source fanned to both vdev channels -> stereo monitor
    let mut d = dev_with_monitor_wires(&[(0, 0), (1, 1)]);
    d.sources[0].channels = 1;
    d.wires[1].src_ch = 0;
    let mut l = layout();
    l.inputs.truncate(1);
    let (mut m, _h, _) = setup(&d, &l);
    let o = run(&mut m, &[vec![0.3; 16]], 4, 16);
    assert!(o[2].iter().chain(&o[3]).all(|x| *x == 0.3));
}

#[test]
fn two_vdev_channels_onto_one_monitor_channel_average() {
    // A mono output fed by L and R: a duplicated mono source stays at its own level (not +6 dB).
    let d = dev_with_monitor_wires(&[(0, 0), (1, 0)]);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &[vec![0.5; 16], vec![0.5; 16]], 4, 16);
    assert!(o[2].iter().all(|x| *x == 0.5), "{:?}", o[2]);
    assert!(o[3].iter().all(|x| *x == 0.0), "dst 1 has no wire");
    // Different levels: the mean.
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(o[2].iter().all(|x| close(*x, 0.75)), "{:?}", o[2]);
    // The vdev's own outputs are not scaled.
    assert!(o[0].iter().all(|x| *x == 1.0) && o[1].iter().all(|x| *x == 0.5));
}

#[test]
fn clipped_blocks_count_blocks_with_a_vdev_sample_over_full_scale() {
    let (mut m, mut h, meters) = setup(&dev(), &layout());
    // Exactly full scale and non-finite input (silence to the mixer) are not clipping.
    run(&mut m, &[vec![1.0; 16], vec![f32::INFINITY; 16]], 4, 16);
    assert_eq!(meters.clipped_blocks(), 0);
    run(&mut m, &[vec![0.2; 16], vec![-1.5; 16]], 4, 16);
    assert_eq!(meters.clipped_blocks(), 1, "negative overshoot counts");
    run(&mut m, &[vec![0.2; 16], vec![0.2; 16]], 4, 16);
    assert_eq!(meters.clipped_blocks(), 1, "a quiet block adds nothing");
    // One call longer than MAX_FRAMES runs as two blocks.
    let n = MAX_FRAMES + 1;
    run(&mut m, &[vec![1.5; n], vec![0.0; n]], 4, n);
    assert_eq!(meters.clipped_blocks(), 3);
    // A routing change runs the old plan once more for the crossfade: still one count per block.
    let mut d = dev();
    d.wires.pop();
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    run(&mut m, &[vec![1.5; 16], vec![1.5; 16]], 4, 16);
    assert_eq!(meters.clipped_blocks(), 4);
}

// ---- wire trim ----------------------------------------------------------------------------------------

fn trim(d: &mut VirtualDevice, wire: usize, gain_db: f32, invert: bool) {
    d.wires[wire].gain_db = gain_db;
    d.wires[wire].invert = invert;
}

#[test]
fn a_wire_trim_scales_that_cable_only() {
    let mut d = dev();
    trim(&mut d, 0, -6.0, false);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!((o[0][15] - 0.501).abs() < 1e-3, "{}", o[0][15]);
    assert!(
        o[1].iter().all(|x| *x == 0.5),
        "the other wire is untouched"
    );
}

#[test]
fn invert_flips_polarity_bit_exactly() {
    let mut d = dev();
    trim(&mut d, 0, 0.0, true);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(o[0].iter().all(|x| x.to_bits() == (-1.0f32).to_bits()));
    assert!(o[1].iter().all(|x| *x == 0.5));
}

/// The case the trim exists for: two mono sources onto one output channel add +6 dB; -6.0206 dB each fixes it.
#[test]
fn fan_in_trim_brings_two_sources_back_to_unity() {
    let mut d = dev();
    d.channels = 2;
    d.sources = vec![src("a", 1, 0.0, false), src("b", 1, 0.0, false)];
    d.wires = vec![Wire::new("a", 0, 0), Wire::new("b", 0, 0)];
    let l = IoLayout {
        inputs: vec![Some(("a".into(), 0)), Some(("b".into(), 0))],
        outputs: vec![Some(OutPort::Device(0)), Some(OutPort::Device(1))],
    };
    let ins = vec![vec![0.5; 8], vec![0.5; 8]];
    let (mut m, _h, _) = setup(&d, &l);
    assert_eq!(run(&mut m, &ins, 2, 8)[0][7], 1.0, "untrimmed fan-in sums");
    let half = 20.0 * 0.5f32.log10();
    trim(&mut d, 0, half, false);
    trim(&mut d, 1, half, false);
    let (mut m, _h, _) = setup(&d, &l);
    assert!((run(&mut m, &ins, 2, 8)[0][7] - 0.5).abs() < 1e-6);
}

/// A trim of 0 dB must not change one bit, also with several sources per channel in their tap order.
#[test]
fn neutral_trims_leave_every_sample_bit_identical() {
    let mut d = dev();
    d.sources = vec![
        src("a", 2, -3.3, false),
        src("b", 2, 2.7, false),
        src("c", 2, 0.0, false),
    ];
    d.wires = ["a", "b", "c"]
        .iter()
        .flat_map(|s| [Wire::new(*s, 0, 0), Wire::new(*s, 1, 1)])
        .collect();
    d.volume_db = -1.9;
    let l = IoLayout {
        inputs: ["a", "b", "c"]
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
    let (mut m, _h, _) = setup(&d, &l);
    let got = run(&mut m, &ins, 2, 300);
    let g = |db: f32| 10f32.powf(db / 20.0);
    for (ch, row) in got.iter().enumerate() {
        for (i, x) in row.iter().enumerate() {
            let mut acc = 0.0f32;
            for (k, db) in [-3.3f32, 2.7, 0.0].iter().enumerate() {
                acc += ins[k * 2 + ch][i] * g(*db);
            }
            acc *= g(-1.9);
            assert_eq!(x.to_bits(), acc.to_bits(), "ch {ch} frame {i}");
        }
    }
}

/// A trim edit is a new plan with the same route; its gain continues from where the old one stood.
#[test]
fn a_trim_edit_ramps_and_never_steps() {
    let mut d = dev();
    trim(&mut d, 0, -6.0, false);
    let (mut m, mut h, _) = setup(&d, &layout());
    let ins = vec![vec![1.0; 64], vec![1.0; 64]];
    let before = run(&mut m, &ins, 4, 64)[0][63];
    trim(&mut d, 0, -12.0, false);
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let o = run(&mut m, &ins, 4, 64);
    let reach = 64.0 / 480.0; // slew moves at most 1/480 per frame
    assert!((o[0][63] - (before - reach)).abs() < 1e-3, "{}", o[0][63]);
    assert!(o[0].windows(2).all(|w| (w[1] - w[0]).abs() < 3e-3));
    // settles at the new trim
    let mut last = 0.0;
    for _ in 0..20 {
        last = run(&mut m, &ins, 4, 64)[0][63];
    }
    assert!((last - 0.2512).abs() < 1e-3, "{last}");
}

/// Flipping polarity ramps through zero (|-1 - 1| = 2 units, 20 ms at 48 kHz), not a step.
#[test]
fn a_polarity_flip_crosses_zero_in_twenty_ms() {
    let mut d = dev();
    let (mut m, mut h, _) = setup(&d, &layout());
    let ins = vec![vec![1.0; 480], vec![1.0; 480]];
    run(&mut m, &ins, 4, 480);
    trim(&mut d, 0, 0.0, true);
    h.set_plan(MixPlan::compile(&d, &layout(), 48_000.0));
    let first = run(&mut m, &ins, 4, 480)[0].clone();
    assert!(first.windows(2).all(|w| (w[1] - w[0]).abs() < 3e-3));
    assert!(first[479].abs() < 1e-3, "half way is zero: {}", first[479]);
    let second = run(&mut m, &ins, 4, 480)[0].clone();
    assert_eq!(second[479], -1.0);
}

#[test]
fn a_nan_trim_is_silence_and_an_over_range_trim_clamps() {
    let mut d = dev();
    trim(&mut d, 0, f32::NAN, false);
    trim(&mut d, 1, 1000.0, false);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!(o[0].iter().all(|x| *x == 0.0));
    assert!(o[1].iter().all(|x| x.is_finite() && *x < 0.5 * 15.9));
}

/// A wire listed twice is one wire: the first, trim included.
#[test]
fn a_duplicate_wire_keeps_the_first_trim() {
    let mut d = dev();
    d.wires.push(d.wires[0].clone());
    trim(&mut d, 0, -6.0, false);
    let (mut m, _h, _) = setup(&d, &layout());
    let o = run(&mut m, &ones(16), 4, 16);
    assert!((o[0][15] - 0.501).abs() < 1e-3);
}
