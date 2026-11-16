//! Pure buffer-shape logic: aggregate stream descriptions -> `IoLayout`, plus
//! (de)interleaving between CoreAudio's interleaved buffers and the planar mixer scratch.
use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use sr_core::config::{MAX_CH, MAX_FRAMES};
use sr_core::mixer::{IoLayout, OutPort};

/// One subdevice of an aggregate with its own stream buffers (channels per buffer).
#[derive(Clone, Debug, PartialEq)]
pub struct SubDesc {
    pub uid: String,
    pub is_main: bool,
    pub in_bufs: Vec<u16>,
    pub out_bufs: Vec<u16>,
}

pub struct LayoutSpec<'a> {
    pub subs: &'a [SubDesc],
    /// Source id fed by the main (companion) device's input; `None` = ignore it.
    pub passthru: Option<&'a str>,
    /// Device uid -> Device source id.
    pub device_sources: &'a HashMap<String, String>,
    pub monitors: &'a HashSet<String>,
    /// Tap source ids in tap-list order (one input buffer each, after the subdevices).
    pub taps: &'a [String],
}

/// Build the planar `IoLayout`; `agg_in`/`agg_out` are the aggregate's real stream shapes and must
/// match what the subdevices + taps predict (aggregate buffers = subdevices in order, then taps).
pub fn build_layout(spec: &LayoutSpec, agg_in: &[u16], agg_out: &[u16]) -> Result<IoLayout> {
    let sub_in: Vec<u16> = spec
        .subs
        .iter()
        .flat_map(|s| s.in_bufs.iter().copied())
        .collect();
    let sub_out: Vec<u16> = spec
        .subs
        .iter()
        .flat_map(|s| s.out_bufs.iter().copied())
        .collect();
    if agg_in.len() != sub_in.len() + spec.taps.len() || agg_in[..sub_in.len()] != sub_in[..] {
        bail!(
            "aggregate input shape {agg_in:?} does not match subdevices {sub_in:?} + {} taps",
            spec.taps.len()
        );
    }
    if agg_out != sub_out.as_slice() {
        bail!("aggregate output shape {agg_out:?} does not match subdevices {sub_out:?}");
    }
    let mut inputs: Vec<Option<(String, u16)>> = Vec::new();
    let mut outputs: Vec<Option<OutPort>> = Vec::new();
    for s in spec.subs {
        let src = if s.is_main {
            spec.passthru
        } else {
            spec.device_sources.get(&s.uid).map(String::as_str)
        };
        for ch in 0..s.in_bufs.iter().sum::<u16>() {
            inputs.push(src.map(|id| (id.to_owned(), ch)));
        }
        for ch in 0..s.out_bufs.iter().sum::<u16>() {
            outputs.push(if s.is_main {
                Some(OutPort::Device(ch))
            } else if spec.monitors.contains(&s.uid) {
                Some(OutPort::Monitor {
                    uid: s.uid.clone(),
                    ch,
                })
            } else {
                None
            });
        }
    }
    for (id, &chs) in spec.taps.iter().zip(&agg_in[sub_in.len()..]) {
        for ch in 0..chs {
            inputs.push(Some((id.clone(), ch)));
        }
    }
    Ok(IoLayout { inputs, outputs })
}

/// FNV-1a over both buffer-channel lists; equal shapes give equal keys. Never 0.
pub fn shape_key(ins: impl Iterator<Item = u32>, outs: impl Iterator<Item = u32>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| h = (h ^ v).wrapping_mul(0x0100_0000_01b3);
    let mut n = 0;
    for c in ins {
        mix(u64::from(c));
        n += 1;
    }
    mix(0xffff_0000 + n);
    for c in outs {
        mix(u64::from(c));
    }
    h | 1
}

pub fn shape_key_of(ins: &[u16], outs: &[u16]) -> u64 {
    shape_key(
        ins.iter().map(|&c| u32::from(c)),
        outs.iter().map(|&c| u32::from(c)),
    )
}

/// Interleaved `src` (frames x ch) -> planar scratch rows `dst_ch0..dst_ch0+ch` (row stride MAX_FRAMES).
pub fn deinterleave(
    src: &[f32],
    ch: usize,
    frame0: usize,
    n: usize,
    dst: &mut [f32],
    dst_ch0: usize,
) {
    for c in 0..ch {
        let row = &mut dst[(dst_ch0 + c) * MAX_FRAMES..][..n];
        for (f, d) in row.iter_mut().enumerate() {
            *d = src[(frame0 + f) * ch + c];
        }
    }
}

