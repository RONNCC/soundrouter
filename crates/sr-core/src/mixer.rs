use crate::config::{
    MAX_CH, MAX_DB, MAX_DELAY_CH, MAX_FRAMES, MAX_FX, MAX_NODE_CH, MAX_NODE_CH_TOTAL, MAX_NODES,
    SourceKind, VirtualDevice,
};
use crate::fx::Effect;
use crate::meters::{METER_SLOTS, Meters, node_slots, peak, peak_finite};
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

/// Frames a gain change takes to cross one unit of gain: 10 ms at 48 kHz. A block shorter than this
/// (64-frame buffers are 1.3 ms) moves only part of the way and the next blocks go on, so a mute or a
/// volume jump never steps in less time than that; a longer block ramps across the whole block.
pub(crate) const RAMP_FRAMES: f32 = 480.0;

/// Where a gain that is at `cur` and wants `target` stands after `n` frames.
fn slew(cur: f32, target: f32, n: usize) -> f32 {
    let reach = cur.abs().max(target.abs()).max(1.0) / RAMP_FRAMES * n as f32;
    if (target - cur).abs() <= reach {
        target
    } else {
        cur + reach.copysign(target - cur)
    }
}

/// A linear gain that ramps from `cur` (previously applied) toward `target` across a block, by at most
/// [`RAMP_FRAMES`]'s worth (`next` is where this block ends).
/// `key` identifies it across plans so a swap continues from the old applied value.
struct Gain {
    key: u64,
    cur: f32,
    target: f32,
    next: f32,
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
            next: target,
        }
    }
    fn inherit(&mut self, old: &[Gain]) {
        if let Some(o) = old.iter().find(|o| o.key == self.key) {
            self.cur = o.cur;
        }
    }
    /// The same gain with its polarity flipped; a slew across the flip passes through zero.
    fn inverted(mut self) -> Gain {
        self.cur = -self.cur;
        self.target = -self.target;
        self.next = -self.next;
        self
    }
}

/// Where a tap reads: an aggregate input slot, or a row of `MixPlan::node_buf`.
#[derive(Clone, Copy)]
enum Src {
    Input(usize),
    Row(usize),
}
/// Where a tap adds: a virtual-device channel of `MixPlan::acc`, or a row of `MixPlan::node_buf`.
#[derive(Clone, Copy)]
enum Dst {
    Vdev(usize),
    Row(usize),
}

struct Tap {
    src: Src,
    dst: Dst,
    /// Index into `MixPlan::srcs`: the source's volume, or the fader of the node it reads.
    gain: usize,
    /// This cable's own trim and polarity, keyed by (source, src_ch, to, out_ch); 1.0 for a plain wire.
    wire: Gain,
    /// Index into `MixPlan::apps` for an app source: its level also follows V's system volume (`SysGain`).
    app: Option<usize>,
}

/// A compiled node: `ch` rows of `MixPlan::node_buf` from `row0` (nodes are numbered in topological
/// order, so every row a node reads lies before its own).
struct NodePlan {
    row0: usize,
    ch: usize,
    /// Index into `MixPlan::srcs` of its fader and mute.
    gain: usize,
    /// First `Meters` node slot: config order, so `Meters::take` can name them from the device alone.
    meter: usize,
    /// Position in `dev.nodes`, the slot of its limiter reduction.
    cfg: usize,
    /// The inserts, run in order over the rows after the incoming taps.
    fx: Vec<Effect>,
    /// It was muted (and so skipped) in an earlier block: what the effects remember is stale.
    was_silent: bool,
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
    next: f32,
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
    /// (vdev ch, output slot, scale). The scale is 1/n when n vdev channels feed the same output slot, so
    /// a downmix (two channels onto a mono output) averages instead of adding 6 dB; 1.0 otherwise.
    wires: Vec<(usize, usize, f32)>,
}

