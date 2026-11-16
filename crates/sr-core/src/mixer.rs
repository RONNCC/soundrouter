use crate::config::{MAX_CH, MAX_FRAMES, VirtualDevice};
use crate::meters::{METER_SLOTS, Meters, peak};
use rtrb::{Consumer, Producer, RingBuffer};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;

pub enum OutPort {
    Device(u16),
    Monitor { uid: String, ch: u16 },
}
/// index = aggregate planar ch; None = ignore (e.g. vdev input).
/// `inputs` entries are `(source id, source channel)`.
pub struct IoLayout {
    pub inputs: Vec<Option<(String, u16)>>,
    pub outputs: Vec<Option<OutPort>>,
}

/// A linear gain that ramps from `cur` (previously applied) to `target` across a block.
/// `key` identifies it across plans so a swap continues from the old applied value.
struct Gain {
    key: u64,
    cur: f32,
    target: f32,
}
impl Gain {
    fn new(key: impl Hash, db: f32, off: bool) -> Gain {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        let target = if off || !db.is_finite() {
            0.0
        } else {
            10f32.powf(db / 20.0)
        };
        Gain {
            key: h.finish(),
            cur: target,
            target,
        }
    }
    fn inherit(&mut self, old: &[Gain]) {
        if let Some(o) = old.iter().find(|o| o.key == self.key) {
            self.cur = o.cur;
        }
    }
}

struct Tap {
    in_idx: usize,
    vch: usize,
    src: usize,
}
struct MonPlan {
    gain: Gain,
    wires: Vec<(usize, usize)>, // (vdev ch, output slot)
}

/// Precomputed sparse taps (input slot -> vdev channel via source gain) + per-output gains.
pub struct MixPlan {
    in_len: usize,
    out_len: usize,
    nch: usize,
    srcs: Vec<Gain>,
    taps: Vec<Tap>,
    dev_gain: Gain,
    dev_out: Vec<(usize, usize)>, // (vdev ch, output slot)
    mons: Vec<MonPlan>,
    meter_in: Vec<usize>,
    acc: Vec<f32>, // nch * MAX_FRAMES vdev channel scratch
}

fn ramp_add(dst: &mut [f32], src: &[f32], from: f32, to: f32) {
    if from == to {
        if to != 0.0 {
            dst.iter_mut().zip(src).for_each(|(d, s)| *d += s * to);
        }
        return;
    }
    let step = (to - from) / dst.len() as f32;
    let mut g = from;
    for (d, s) in dst.iter_mut().zip(src) {
        g += step;
        *d += s * g;
    }
}

fn ramp_scale(buf: &mut [f32], from: f32, to: f32) {
    if from == to {
        if to != 1.0 {
            buf.iter_mut().for_each(|x| *x *= to);
        }
        return;
    }
    let step = (to - from) / buf.len() as f32;
    let mut g = from;
    for x in buf {
        g += step;
        *x *= g;
    }
}

impl MixPlan {
    /// Wires whose source/channel is absent from the layout, or out of range, are dropped.
    /// Source volume/mute -> per-source gain; device volume/mute -> vdev gain;
    /// monitor volume/enabled -> monitor gain (monitors hear the vdev post gain).
    pub fn compile(dev: &VirtualDevice, layout: &IoLayout) -> MixPlan {
        let nch = usize::from(dev.channels).min(MAX_CH);
        let srcs: Vec<Gain> = dev
            .sources
            .iter()
            .map(|s| Gain::new(&s.id, s.volume_db, s.muted))
            .collect();
        let mut taps = Vec::new();
        for w in &dev.wires {
            let (Some(src), vch) = (
                dev.sources.iter().position(|s| s.id == w.source),
                usize::from(w.out_ch),
            ) else {
                continue;
            };
            if vch >= nch {
                continue;
            }
            for (in_idx, slot) in layout.inputs.iter().enumerate() {
                if let Some((id, ch)) = slot
                    && *id == w.source
                    && *ch == w.src_ch
                {
                    taps.push(Tap { in_idx, vch, src });
                }
            }
        }
        let mut dev_out = Vec::new();
        for (slot, p) in layout.outputs.iter().enumerate() {
            if let Some(OutPort::Device(c)) = p
                && usize::from(*c) < nch
            {
                dev_out.push((usize::from(*c), slot));
            }
        }
        let mons = dev
            .monitors
            .iter()
            .map(|m| {
                let mut wires = Vec::new();
                for mw in &m.wires {
                    if usize::from(mw.out_ch) >= nch {
                        continue;
                    }
                    for (slot, p) in layout.outputs.iter().enumerate() {
                        if let Some(OutPort::Monitor { uid, ch }) = p
                            && *uid == m.device_uid
                            && *ch == mw.dst_ch
                        {
                            wires.push((usize::from(mw.out_ch), slot));
                        }
                    }
                }
                MonPlan {
                    gain: Gain::new(&m.device_uid, m.volume_db, !m.enabled),
                    wires,
                }
            })
            .collect();
        let meter_in = layout
            .inputs
            .iter()
            .enumerate()
            .filter(|(i, s)| s.is_some() && *i < METER_SLOTS)
            .map(|(i, _)| i)
            .collect();
        MixPlan {
            in_len: layout.inputs.len(),
            out_len: layout.outputs.len(),
            nch,
            srcs,
            taps,
            dev_gain: Gain::new("dev", dev.volume_db, dev.muted),
            dev_out,
            mons,
            meter_in,
            acc: vec![0.0; nch * MAX_FRAMES],
        }
    }

