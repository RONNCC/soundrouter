use crate::config::{MAX_CH, MAX_DB, MAX_FRAMES, SourceKind, VirtualDevice};
use crate::meters::{METER_SLOTS, Meters, peak, peak_finite};
use rtrb::{Consumer, Producer, RingBuffer};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ptr::null_mut;
use std::sync::atomic::Ordering::{AcqRel, Relaxed};
use std::sync::atomic::{AtomicPtr, AtomicU32};
use std::sync::{Arc, Mutex};

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
        // NaN is silence; +inf and anything above +24 dB clamp so a bad value cannot blow up the output.
        let target = if off || db.is_nan() {
            0.0
        } else {
            10f32.powf(db.min(MAX_DB) / 20.0)
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
    /// Index into `MixPlan::apps` for an app source: its level also follows V's system volume (`SysGain`).
    app: Option<usize>,
}

/// What V's volume and mute (what the volume keys move while V is the system output) do to each app source,
/// as one linear gain per source id: V's gain while the app plays to V, unity otherwise. The engine's
/// listeners publish it; the audio thread loads one atomic per app source per block.
/// The driver applies the control to what apps play into V; app taps never pass through V, so the mixer
/// applies it to them. Unity until published.
#[derive(Default)]
pub struct SysGain(Mutex<HashMap<String, Arc<AtomicU32>>>);
impl SysGain {
    pub fn set(&self, source: &str, gain: f32) {
        self.cell(source).store(gain.to_bits(), Relaxed);
    }
    fn cell(&self, source: &str) -> Arc<AtomicU32> {
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        m.entry(source.to_owned())
            .or_insert_with(|| Arc::new(AtomicU32::new(1.0f32.to_bits())))
            .clone()
    }
}

/// One app source's `SysGain` cell as the plan applies it, ramped like every gain.
struct AppSys {
    key: u64,
    id: String,
    cell: Arc<AtomicU32>,
    cur: f32,
    target: f32,
}
impl AppSys {
    /// 0..=1; a NaN or negative value reads as unity or silence so a bad publish cannot blow up the output.
    fn read(&self) -> f32 {
        let g = f32::from_bits(self.cell.load(Relaxed));
        if g.is_nan() { 1.0 } else { g.clamp(0.0, 1.0) }
    }
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
    /// App sources, see [`AppSys`].
    apps: Vec<AppSys>,
    dev_out: Vec<(usize, usize)>, // (vdev ch, output slot)
    mons: Vec<MonPlan>,
    meter_in: Vec<usize>,
    acc: Vec<f32>, // nch * MAX_FRAMES vdev channel scratch
    /// Every `acc` row provably holds finite samples, so its meter can skip the finite check.
    acc_finite: bool,
    /// Hash of which input reaches which output (wires, monitors); gains do not count. Two plans with
    /// different keys cannot be joined by a gain ramp, so the mixer crossfades them.
    route: u64,
}

/// Taps into one vdev channel up to which `acc` stays finite: each tap adds at most
/// `SAMPLE_LIMIT` x 15.85 (+24 dB source gain) and the device stage multiplies by at most
/// 15.85 more, so 2^16 x 1e30 x 15.85^2 = 1.6e37 < f32::MAX (3.4e38). Valid configs have <= 64.
const MAX_FINITE_TAPS: usize = 1 << 16;

/// Finite samples are clamped here so that even 64 taps through three +24 dB stages (x15.85 each) cannot overflow to inf.
const SAMPLE_LIMIT: f32 = 1e30;

/// NaN/Inf samples count as silence so one bad source cannot poison the whole device.
fn clean(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-SAMPLE_LIMIT, SAMPLE_LIMIT)
    } else {
        0.0
    }
}