/// Precomputed sparse taps (input slot -> vdev channel via source gain) + per-output gains.
pub struct MixPlan {
    in_len: usize,
    out_len: usize,
    nch: usize,
    /// The sources' volumes, then the nodes' faders (config order), each keyed by its id.
    srcs: Vec<Gain>,
    /// Taps into nodes first (grouped per node in `nodes` order, `node_end` bounds each group), then
    /// the taps into virtual-device channels in wire order.
    taps: Vec<Tap>,
    nodes: Vec<NodePlan>,
    /// `taps[..node_end[i]]` ends node `i`'s group; the virtual-device taps start at the last one.
    node_end: Vec<usize>,
    /// Sample rate the plan was compiled for; what filters and delay lines are sized and tuned to.
    rate: f32,
    /// `rows * MAX_FRAMES` node scratch, prefaulted like `acc`.
    node_buf: Vec<f32>,
    /// `(wire key, index into taps)` sorted by key, so `inherit` joins two plans' wire gains in one pass
    /// on the audio thread instead of searching.
    tap_keys: Vec<(u64, usize)>,
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

/// Taps into one vdev channel (or node row) up to which it stays finite: each tap adds at most
/// `SAMPLE_LIMIT` x 15.85 (+24 dB source gain) x 15.85 (+24 dB wire trim) and the device stage
/// multiplies by at most 15.85 more, so 2^16 x 1e30 x 15.85^3 = 2.6e38 < f32::MAX (3.4e38).
/// Valid configs have <= 64.
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

/// Zeroed scratch whose pages are already mapped. Fresh zero pages fault in on the first write, and that
/// write would be the audio thread's: up to 64 faults (about 1.5 us each) after every plan swap.
pub(crate) fn prefaulted(len: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; len];
    // One write per 4 KiB, the smallest page; volatile so the store of a zero over zeroed memory stays.
    for x in v.iter_mut().step_by(1024) {
        // SAFETY: `x` is a valid, aligned `&mut f32`.
        unsafe { std::ptr::write_volatile(x, 0.0) };
    }
    v
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

/// One tap of `n` frames: reads its source (scaled by the source or node fader, the app's system gain and
/// the cable's own trim, each ramped) and adds it to its destination.
#[allow(clippy::too_many_arguments)]
fn mix_tap(
    t: &mut Tap,
    srcs: &[Gain],
    apps: &[AppSys],
    inputs: &[&[f32]],
    (off, end, n): (usize, usize, usize),
    node_buf: &mut [f32],
    acc: &mut [f32],
) {
    let g = &srcs[t.gain];
    let (s0, s1) = t.app.map_or((1.0, 1.0), |i| (apps[i].cur, apps[i].next));
    t.wire.next = slew(t.wire.cur, t.wire.target, n);
    // x * 1.0 is exact, so a plain wire sums bit for bit as it did before wires had a trim.
    let (from, to) = (g.cur * s0 * t.wire.cur, g.next * s1 * t.wire.next);
    match (t.src, t.dst) {
        (Src::Input(i), Dst::Vdev(c)) => ramp_add(
            &mut acc[c * MAX_FRAMES..][..n],
            &inputs[i][off..end],
            from,
            to,
        ),
        (Src::Input(i), Dst::Row(r)) => ramp_add(
            &mut node_buf[r * MAX_FRAMES..][..n],
            &inputs[i][off..end],
            from,
            to,
        ),
        (Src::Row(s), Dst::Vdev(c)) => ramp_add(
            &mut acc[c * MAX_FRAMES..][..n],
            &node_buf[s * MAX_FRAMES..][..n],
            from,
            to,
        ),
        (Src::Row(s), Dst::Row(r)) => {
            // `compile` only keeps taps from an earlier row to a later one.
            let (lo, hi) = node_buf.split_at_mut(r * MAX_FRAMES);
            ramp_add(&mut hi[..n], &lo[s * MAX_FRAMES..][..n], from, to);
        }
    }
    t.wire.cur = t.wire.next;
}

/// A tap whose node ends are still config positions: `compile` orders the nodes before it can say rows.
struct Pending {
    tap: Tap,
    from: Option<(usize, usize)>,
    to: Option<(usize, usize)>,
}

impl MixPlan {
    /// Wires whose source/channel is absent from the layout, or out of range, are dropped, and so are
    /// those that name a node that cannot run (duplicate id, over the caps) or close a loop of nodes
    /// (the back edge of what Kahn's algorithm cannot order): a plan never trusts `Config::validate`.
    /// Source volume/mute -> per-source gain; node fader/mute -> its outgoing wires' gain;
    /// device volume/mute -> vdev gain; monitor volume/enabled -> monitor gain (monitors hear the vdev
    /// post gain). `rate` is the sample rate of the aggregate this plan will run in.
    pub fn compile(dev: &VirtualDevice, layout: &IoLayout, rate: f32) -> MixPlan {
        let nch = usize::from(dev.channels).min(MAX_CH);
        let mut srcs: Vec<Gain> = dev
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
                next: 1.0,
            })
            .collect();
        // The nodes that can run, as config indices: unique id (also among the sources), channels and
        // totals within the caps. `node_srcs` is each one's index in `srcs`.
        let slots = node_slots(&dev.nodes);
        let mut ids: HashSet<&str> = dev.sources.iter().map(|s| s.id.as_str()).collect();
        let mut usable: Vec<usize> = Vec::new();
        let mut total = 0;
        for (i, n) in dev.nodes.iter().enumerate() {
            let c = usize::from(n.channels);
            if usable.len() < MAX_NODES
                && (1..=MAX_NODE_CH).contains(&c)
                && total + c <= MAX_NODE_CH_TOTAL
                && ids.insert(n.id.as_str())
            {
                total += c;
                usable.push(i);
                srcs.push(Gain::new(&n.id, n.gain_db, n.muted));
            }
        }
        // The inserts of each usable node, built in config order so the delay-line budget is spent in a
        // fixed order; those past `MAX_FX` or the budget are not built.
        let mut delay_budget = MAX_DELAY_CH;
        let mut fx_of: Vec<Vec<Effect>> = usable
            .iter()
            .map(|&i| {
                let n = &dev.nodes[i];
                n.fx.iter()
                    .take(MAX_FX)
                    .filter_map(|f| {
                        Effect::new(f, usize::from(n.channels), rate, &mut delay_budget)
                    })
                    .collect()
            })
            .collect();
        let node_of = |id: &str| usable.iter().position(|&i| dev.nodes[i].id == id);
        let node_ch = |k: usize| usize::from(dev.nodes[usable[k]].channels);
        let src_gain = dev.sources.len();
        let mut pending: Vec<Pending> = Vec::new();
        // A wire listed twice would sum twice (+6 dB): hand-edited configs and presets can hold one.
        let mut seen = HashSet::new();
        for w in &dev.wires {
            if !seen.insert((&w.source, w.src_ch, &w.to, w.out_ch)) {
                continue;
            }
            let to = match &w.to {
                None if usize::from(w.out_ch) < nch => None,
                Some(id) => match node_of(id) {
                    Some(k) if usize::from(w.out_ch) < node_ch(k) => {
                        Some((k, usize::from(w.out_ch)))
                    }
                    _ => continue,
                },
                None => continue,
            };
            let wire = || {
                let g = Gain::new((&w.source, w.src_ch, &w.to, w.out_ch), w.gain_db, false);
                if w.invert { g.inverted() } else { g }
            };
            let dst = Dst::Vdev(usize::from(w.out_ch));
            if let Some(src) = dev.sources.iter().position(|s| s.id == w.source) {
                for (in_idx, slot) in layout.inputs.iter().enumerate() {
                    if let Some((id, ch)) = slot
                        && *id == w.source
                        && *ch == w.src_ch
                    {
                        pending.push(Pending {
                            tap: Tap {
                                src: Src::Input(in_idx),
                                dst,
                                gain: src,
                                wire: wire(),
                                app: apps.iter().position(|a| a.id == dev.sources[src].id),
                            },
                            from: None,
                            to,
                        });
                    }
                }
            } else if let Some(k) = node_of(&w.source)
                && usize::from(w.src_ch) < node_ch(k)
            {
                pending.push(Pending {
                    tap: Tap {
                        src: Src::Row(0),
                        dst,
                        gain: src_gain + k,
                        wire: wire(),
                        app: None,
                    },
                    from: Some((k, usize::from(w.src_ch))),
                    to,
                });
            }
        }
        // Topological order of the nodes, ties by config order: Kahn's algorithm over the node-to-node
        // wires. Nodes it cannot order (a loop) follow in config order, and `retain` below drops the
        // wires that then point backwards.
        let n_nodes = usable.len();
        let mut indeg = vec![0usize; n_nodes];
        for p in &pending {
            if let (Some((a, _)), Some((b, _))) = (p.from, p.to)
                && a != b
            {
                indeg[b] += 1;
            }
        }
        let mut order: Vec<usize> = Vec::with_capacity(n_nodes);
        let mut placed = vec![false; n_nodes];
        while let Some(k) = (0..n_nodes).find(|&k| !placed[k] && indeg[k] == 0) {
            placed[k] = true;
            order.push(k);
            for p in &pending {
                if let (Some((a, _)), Some((b, _))) = (p.from, p.to)
                    && a == k
                    && b != k
                {
                    indeg[b] -= 1;
                }
            }
        }
        order.extend((0..n_nodes).filter(|&k| !placed[k]));
        let mut pos = vec![0usize; n_nodes];
        for (i, &k) in order.iter().enumerate() {
            pos[k] = i;
        }
        let mut row0 = vec![0usize; n_nodes]; // by config position among the usable
        let mut rows = 0;
        for &k in &order {
            row0[k] = rows;
            rows += node_ch(k);
        }
        pending.retain(|p| match (p.from, p.to) {
            (Some((a, _)), Some((b, _))) => pos[a] < pos[b],
            _ => true,
        });
        // Taps into nodes first, node by node; the stable sort keeps wire order within each group, and
        // the virtual-device taps keep theirs, so a device without nodes sums in the order it always did.
        pending.sort_by_key(|p| p.to.map_or(usize::MAX, |(k, _)| pos[k]));
        let mut node_end = vec![0usize; n_nodes];
        let mut taps = Vec::with_capacity(pending.len());
        for p in pending {
            let mut t = p.tap;
            if let Some((k, ch)) = p.from {
                t.src = Src::Row(row0[k] + ch);
            }
            if let Some((k, ch)) = p.to {
                t.dst = Dst::Row(row0[k] + ch);
                node_end[pos[k]] = taps.len() + 1;
            }
            taps.push(t);
        }
        // A node without taps inherits its predecessor's end.
        for i in 1..n_nodes {
            node_end[i] = node_end[i].max(node_end[i - 1]);
        }
        let nodes: Vec<NodePlan> = order
            .iter()
            .map(|&k| NodePlan {
                row0: row0[k],
                ch: node_ch(k),
                gain: src_gain + k,
                meter: slots[usable[k]],
                cfg: usable[k],
                fx: std::mem::take(&mut fx_of[k]),
                was_silent: false,
            })
            .collect();
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
                            wires.push((usize::from(mw.out_ch), slot, 1.0));
                        }
                    }
                }
                let fan_in: Vec<usize> = wires
                    .iter()
                    .map(|w| wires.iter().filter(|o| o.1 == w.1).count())
                    .collect();
                for (w, n) in wires.iter_mut().zip(fan_in) {
                    w.2 = 1.0 / n as f32;
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
        let finite_into = |same: &dyn Fn(&Tap) -> bool| {
            taps.iter().filter(|t| same(t)).count() <= MAX_FINITE_TAPS
        };
        let acc_finite =
            (0..nch).all(|c| finite_into(&|t| matches!(t.dst, Dst::Vdev(d) if d == c)));
        let mut tap_keys: Vec<(u64, usize)> = taps
            .iter()
            .enumerate()
            .map(|(i, t)| (t.wire.key, i))
            .collect();
        tap_keys.sort_unstable();
        let mut h = DefaultHasher::new();
        let hash_src = |s: Src| match s {
            Src::Input(i) => (0u8, i),
            Src::Row(r) => (1, r),
        };
        let hash_dst = |d: Dst| match d {
            Dst::Vdev(c) => (0u8, c),
            Dst::Row(r) => (1, r),
        };
        for t in &taps {
            (hash_src(t.src), hash_dst(t.dst), srcs[t.gain].key).hash(&mut h);
        }
        for n in &nodes {
            (srcs[n.gain].key, n.row0, n.ch).hash(&mut h);
            n.fx.iter().for_each(|f| f.tag().hash(&mut h));
        }
        dev_out.hash(&mut h);
        for m in &mons {
            m.gain.key.hash(&mut h);
            for w in &m.wires {
                (w.0, w.1).hash(&mut h);
            }
        }
        MixPlan {
            in_len: layout.inputs.len(),
            out_len: layout.outputs.len(),
            nch,
            srcs,
            taps,
            nodes,
            node_end,
            rate,
            node_buf: prefaulted(rows * MAX_FRAMES),
            tap_keys,
            dev_gain: Gain::new("dev", dev.volume_db, dev.muted),
            apps,
            dev_out,
            mons,
            meter_in,
            acc: prefaulted(nch * MAX_FRAMES),
            acc_finite,
            route: h.finish(),
        }
    }

    /// The sample rate this plan was compiled for.
    pub fn rate(&self) -> f32 {
        self.rate
    }

    /// Continue gain ramps and effect state from what `old` last applied. `keep_old`: `old` still runs one
    /// more block (a crossfade), so its large state is copied instead of swapped.
    fn inherit(&mut self, old: &mut MixPlan, keep_old: bool) {
        self.srcs.iter_mut().for_each(|g| g.inherit(&old.srcs));
        for n in &mut self.nodes {
            let key = self.srcs[n.gain].key;
            if let Some(o) = old
                .nodes
                .iter_mut()
                .find(|o| old.srcs[o.gain].key == key && o.ch == n.ch)
            {
                n.was_silent = o.was_silent;
                for (f, of) in n.fx.iter_mut().zip(&mut o.fx) {
                    if f.tag() == of.tag() {
                        f.inherit(of, keep_old);
                    }
                }
            }
        }
        let (mut i, mut j) = (0, 0);
        while let (Some(&(ki, ti)), Some(&(kj, tj))) = (self.tap_keys.get(i), old.tap_keys.get(j)) {
            match ki.cmp(&kj) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    self.taps[ti].wire.cur = old.taps[tj].wire.cur;
                    i += 1;
                    j += 1;
                }
            }
        }
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
    /// Returns whether a vdev output sample exceeded full scale in this block.
    fn run(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        off: usize,
        n: usize,
        meters: &Meters,
    ) -> bool {
        let end = off + n;
        let span = (off, end, n);
        for g in &mut self.srcs {
            g.next = slew(g.cur, g.target, n);
        }
        for a in &mut self.apps {
            a.next = slew(a.cur, a.target, n);
        }
        self.dev_gain.next = slew(self.dev_gain.cur, self.dev_gain.target, n);
        for m in &mut self.mons {
            m.gain.next = slew(m.gain.cur, m.gain.target, n);
        }
        for c in 0..self.nch {
            self.acc[c * MAX_FRAMES..][..n].fill(0.0);
        }
        let mut start = 0;
        for (node, &stop) in self.nodes.iter_mut().zip(&self.node_end) {
            let g = &self.srcs[node.gain];
            // A node whose mute has settled neither mixes nor meters: its outgoing wires add nothing.
            if g.cur != 0.0 || g.next != 0.0 {
                for r in 0..node.ch {
                    self.node_buf[(node.row0 + r) * MAX_FRAMES..][..n].fill(0.0);
                }
                for t in &mut self.taps[start..stop] {
                    mix_tap(
                        t,
                        &self.srcs,
                        &self.apps,
                        inputs,
                        span,
                        &mut self.node_buf,
                        &mut self.acc,
                    );
                }
                let rows = &mut self.node_buf[node.row0 * MAX_FRAMES..][..node.ch * MAX_FRAMES];
                if node.was_silent {
                    node.fx.iter_mut().for_each(Effect::reset);
                    node.was_silent = false;
                }
                let mut reduction = 0.0f32;
                for f in &mut node.fx {
                    reduction = reduction.max(f.run(rows, node.ch, n));
                }
                if reduction > 0.0 {
                    meters.update_reduction(node.cfg, reduction);
                }
                // What leaves the node: row peak times the fader's larger end of the block.
                let fader = g.cur.abs().max(g.next.abs());
                for r in 0..node.ch {
                    let row = &self.node_buf[(node.row0 + r) * MAX_FRAMES..][..n];
                    meters.update_node(node.meter + r, peak(row) * fader);
                }
            } else {
                node.was_silent = true;
            }
            start = stop;
        }
        for t in &mut self.taps[start..] {
            mix_tap(
                t,
                &self.srcs,
                &self.apps,
                inputs,
                span,
                &mut self.node_buf,
                &mut self.acc,
            );
        }
        for a in &mut self.apps {
            a.cur = a.next;
        }
        for g in &mut self.srcs {
            g.cur = g.next;
        }
        for &i in &self.meter_in {
            meters.update_input(i, peak(&inputs[i][off..end]));
        }
        let mut clipped = false;
        for c in 0..self.nch {
            let ch = &mut self.acc[c * MAX_FRAMES..][..n];
            ramp_scale(ch, self.dev_gain.cur, self.dev_gain.next);
            let p = if self.acc_finite {
                peak_finite(ch)
            } else {
                peak(ch)
            };
            clipped |= p > 1.0;
            meters.update_output(c, p);
        }
        self.dev_gain.cur = self.dev_gain.next;
        for &(c, slot) in &self.dev_out {
            ramp_add(
                &mut outputs[slot][off..end],
                &self.acc[c * MAX_FRAMES..][..n],
                1.0,
                1.0,
            );
        }
        for m in &mut self.mons {
            for &(c, slot, scale) in &m.wires {
                ramp_add(
                    &mut outputs[slot][off..end],
                    &self.acc[c * MAX_FRAMES..][..n],
                    m.gain.cur * scale,
                    m.gain.next * scale,
                );
            }
            m.gain.cur = m.gain.next;
        }
        clipped
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
            // Prefaulted: the first crossfade would otherwise fault these pages in on the audio thread.
            fade_buf: prefaulted(MAX_CH * MAX_FRAMES),
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
            if let Some(mut old) = self.plan.take() {
                let fade = old.route != new.route
                    && old.in_len == new.in_len
                    && old.out_len == new.out_len
                    && old.out_len <= MAX_CH;
                new.inherit(&mut old, fade);
                if fade {
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
            if plan.run(inputs, outputs, off, n, meters) {
                meters.clipped_blocks.fetch_add(1, Relaxed);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{VirtualDevice, Wire};

    fn dev(wires: Vec<Wire>) -> VirtualDevice {
        let src = |id: &str| crate::config::Source {
            id: id.into(),
            kind: SourceKind::PassThru,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            mute_original: false,
            name: String::new(),
        };
        VirtualDevice {
            id: "v".into(),
            name: "V".into(),
            enabled: true,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            sources: vec![src("a"), src("b")],
            wires,
            monitors: vec![],
            nodes: Vec::new(),
        }
    }

    fn layout() -> IoLayout {
        IoLayout {
            inputs: ["a", "b"]
                .iter()
                .flat_map(|s| (0..2).map(move |c| Some((s.to_string(), c))))
                .collect(),
            outputs: vec![Some(OutPort::Device(0)), Some(OutPort::Device(1))],
        }
    }

    fn trimmed(source: &str, src_ch: u16, out_ch: u16, gain_db: f32) -> Wire {
        Wire {
            gain_db,
            ..Wire::new(source, src_ch, out_ch)
        }
    }

    /// Wire gains are matched by key whatever the tap order and whichever wires came or went.
    #[test]
    fn inherit_joins_wire_gains_by_key_across_reordered_and_removed_wires() {
        let mut old = MixPlan::compile(
            &dev(vec![
                trimmed("a", 0, 0, -6.0),
                trimmed("a", 1, 1, -12.0),
                trimmed("b", 0, 0, -18.0),
                trimmed("b", 1, 1, 6.0),
            ]),
            &layout(),
            48_000.0,
        );
        for (t, cur) in old.taps.iter_mut().zip([0.5f32, 0.25, 0.125, 2.0]) {
            t.wire.cur = cur;
        }
        let mut new = MixPlan::compile(
            &dev(vec![
                trimmed("b", 1, 1, 0.0),
                trimmed("a", 0, 0, 0.0),
                trimmed("c", 0, 0, 0.0), // unknown source: no tap at all
                trimmed("b", 0, 0, 0.0),
            ]),
            &layout(),
            48_000.0,
        );
        new.inherit(&mut old, false);
        let cur: Vec<f32> = new.taps.iter().map(|t| t.wire.cur).collect();
        assert_eq!(
            cur,
            [2.0, 0.5, 0.125],
            "b1, a0, b0 follow the old plan, a1 is gone"
        );
        assert_eq!(
            new.taps.iter().map(|t| t.wire.target).collect::<Vec<_>>(),
            [1.0; 3]
        );
    }
}