/// Inverse of [`deinterleave`].
pub fn interleave(
    src: &[f32],
    src_ch0: usize,
    frame0: usize,
    n: usize,
    dst: &mut [f32],
    ch: usize,
) {
    for c in 0..ch {
        let row = &src[(src_ch0 + c) * MAX_FRAMES..][..n];
        for (f, &s) in row.iter().enumerate() {
            dst[(frame0 + f) * ch + c] = s;
        }
    }
}

pub const SCRATCH_LEN: usize = MAX_CH * MAX_FRAMES;

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(uid: &str, main: bool, i: &[u16], o: &[u16]) -> SubDesc {
        SubDesc {
            uid: uid.into(),
            is_main: main,
            in_bufs: i.to_vec(),
            out_bufs: o.to_vec(),
        }
    }

    #[test]
    fn roundtrip() {
        let src: Vec<f32> = (0..12).map(|x| x as f32).collect(); // 4 frames x 3 ch
        let mut planar = vec![0.0; SCRATCH_LEN];
        deinterleave(&src, 3, 0, 4, &mut planar, 2);
        assert_eq!(
            &planar[2 * MAX_FRAMES..2 * MAX_FRAMES + 4],
            &[0.0, 3.0, 6.0, 9.0]
        );
        assert_eq!(
            &planar[4 * MAX_FRAMES..4 * MAX_FRAMES + 4],
            &[2.0, 5.0, 8.0, 11.0]
        );
        let mut out = vec![-1.0; 12];
        interleave(&planar, 2, 0, 4, &mut out, 3);
        assert_eq!(out, src);
    }

    #[test]
    fn roundtrip_chunked() {
        let src: Vec<f32> = (0..10).map(|x| x as f32).collect(); // 5 frames x 2
        let mut planar = vec![0.0; SCRATCH_LEN];
        let mut out = vec![0.0; 10];
        for (f0, n) in [(0, 3), (3, 2)] {
            deinterleave(&src, 2, f0, n, &mut planar, 0);
            interleave(&planar, 0, f0, n, &mut out, 2);
        }
        assert_eq!(out, src);
    }

    #[test]
    fn layout_order() {
        let subs = [
            sub("comp", true, &[2], &[2]),
            sub("mic", false, &[1], &[]),
            sub("spk", false, &[], &[1, 1]),
        ];
        let ds = HashMap::from([("mic".to_owned(), "s2".to_owned())]);
        let mons = HashSet::from(["spk".to_owned()]);
        let taps = ["s1".to_owned()];
        let spec = LayoutSpec {
            subs: &subs,
            passthru: Some("pt"),
            device_sources: &ds,
            monitors: &mons,
            taps: &taps,
        };
        let l = build_layout(&spec, &[2, 1, 2], &[2, 1, 1]).unwrap();
        let ins: Vec<_> = l
            .inputs
            .iter()
            .map(|i| i.clone().map(|(s, c)| format!("{s}:{c}")))
            .collect();
        assert_eq!(
            ins,
            ["pt:0", "pt:1", "s2:0", "s1:0", "s1:1"].map(|s| Some(s.to_owned()))
        );
        assert_eq!(l.outputs.len(), 4);
        assert!(matches!(l.outputs[1], Some(OutPort::Device(1))));
        assert!(matches!(&l.outputs[3], Some(OutPort::Monitor { uid, ch: 1 }) if uid == "spk"));
    }

    #[test]
    fn layout_unwired_slots_and_mismatch() {
        let subs = [sub("comp", true, &[2], &[2]), sub("mic", false, &[1], &[1])];
        let (ds, mons) = (HashMap::new(), HashSet::new());
        let spec = LayoutSpec {
            subs: &subs,
            passthru: None,
            device_sources: &ds,
            monitors: &mons,
            taps: &[],
        };
        let l = build_layout(&spec, &[2, 1], &[2, 1]).unwrap();
        assert!(l.inputs.iter().all(Option::is_none));
        assert!(l.outputs[2].is_none());
        assert!(build_layout(&spec, &[2], &[2, 1]).is_err());
        assert!(build_layout(&spec, &[2, 1, 2], &[2, 1]).is_err());
    }

    #[test]
    fn shape_keys_differ() {
        assert_ne!(shape_key_of(&[2, 1], &[2]), shape_key_of(&[2], &[1, 2]));
        assert_eq!(
            shape_key_of(&[2], &[2]),
            shape_key([2u32].into_iter(), [2u32].into_iter())
        );
    }
}