fn ramp_add(dst: &mut [f32], src: &[f32], from: f32, to: f32) {
    if from == to {
        if to != 0.0 {
            dst.iter_mut()
                .zip(src)
                .for_each(|(d, s)| *d += clean(*s) * to);
        }
        return;
    }
    // from + step*k, not a running sum: f32 drift over a block would show up as a step at the next block boundary.
    let step = (to - from) / dst.len() as f32;
    for (k, (d, s)) in dst.iter_mut().zip(src).enumerate() {
        *d += clean(*s) * (from + step * (k + 1) as f32);
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
    for (k, x) in buf.iter_mut().enumerate() {
        *x *= from + step * (k + 1) as f32;
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
        let apps: Vec<AppSys> = dev
            .sources
            .iter()
            .filter(|s| matches!(s.kind, SourceKind::App { .. }))
            .map(|s| AppSys {
                key: Gain::new(&s.id, 0.0, false).key,
                id: s.id.clone(),
                cell: Arc::new(AtomicU32::new(1.0f32.to_bits())),
                cur: 1.0,
                target: 1.0,
            })
            .collect();
        let mut taps = Vec::new();
        // A wire listed twice would sum twice (+6 dB): hand-edited configs and presets can hold one.
        let mut seen = HashSet::new();
        for w in &dev.wires {
            if !seen.insert((&w.source, w.src_ch, w.out_ch)) {
                continue;
            }
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
                    taps.push(Tap {
                        in_idx,
                        vch,
                        src,
                        app: apps.iter().position(|a| a.id == dev.sources[src].id),
                    });
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
        let mons: Vec<MonPlan> = dev
            .monitors
            .iter()
            .map(|m| {
                let mut wires = Vec::new();
                let mut seen = HashSet::new();
                for mw in &m.wires {
                    if usize::from(mw.out_ch) >= nch || !seen.insert((mw.out_ch, mw.dst_ch)) {
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
        let acc_finite =
            (0..nch).all(|c| taps.iter().filter(|t| t.vch == c).count() <= MAX_FINITE_TAPS);
        let mut h = DefaultHasher::new();
        for t in &taps {
            (t.in_idx, t.vch, srcs[t.src].key).hash(&mut h);
        }
        dev_out.hash(&mut h);
        for m in &mons {
            (m.gain.key, &m.wires).hash(&mut h);
        }
        MixPlan {
            in_len: layout.inputs.len(),
            out_len: layout.outputs.len(),
            nch,
            srcs,
            taps,
            dev_gain: Gain::new("dev", dev.volume_db, dev.muted),
            apps,
            dev_out,
            mons,
            meter_in,
            acc: vec![0.0; nch * MAX_FRAMES],
            acc_finite,
            route: h.finish(),
        }
    }

    /// Continue gain ramps from what `old` last applied.
    fn inherit(&mut self, old: &MixPlan) {
        self.srcs.iter_mut().for_each(|g| g.inherit(&old.srcs));
        self.dev_gain.cur = old.dev_gain.cur;
        for a in &mut self.apps {
            if let Some(o) = old.apps.iter().find(|o| o.key == a.key) {
                a.cur = o.cur;
            }
        }
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
            let (s0, s1) = t
                .app
                .map_or((1.0, 1.0), |i| (self.apps[i].cur, self.apps[i].target));
            ramp_add(
                &mut self.acc[t.vch * MAX_FRAMES..][..n],
                &inputs[t.in_idx][off..end],
                g.cur * s0,
                g.target * s1,
            );
        }
        for a in &mut self.apps {
            a.cur = a.target;
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
            let p = if self.acc_finite {
                peak_finite(ch)
            } else {
                peak(ch)
            };
            meters.update_output(c, p);
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

// Boxes outstanding on the return ring: at most 1 per `set_plan`, and `set_plan` drains it first.
const RETURN_QUEUE: usize = 4;

/// Single-slot plan mailbox: a newer plan replaces an unread older one, so the audio thread
/// always sees the newest plan no matter how long it was not consuming.
struct Mailbox(AtomicPtr<MixPlan>);
impl Mailbox {
    fn swap(&self, new: *mut MixPlan) -> Option<Box<MixPlan>> {
        let old = self.0.swap(new, AcqRel);
        // SAFETY: the slot only ever holds null or a pointer from `Box::into_raw` in `put`,
        // and `swap` hands each stored pointer out exactly once.
        (!old.is_null()).then(|| unsafe { Box::from_raw(old) })
    }
    /// Returns the plan it replaced, if the audio thread had not taken it yet.
    fn put(&self, p: Box<MixPlan>) -> Option<Box<MixPlan>> {
        self.swap(Box::into_raw(p))
    }
    fn take(&self) -> Option<Box<MixPlan>> {
        self.swap(null_mut())
    }
}
impl Drop for Mailbox {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Audio-thread side; zero alloc in process.
pub struct Mixer {
    plan: Option<Box<MixPlan>>,
    /// The plan a newly applied one replaced when their routing differs: it runs once more, over the
    /// first block, and the two outputs are crossfaded (a wire or monitor that appears or goes away
    /// would otherwise step).
    fade: Option<Box<MixPlan>>,
    /// `MAX_CH` rows of `MAX_FRAMES` for the fading plan's output.
    fade_buf: Vec<f32>,
    slot: Arc<Mailbox>,
    ret: Producer<Box<MixPlan>>,
    meters: Arc<Meters>,
}
/// Control side.
pub struct MixerHandle {
    slot: Arc<Mailbox>,
    ret: Consumer<Box<MixPlan>>,
    sys: Arc<SysGain>,
}
pub fn mixer(meters: Arc<Meters>) -> (Mixer, MixerHandle) {
    let slot = Arc::new(Mailbox(AtomicPtr::new(null_mut())));
    let (ret, back) = RingBuffer::new(RETURN_QUEUE);
    let sys = Arc::new(SysGain::default());
    (
        Mixer {
            plan: None,
            fade: None,
            fade_buf: vec![0.0; MAX_CH * MAX_FRAMES],
            slot: slot.clone(),
            ret,
            meters,
        },
        MixerHandle {
            slot,
            ret: back,
            sys,
        },
    )
}
impl MixerHandle {
    /// Publish `p`; the audio thread applies it at its next block, replacing any plan it has
    /// not picked up yet. Plans the audio thread retired come back on a ring and drop here.
    pub fn set_plan(&mut self, mut p: MixPlan) {
        self.flush();
        for a in &mut p.apps {
            a.cell = self.sys.cell(&a.id);
            a.target = a.read();
            a.cur = a.target;
        }
        drop(self.slot.put(Box::new(p)));
    }

    /// Where the control side publishes V's volume/mute gain per app source.
    pub fn sys_gain(&self) -> Arc<SysGain> {
        self.sys.clone()
    }

    /// Drop plans returned by the audio thread.
    pub fn flush(&mut self) {
        while self.ret.pop().is_ok() {}
    }
}
impl Mixer {
    fn poll(&mut self) {
        if let Some(mut new) = self.slot.take() {
            if let Some(stale) = self.fade.take() {
                let _ = self.ret.push(stale); // a fade the block loop never reached
            }
            if let Some(old) = self.plan.take() {
                new.inherit(&old);
                if old.route != new.route
                    && old.in_len == new.in_len
                    && old.out_len == new.out_len
                    && old.out_len <= MAX_CH
                {
                    self.fade = Some(old);
                } else {
                    let _ = self.ret.push(old); // full return queue: drops here, cannot happen by sizing
                }
            } else {
                // Nothing applied yet: start at V's level, do not fade in from unity.
                new.apps.iter_mut().for_each(|a| a.cur = a.read());
            }
            self.plan = Some(new);
        }
    }

    /// Inputs the current plan expects, after picking up a newly published one; `None` before the first.
    pub fn in_len(&mut self) -> Option<usize> {
        self.poll();
        self.plan.as_ref().map(|p| p.in_len)
    }

    /// Planar; zero outputs first; no plan = silence. Mismatched buffer counts/lengths vs the plan's
    /// layout give silence and bump `Meters::mismatch_count`. More than MAX_FRAMES frames run in chunks.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize) {
        for out in outputs.iter_mut() {
            out.fill(0.0);
        }
        self.poll();
        let Mixer {
            plan,
            fade,
            fade_buf,
            meters,
            ret,
            ..
        } = &mut *self;
        let Some(plan) = plan.as_deref_mut() else {
            return;
        };
        if inputs.len() != plan.in_len
            || outputs.len() != plan.out_len
            || inputs.iter().any(|s| s.len() < frames)
            || outputs.iter().any(|s| s.len() < frames)
        {
            meters.mismatch_count.fetch_add(1, Relaxed);
            return;
        }
        plan.apps.iter_mut().for_each(|a| a.target = a.read());
        let mut off = 0;
        while off < frames {
            let n = (frames - off).min(MAX_FRAMES);
            plan.run(inputs, outputs, off, n, meters);
            if let Some(mut old) = fade.take() {
                crossfade(&mut old, fade_buf, inputs, outputs, n, meters);
                let _ = ret.push(old); // back to the control side, never freed on the audio thread
            }
            off += n;
        }
    }
}

/// The first `n` frames of `new_out` (what the new plan wrote) become a linear crossfade from what the
/// `old` plan writes for the same inputs: the output starts on the old routing and ends on the new one.
fn crossfade(
    old: &mut MixPlan,
    scratch: &mut [f32],
    inputs: &[&[f32]],
    new_out: &mut [&mut [f32]],
    n: usize,
    meters: &Meters,
) {
    let rows = old.out_len;
    let mut refs: [&mut [f32]; MAX_CH] = std::array::from_fn(|_| &mut [][..]);
    for (r, row) in refs
        .iter_mut()
        .zip(scratch.chunks_mut(MAX_FRAMES))
        .take(rows)
    {
        row.fill(0.0);
        *r = row;
    }
    old.run(inputs, &mut refs[..rows], 0, n, meters);
    let inv = 1.0 / n as f32;
    for (new, old) in new_out.iter_mut().zip(&refs[..rows]) {
        for (k, (y, o)) in new[..n].iter_mut().zip(old.iter()).enumerate() {
            *y = *o + (*y - *o) * ((k + 1) as f32 * inv);
        }
    }
}