    /// Continue gain ramps from what `old` last applied.
    fn inherit(&mut self, old: &MixPlan) {
        self.srcs.iter_mut().for_each(|g| g.inherit(&old.srcs));
        self.dev_gain.cur = old.dev_gain.cur;
        for m in &mut self.mons {
            if let Some(o) = old.mons.iter().find(|o| o.gain.key == m.gain.key) {
                m.gain.cur = o.gain.cur;
            }
        }
    }

    /// One block of `n <= MAX_FRAMES` frames at `off`. Caller checked lengths. Outputs already zeroed.
    fn run(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        off: usize,
        n: usize,
        meters: &Meters,
    ) {
        let end = off + n;
        for c in 0..self.nch {
            self.acc[c * MAX_FRAMES..][..n].fill(0.0);
        }
        for t in &self.taps {
            let g = &self.srcs[t.src];
            ramp_add(
                &mut self.acc[t.vch * MAX_FRAMES..][..n],
                &inputs[t.in_idx][off..end],
                g.cur,
                g.target,
            );
        }
        for g in &mut self.srcs {
            g.cur = g.target;
        }
        for &i in &self.meter_in {
            meters.update_input(i, peak(&inputs[i][off..end]));
        }
        for c in 0..self.nch {
            let ch = &mut self.acc[c * MAX_FRAMES..][..n];
            ramp_scale(ch, self.dev_gain.cur, self.dev_gain.target);
            meters.update_output(c, peak(ch));
        }
        self.dev_gain.cur = self.dev_gain.target;
        for &(c, slot) in &self.dev_out {
            ramp_add(
                &mut outputs[slot][off..end],
                &self.acc[c * MAX_FRAMES..][..n],
                1.0,
                1.0,
            );
        }
        for m in &mut self.mons {
            for &(c, slot) in &m.wires {
                ramp_add(
                    &mut outputs[slot][off..end],
                    &self.acc[c * MAX_FRAMES..][..n],
                    m.gain.cur,
                    m.gain.target,
                );
            }
            m.gain.cur = m.gain.target;
        }
    }
}

const PLAN_QUEUE: usize = 4;
// Every box ever pushed comes back here; at most PLAN_QUEUE + 1 are outstanding per control-side drain.
const RETURN_QUEUE: usize = 2 * PLAN_QUEUE;

/// Audio-thread side; zero alloc in process.
pub struct Mixer {
    plan: Option<Box<MixPlan>>,
    rx: Consumer<Box<MixPlan>>,
    ret: Producer<Box<MixPlan>>,
    meters: Arc<Meters>,
}
/// Control side.
pub struct MixerHandle {
    tx: Producer<Box<MixPlan>>,
    ret: Consumer<Box<MixPlan>>,
    /// Newest plan the full queue refused; retried by `flush`/`set_plan`.
    stash: Option<Box<MixPlan>>,
}
pub fn mixer(meters: Arc<Meters>) -> (Mixer, MixerHandle) {
    let (tx, rx) = RingBuffer::new(PLAN_QUEUE);
    let (ret, back) = RingBuffer::new(RETURN_QUEUE);
    (
        Mixer {
            plan: None,
            rx,
            ret,
            meters,
        },
        MixerHandle {
            tx,
            ret: back,
            stash: None,
        },
    )
}
impl MixerHandle {
    /// rtrb push Box<MixPlan>; old boxes come back on a 2nd rtrb and drop here.
    /// If the audio thread is not consuming and the queue is full, the newest plan is kept
    /// aside (replacing any older one) and pushed on the next `flush`/`set_plan`.
    pub fn set_plan(&mut self, p: MixPlan) {
        self.stash = Some(Box::new(p));
        self.flush();
    }

    /// Drop plans returned by the audio thread and push the stashed plan if there is room.
    pub fn flush(&mut self) {
        while self.ret.pop().is_ok() {}
        if let Some(p) = self.stash.take()
            && let Err(rtrb::PushError::Full(p)) = self.tx.push(p)
        {
            self.stash = Some(p);
        }
    }
}
impl Mixer {
    fn poll(&mut self) {
        while let Ok(mut new) = self.rx.pop() {
            if let Some(old) = self.plan.take() {
                new.inherit(&old);
                let _ = self.ret.push(old); // full return queue: drops here, cannot happen by sizing
            }
            self.plan = Some(new);
        }
    }

    /// Planar; zero outputs first; no plan = silence. Mismatched buffer counts/lengths vs the plan's
    /// layout give silence and bump `Meters::mismatch_count`. More than MAX_FRAMES frames run in chunks.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize) {
        for out in outputs.iter_mut() {
            out.fill(0.0);
        }
        self.poll();
        let Some(plan) = self.plan.as_deref_mut() else {
            return;
        };
        if inputs.len() != plan.in_len
            || outputs.len() != plan.out_len
            || inputs.iter().any(|s| s.len() < frames)
            || outputs.iter().any(|s| s.len() < frames)
        {
            self.meters.mismatch_count.fetch_add(1, Relaxed);
            return;
        }
        let mut off = 0;
        while off < frames {
            let n = (frames - off).min(MAX_FRAMES);
            plan.run(inputs, outputs, off, n, &self.meters);
            off += n;
        }
    }
}
