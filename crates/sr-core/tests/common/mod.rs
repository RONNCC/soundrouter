#![allow(dead_code)]
use sr_core::config::*;
use sr_core::mixer::*;

pub fn src(id: &str, channels: u16, volume_db: f32, muted: bool) -> Source {
    Source {
        id: id.into(),
        kind: SourceKind::Device {
            uid: format!("uid-{id}"),
        },
        channels,
        volume_db,
        muted,
        mute_original: false,
        name: String::new(),
    }
}

/// 2ch vdev, source s1 (2ch) wired straight through.
pub fn dev() -> VirtualDevice {
    VirtualDevice {
        id: "v1".into(),
        name: "V".into(),
        enabled: true,
        channels: 2,
        volume_db: 0.0,
        muted: false,
        sources: vec![src("s1", 2, 0.0, false)],
        wires: vec![
            Wire {
                source: "s1".into(),
                src_ch: 0,
                out_ch: 0,
            },
            Wire {
                source: "s1".into(),
                src_ch: 1,
                out_ch: 1,
            },
        ],
        monitors: vec![],
    }
}

pub fn mon(uid: &str, enabled: bool, volume_db: f32) -> Monitor {
    Monitor {
        device_uid: uid.into(),
        enabled,
        volume_db,
        wires: vec![
            MonitorWire {
                out_ch: 0,
                dst_ch: 0,
            },
            MonitorWire {
                out_ch: 1,
                dst_ch: 1,
            },
        ],
        name: String::new(),
    }
}

/// inputs: s1 ch0, s1 ch1. outputs: vdev ch0, ch1, then speaker ch0, ch1.
pub fn layout() -> IoLayout {
    IoLayout {
        inputs: vec![Some(("s1".into(), 0)), Some(("s1".into(), 1))],
        outputs: vec![
            Some(OutPort::Device(0)),
            Some(OutPort::Device(1)),
            Some(OutPort::Monitor {
                uid: "spk".into(),
                ch: 0,
            }),
            Some(OutPort::Monitor {
                uid: "spk".into(),
                ch: 1,
            }),
        ],
    }
}

pub fn run(m: &mut Mixer, ins: &[Vec<f32>], n_out: usize, frames: usize) -> Vec<Vec<f32>> {
    let ins: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut outs = vec![vec![9.0f32; frames]; n_out];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    m.process(&ins, &mut refs, frames);
    outs
}
