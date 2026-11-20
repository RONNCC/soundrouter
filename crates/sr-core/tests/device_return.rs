//! A physical input or monitor that disappears and comes back keeps its routing, gain and mute:
//! they live in the config, and the mixer plan only drops what the layout lacks.
mod common;
use common::*;
use sr_core::config::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::sync::Arc;

/// The aggregate without the unplugged device: no input slots, no monitor outputs.
fn unplugged() -> IoLayout {
    IoLayout {
        inputs: vec![],
        outputs: vec![Some(OutPort::Device(0)), Some(OutPort::Device(1))],
    }
}

fn mixer_for(d: &VirtualDevice, l: &IoLayout) -> Mixer {
    let (m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(d, l));
    m
}

fn routed() -> VirtualDevice {
    let mut d = dev();
    d.sources[0].volume_db = -6.0;
    d.monitors.push(mon("spk", true, -3.0));
    d
}

#[test]
fn input_and_monitor_come_back_exactly_as_they_were() {
    let d = routed();
    let ins = vec![vec![1.0; 16], vec![0.5; 16]];
    let before = run(&mut mixer_for(&d, &layout()), &ins, 4, 16);
    assert!((before[0][15] - 0.501).abs() < 1e-3, "source gain applied");
    assert!(before[2][15] > 0.0, "monitor hears it");

    // Unplugged: silence on the virtual device, nothing panics, the config is untouched.
    let gone = run(&mut mixer_for(&d, &unplugged()), &[], 2, 16);
    assert!(gone.iter().flatten().all(|x| *x == 0.0));

    // Plugged back in: the same plan on the full layout gives the same samples.
    assert_eq!(run(&mut mixer_for(&d, &layout()), &ins, 4, 16), before);
}

#[test]
fn mute_and_gain_changed_while_unplugged_apply_when_it_returns() {
    let mut d = routed();
    d.sources[0].muted = true;
    let ins = vec![vec![1.0; 16], vec![0.5; 16]];
    let muted = run(&mut mixer_for(&d, &layout()), &ins, 4, 16);
    assert!(muted.iter().flatten().all(|x| *x == 0.0), "mute survives");

    // Unplugged: the edits are made while the plan has no slot for the device.
    let kept = d.clone();
    run(&mut mixer_for(&d, &unplugged()), &[], 2, 16);
    assert_eq!(
        d, kept,
        "building a plan without it leaves the config alone"
    );
    d.sources[0].muted = false;
    d.sources[0].volume_db = 0.0;
    d.monitors[0].volume_db = -80.0;
    let gone = run(&mut mixer_for(&d, &unplugged()), &[], 2, 16);
    assert!(gone.iter().flatten().all(|x| *x == 0.0), "still silent");
    assert_ne!(d, kept, "the edits are in the config");
    let edited = d.clone();

    let back = run(&mut mixer_for(&d, &layout()), &ins, 4, 16);
    assert_eq!(d, edited, "plugging in changes nothing either");
    assert_eq!(back[0][15], 1.0);
    assert!(back[2][15] < 1e-3, "monitor gain edited while away");
}

#[test]
fn a_returning_device_is_found_by_uid_not_by_position() {
    // Another input appeared in between: the layout's slots shift, the wires follow the source id.
    let d = routed();
    let shifted = IoLayout {
        inputs: vec![None, None, Some(("s1".into(), 0)), Some(("s1".into(), 1))],
        ..layout()
    };
    let ins = vec![vec![0.0; 16], vec![0.0; 16], vec![1.0; 16], vec![0.5; 16]];
    let o = run(&mut mixer_for(&d, &shifted), &ins, 4, 16);
    assert!((o[0][15] - 0.501).abs() < 1e-3 && (o[1][15] - 0.2505).abs() < 1e-3);
}
