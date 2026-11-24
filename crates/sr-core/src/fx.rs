//! The built-in effects of a mixer node (`docs/NODES.md` section 2): mono, limiter, EQ and delay.
//!
//! Every effect works in place on a node's rows (`ch` rows `MAX_FRAMES` apart), never allocates, never
//! locks, and is skipped or bit-identical while neutral. Parameters that change move at the mixer's own
//! rate: a dB value at 36 dB, a frequency at 10 octaves per `RAMP_FRAMES` frames, so a slider drag never
//! steps. State (filter memory, delay lines, the parameters' current values) is continued across plan
//! swaps by [`Effect::inherit`].
use crate::config::{Fx, MAX_DELAY_MS, MAX_FRAMES, MAX_NODE_CH};
use crate::mixer::RAMP_FRAMES;

/// A state below this magnitude is zeroed after every block (subnormals are slow on some targets, and
/// a decaying tail would otherwise sit in that range for a long time).
const FLUSH: f32 = 1e-20;
/// A filter memory this large is not audio (the mixer clamps samples to 1e30 and a real signal is below
/// 1): it is the echo of a burst, and zeroed so the next block starts clean instead of ringing for seconds.
const BLOWUP: f32 = 1e20;
const DB_PER_RAMP: f32 = 36.0;
const OCTAVES_PER_RAMP: f32 = 10.0;

/// Moves `cur` toward `target` by at most `step`.
fn approach(cur: f32, target: f32, step: f32) -> f32 {
    if (target - cur).abs() <= step {
        target
    } else {
        cur + step.copysign(target - cur)
    }
}

/// A parameter that glides: the value applied so far and the one it is heading for.
#[derive(Clone, Copy)]
struct Param {
    cur: f32,
    target: f32,
}
impl Param {
    fn at(v: f32) -> Param {
        Param { cur: v, target: v }
    }
    fn settled(&self) -> bool {
        self.cur == self.target
    }
}

/// `x` if it is a number, else `neutral`; then clamped into `lo..=hi`. A NaN parameter is "neutral".
fn param(x: f32, neutral: f32, lo: f32, hi: f32) -> f32 {
    if x.is_nan() { neutral } else { x.clamp(lo, hi) }
}

pub(crate) enum Effect {
    Mono,
    Limiter(Limiter),
    Eq(Eq),
    Delay(Delay),
}

impl Effect {
    /// `rate` in Hz; `ch` the node's channel count. `delay_budget` is how many more delay channels the
    /// device may still allocate: an over-budget delay is built as no effect at all (`None`).
    pub(crate) fn new(fx: &Fx, ch: usize, rate: f32, delay_budget: &mut usize) -> Option<Effect> {
        // `compile` takes the rate on trust: a nonsense one must not make a range empty or a ring huge.
        let rate = if rate.is_finite() {
            rate.clamp(8_000.0, 384_000.0)
        } else {
            48_000.0
        };
        Some(match *fx {
            Fx::Mono => Effect::Mono,
            Fx::Limiter {
                ceiling_db,
                knee_db,
            } => Effect::Limiter(Limiter {
                ceil: Param::at(param(ceiling_db, 0.0, -24.0, 0.0)),
                knee: Param::at(param(knee_db, 6.0, 0.0, 12.0)),
            }),
            Fx::Eq {
                low_hz,
                low_db,
                high_hz,
                high_db,
            } => {
                let top = 0.45 * rate;
                Effect::Eq(Eq {
                    low: Band::new(false, low_hz, low_db, 120.0, 20.0, 2000f32.min(top)),
                    high: Band::new(true, high_hz, high_db, 8000.0, 1000.0, 20000f32.min(top)),
                    rate,
                })
            }
            Fx::Delay { ms } => {
                if ch > *delay_budget {
                    return None;
                }
                *delay_budget -= ch;
                Effect::Delay(Delay::new(ch, rate, param(ms, 0.0, 0.0, MAX_DELAY_MS)))
            }
        })
    }

    /// What kind of effect this is, for the route hash and for deciding whose state continues.
    pub(crate) fn tag(&self) -> u8 {
        match self {
            Effect::Mono => 0,
            Effect::Limiter(_) => 1,
            Effect::Eq(_) => 2,
            Effect::Delay(_) => 3,
        }
    }

    /// One block of `n` frames over the node's `ch` rows. Returns the largest gain reduction `1 - |y|/|x|`
    /// a limiter applied (0 for every other effect).
    pub(crate) fn run(&mut self, rows: &mut [f32], ch: usize, n: usize) -> f32 {
        match self {
            Effect::Mono => {
                mono(rows, ch, n);
                0.0
            }
            Effect::Limiter(l) => l.run(rows, ch, n),
            Effect::Eq(e) => {
                e.run(rows, ch, n);
                0.0
            }
            Effect::Delay(d) => {
                d.run(rows, ch, n);
                0.0
            }
        }
    }

    /// Forget everything the signal left behind (the node was muted, so the state is stale).
    pub(crate) fn reset(&mut self) {
        match self {
            Effect::Eq(e) => {
                e.low.reset();
                e.high.reset();
            }
            Effect::Delay(d) => d.reset(),
            Effect::Mono | Effect::Limiter(_) => {}
        }
    }

    /// Continue `old`'s state (same kind, same shape; anything else starts cold). When `keep_old` the old
    /// plan still has to run one more block (a crossfade), so large state is copied; otherwise it is
    /// swapped, which is O(1).
    pub(crate) fn inherit(&mut self, old: &mut Effect, keep_old: bool) {
        match (self, old) {
            (Effect::Limiter(a), Effect::Limiter(b)) => {
                a.ceil.cur = b.ceil.cur;
                a.knee.cur = b.knee.cur;
            }
            (Effect::Eq(a), Effect::Eq(b)) => {
                a.low.inherit(&b.low);
                a.high.inherit(&b.high);
            }
            (Effect::Delay(a), Effect::Delay(b)) if a.ch == b.ch && a.cap == b.cap => {
                if keep_old {
                    a.buf.copy_from_slice(&b.buf);
                } else {
                    std::mem::swap(&mut a.buf, &mut b.buf);
                }
                (a.write, a.d, a.from, a.fade) = (b.write, b.d, b.from, b.fade);
            }
            _ => {}
        }
    }
}

/// Folds each channel pair (0,1), (2,3), ... to its average; an odd last channel passes. Averaging, not
/// summing, so a signal that is already mono keeps its level.
fn mono(rows: &mut [f32], ch: usize, n: usize) {
    for p in 0..ch / 2 {
        let (l, r) = rows[2 * p * MAX_FRAMES..].split_at_mut(MAX_FRAMES);
        for (l, r) in l[..n].iter_mut().zip(&mut r[..n]) {
            let m = 0.5 * (*l + *r);
            (*l, *r) = (m, m);
        }
    }
}

// ---- limiter -------------------------------------------------------------------------------------------

/// Zero look-ahead soft clip: below the knee the sample is untouched, above it a rational tanh-like curve
/// takes it to the ceiling, exactly reached at three knee widths over. Not a mastering limiter.
pub(crate) struct Limiter {
    ceil: Param,
    knee: Param,
}
impl Limiter {
    fn run(&mut self, rows: &mut [f32], ch: usize, n: usize) -> f32 {
        let step = DB_PER_RAMP * n as f32 / RAMP_FRAMES;
        let ceil_next = approach(self.ceil.cur, self.ceil.target, step);
        let knee_next = approach(self.knee.cur, self.knee.target, step);
        let lin = |c_db: f32, k_db: f32| {
            let c = 10f32.powf(c_db / 20.0);
            (c, c * 10f32.powf(-k_db / 20.0))
        };
        let (c0, k0) = lin(self.ceil.cur, self.knee.cur);
        let (c1, k1) = lin(ceil_next, knee_next);
        let (dc, dk) = ((c1 - c0) / n as f32, (k1 - k0) / n as f32);
        (self.ceil.cur, self.knee.cur) = (ceil_next, knee_next);
        let mut reduction = 0.0f32;
        for r in 0..ch {
            for (i, x) in rows[r * MAX_FRAMES..][..n].iter_mut().enumerate() {
                let a = x.abs();
                // Linear gliding of the two limits across the block keeps a moving ceiling click-free.
                let k = k0 + dk * (i + 1) as f32;
                if a <= k {
                    continue;
                }
                let c = c0 + dc * (i + 1) as f32;
                let u = ((a - k) / (c - k)).min(3.0);
                let y = k + (c - k) * (u * (27.0 + u * u) / (27.0 + 9.0 * u * u));
                reduction = reduction.max(1.0 - y / a);
                *x = y.copysign(*x);
            }
        }
        reduction
    }
}

/// The soft clip of `x` for a given ceiling and knee in linear gain; the reference the tests use.
#[cfg(test)]
fn soft_clip(x: f32, c: f32, k: f32) -> f32 {
    let a = x.abs();
    if a <= k {
        return x;
    }
    let u = ((a - k) / (c - k)).min(3.0);
    (k + (c - k) * (u * (27.0 + u * u) / (27.0 + 9.0 * u * u))).copysign(x)
}

// ---- EQ ------------------------------------------------------------------------------------------------

/// Two RBJ shelves in series (slope 1), transposed direct form II in f32, coefficients in f64.
pub(crate) struct Eq {
    low: Band,
    high: Band,
    rate: f32,
}
struct Band {
    high: bool,
    hz: Param,
    db: Param,
    lo: f32,
    hi: f32,
    /// b0, b1, b2, a1, a2 (a0 divided out).
    coef: [f32; 5],
    /// Coefficients match `hz.cur` and `db.cur`.
    fresh: bool,
    z: [[f32; 2]; MAX_NODE_CH],
}
impl Band {
    fn new(high: bool, hz: f32, db: f32, default_hz: f32, lo: f32, hi: f32) -> Band {
        Band {
            high,
            hz: Param::at(param(hz, default_hz, lo, hi)),
            db: Param::at(param(db, 0.0, -18.0, 18.0)),
            lo,
            hi,
            coef: [1.0, 0.0, 0.0, 0.0, 0.0],
            fresh: false,
            z: [[0.0; 2]; MAX_NODE_CH],
        }
    }
    fn reset(&mut self) {
        self.z = [[0.0; 2]; MAX_NODE_CH];
    }
    fn inherit(&mut self, old: &Band) {
        self.hz.cur = old.hz.cur;
        self.db.cur = old.db.cur;
        self.z = old.z;
        self.fresh = false; // the rate may differ: tune again
    }

    /// RBJ cookbook shelf at `hz` (clamped below Nyquist) with `db` of gain.
    fn tune(&mut self, rate: f32) {
        let a = 10f64.powf(f64::from(self.db.cur) / 40.0);
        let w0 = 2.0 * std::f64::consts::PI * f64::from(self.hz.cur) / f64::from(rate);
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / 2.0 * std::f64::consts::SQRT_2;
        let t = 2.0 * a.sqrt() * alpha;
        let (am, ap) = (a - 1.0, a + 1.0);
        let (b0, b1, b2, a0, a1, a2) = if self.high {
            (
                a * (ap + am * cos + t),
                -2.0 * a * (am + ap * cos),
                a * (ap + am * cos - t),
                ap - am * cos + t,
                2.0 * (am - ap * cos),
                ap - am * cos - t,
            )
        } else {
            (
                a * (ap - am * cos + t),
                2.0 * a * (am - ap * cos),
                a * (ap - am * cos - t),
                ap + am * cos + t,
                -2.0 * (am + ap * cos),
                ap + am * cos - t,
            )
        };
        self.coef = [b0, b1, b2, a1, a2].map(|c| (c / a0) as f32);
        self.fresh = true;
    }

    fn run(&mut self, rows: &mut [f32], ch: usize, n: usize, rate: f32) {
        let n_f = n as f32 / RAMP_FRAMES;
        let db = approach(self.db.cur, self.db.target, DB_PER_RAMP * n_f);
        if db == 0.0 && self.db.target == 0.0 {
            // 0 dB settled is the identity: skipped, so the signal is bit-identical. Its memory is zero
            // so a later boost starts from nothing, which an identity filter agrees with.
            self.db.cur = 0.0;
            self.hz.cur = self.hz.target;
            self.reset();
            self.fresh = false;
            return;
        }
        let hz = if self.hz.settled() {
            self.hz.cur
        } else {
            let step = OCTAVES_PER_RAMP * n_f;
            let (from, to) = (self.hz.cur.log2(), self.hz.target.log2());
            match approach(from, to, step) {
                l if l == to => self.hz.target,
                l => l.exp2(),
            }
        };
        if !self.fresh || db != self.db.cur || hz != self.hz.cur {
            (self.db.cur, self.hz.cur) = (db, hz.clamp(self.lo, self.hi));
            self.tune(rate);
        }
        let [b0, b1, b2, a1, a2] = self.coef;
        for (c, z) in self.z.iter_mut().enumerate().take(ch) {
            let [mut z1, mut z2] = *z;
            for x in &mut rows[c * MAX_FRAMES..][..n] {
                let y = b0 * *x + z1;
                z1 = b1 * *x - a1 * y + z2;
                z2 = b2 * *x - a2 * y;
                *x = y;
            }
            // A few compares per state per block: an inf burst cannot latch a NaN in the memory, a 1e30
            // burst does not ring on, and a decaying tail does not linger in the subnormal range.
            let tidy = |z: f32| {
                if (FLUSH..BLOWUP).contains(&z.abs()) {
                    z
                } else {
                    0.0
                }
            };
            *z = [tidy(z1), tidy(z2)];
        }
    }
}
impl Eq {
    fn run(&mut self, rows: &mut [f32], ch: usize, n: usize) {
        self.low.run(rows, ch, n, self.rate);
        self.high.run(rows, ch, n, self.rate);
    }
}

// ---- delay ---------------------------------------------------------------------------------------------

/// Sample-accurate delay of up to `MAX_DELAY_MS`: one ring per channel, allocated at its largest size so
/// a retime never allocates and the ring can be handed to the next plan. A retime crossfades the old and
/// the new read position over `RAMP_FRAMES` frames, so dragging the value neither clicks nor bends pitch.
pub(crate) struct Delay {
    ch: usize,
    /// Frames per channel: the longest delay plus one block, so a block can be written before it is read.
    cap: usize,
    /// `ch * cap` samples.
    buf: Vec<f32>,
    write: usize,
    /// Delay of the position being heard, in frames; `from` is the one fading out for `fade` more frames.
    d: usize,
    from: usize,
    fade: usize,
    /// Where the retime is heading, in frames.
    target: usize,
}
impl Delay {
    fn new(ch: usize, rate: f32, ms: f32) -> Delay {
        let rate = rate.clamp(8_000.0, 384_000.0);
        let longest = (MAX_DELAY_MS / 1000.0 * rate).ceil() as usize;
        let cap = longest + MAX_FRAMES;
        let d = ((ms * rate / 1000.0).round() as usize).min(longest);
        Delay {
            ch,
            cap,
            buf: crate::mixer::prefaulted(ch * cap),
            write: 0,
            d,
            from: d,
            fade: 0,
            target: d,
        }
    }

    /// The frames of the delay the plan was compiled with, for the UI's latency readout and the tests.
    #[cfg(test)]
    fn frames(&self) -> usize {
        self.target
    }

    fn reset(&mut self) {
        self.buf.fill(0.0);
        (self.d, self.from, self.fade) = (self.target, self.target, 0);
    }

    fn run(&mut self, rows: &mut [f32], ch: usize, n: usize) {
        // `compile` made `target`; `inherit` kept where the heard position actually is.
        if self.fade == 0 && self.d != self.target {
            (self.from, self.d, self.fade) = (self.d, self.target, RAMP_FRAMES as usize);
        }
        let (cap, w, d, from, fade) = (self.cap, self.write, self.d, self.from, self.fade);
        for c in 0..ch.min(self.ch) {
            let ring = &mut self.buf[c * cap..][..cap];
            let x = &mut rows[c * MAX_FRAMES..][..n];
            let first = n.min(cap - w);
            ring[w..w + first].copy_from_slice(&x[..first]);
            ring[..n - first].copy_from_slice(&x[first..]);
            if fade == 0 {
                if d == 0 {
                    continue; // neutral: the line only listens, the signal is untouched
                }
                let r = (w + cap - d) % cap;
                let first = n.min(cap - r);
                x[..first].copy_from_slice(&ring[r..r + first]);
                x[first..].copy_from_slice(&ring[..n - first]);
            } else {
                for (i, y) in x.iter_mut().enumerate() {
                    let a = ((RAMP_FRAMES as usize - fade + i + 1) as f32 / RAMP_FRAMES).min(1.0);
                    let (new, old) = (
                        ring[(w + i + cap - d) % cap],
                        ring[(w + i + cap - from) % cap],
                    );
                    *y = new * a + old * (1.0 - a);
                }
            }
        }
        self.write = (w + n) % cap;
        self.fade = fade.saturating_sub(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference for the limiter curve in f64.
    fn clip64(x: f64, c: f64, k: f64) -> f64 {
        let a = x.abs();
        if a <= k {
            return x;
        }
        let u = ((a - k) / (c - k)).min(3.0);
        (k + (c - k) * (u * (27.0 + u * u) / (27.0 + 9.0 * u * u))).copysign(x)
    }

    fn rows_of(ch: usize, n: usize, f: impl Fn(usize, usize) -> f32) -> Vec<f32> {
        let mut v = vec![0.0; ch * MAX_FRAMES];
        for c in 0..ch {
            for i in 0..n {
                v[c * MAX_FRAMES + i] = f(c, i);
            }
        }
        v
    }

    fn fx(fx: Fx, ch: usize, rate: f32) -> Effect {
        Effect::new(&fx, ch, rate, &mut 8).unwrap()
    }

    #[test]
    fn mono_averages_pairs_and_passes_an_odd_last_channel() {
        let mut e = fx(Fx::Mono, 3, 48_000.0);
        let mut r = rows_of(3, 4, |c, i| [1.0, 3.0, 5.0][c] + i as f32);
        e.run(&mut r, 3, 4);
        assert_eq!(r[..4], [2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r[MAX_FRAMES..][..4], [2.0, 3.0, 4.0, 5.0]);
        assert_eq!(r[2 * MAX_FRAMES..][..4], [5.0, 6.0, 7.0, 8.0]);
        // already mono: bit-identical
        let mut e = fx(Fx::Mono, 2, 48_000.0);
        let mut r = rows_of(2, 64, |_, i| (i as f32 * 0.37).sin());
        let want = r.clone();
        e.run(&mut r, 2, 64);
        assert_eq!(r, want);
    }

    #[test]
    fn the_limiter_is_the_reference_curve_bounded_continuous_and_untouched_below_the_knee() {
        let (c_db, k_db) = (-3.0f32, 6.0f32);
        let mut e = fx(
            Fx::Limiter {
                ceiling_db: c_db,
                knee_db: k_db,
            },
            1,
            48_000.0,
        );
        let c = 10f32.powf(c_db / 20.0);
        let k = c * 10f32.powf(-k_db / 20.0);
        let xs: Vec<f32> = (-2000..=2000).map(|i| i as f32 * 0.005).collect();
        let n = xs.len().min(MAX_FRAMES);
        let mut r = rows_of(1, n, |_, i| xs[i]);
        e.run(&mut r, 1, n);
        for (x, y) in xs.iter().zip(&r) {
            assert!(
                y.abs() <= c * (1.0 + 1e-6),
                "{x} -> {y} over the ceiling {c}"
            );
            if x.abs() <= k {
                assert_eq!(x.to_bits(), y.to_bits(), "below the knee is bit-identical");
            }
            let want = clip64(f64::from(*x), f64::from(c), f64::from(k));
            assert!((f64::from(*y) - want).abs() < 1e-5, "{x}: {y} vs {want}");
            assert_eq!(*y, soft_clip(*x, c, k));
        }
        // monotone, and exactly at the ceiling three knee widths over
        assert!(r.windows(2).take(n - 1).all(|w| w[1] >= w[0]));
        let at = k + 3.0 * (c - k);
        assert!((soft_clip(at, c, k) - c).abs() < 1e-7 && (soft_clip(10.0, c, k) - c).abs() < 1e-7);
        // slope 1 at the knee
        let e = 1e-4;
        let slope = (soft_clip(k + e, c, k) - k) / e;
        assert!((slope - 1.0).abs() < 1e-2, "{slope}");
    }

    #[test]
    fn the_limiter_reports_its_largest_reduction_and_a_hard_knee_clips() {
        let mut e = fx(
            Fx::Limiter {
                ceiling_db: 0.0,
                knee_db: 0.0,
            },
            1,
            48_000.0,
        );
        let mut r = rows_of(1, 4, |_, i| [0.5, 1.0, 2.0, -4.0][i]);
        let red = e.run(&mut r, 1, 4);
        assert_eq!(r[..4], [0.5, 1.0, 1.0, -1.0]);
        assert!((red - 0.75).abs() < 1e-6, "{red}");
        let mut quiet = rows_of(1, 4, |_, _| 0.1);
        assert_eq!(e.run(&mut quiet, 1, 4), 0.0);
    }

    #[test]
    fn a_moving_ceiling_glides_and_never_steps() {
        let mut e = fx(
            Fx::Limiter {
                ceiling_db: 0.0,
                knee_db: 0.0,
            },
            1,
            48_000.0,
        );
        let Effect::Limiter(l) = &mut e else {
            unreachable!()
        };
        l.ceil.target = -24.0; // 24 dB takes 320 frames
        let mut prev = 1.0f32;
        for _ in 0..10 {
            let mut r = rows_of(1, 64, |_, _| 2.0);
            e.run(&mut r, 1, 64);
            for y in &r[..64] {
                assert!((prev - y).abs() < 0.02, "{prev} -> {y}");
                prev = *y;
            }
        }
        assert!((prev - 10f32.powf(-24.0 / 20.0)).abs() < 1e-6, "{prev}");
    }

    /// Magnitude of a sine through the EQ after the filter has settled.
    fn gain_at(e: &mut Effect, hz: f32, rate: f32) -> f32 {
        let mut peak = 0.0f32;
        let mut t = 0usize;
        for blk in 0..40 {
            let mut r = rows_of(1, 256, |_, i| {
                (2.0 * std::f32::consts::PI * hz * (t + i) as f32 / rate).sin()
            });
            e.run(&mut r, 1, 256);
            t += 256;
            if blk >= 30 {
                peak = r[..256].iter().fold(peak, |m, y| m.max(y.abs()));
            }
        }
        peak
    }

    #[test]
    fn the_shelves_boost_and_cut_where_the_cookbook_says() {
        let rate = 48_000.0;
        let mut e = fx(
            Fx::Eq {
                low_hz: 120.0,
                low_db: 12.0,
                high_hz: 8000.0,
                high_db: -12.0,
            },
            1,
            rate,
        );
        let mut g = |hz| 20.0 * gain_at(&mut e, hz, rate).log10();
        assert!(
            (g(30.0) - 12.0).abs() < 1.0,
            "low shelf boosts below: {}",
            g(30.0)
        );
        assert!(g(1000.0).abs() < 1.0, "flat in the middle: {}", g(1000.0));
        assert!(
            (g(18000.0) + 12.0).abs() < 1.0,
            "high shelf cuts above: {}",
            g(18000.0)
        );
        // At the corner a shelf is half its gain (+6 dB).
        assert!((g(120.0) - 6.0).abs() < 1.0, "{}", g(120.0));
    }

    #[test]
    fn a_neutral_eq_is_bit_identical_and_skipped() {
        let mut e = fx(
            Fx::Eq {
                low_hz: 120.0,
                low_db: 0.0,
                high_hz: 8000.0,
                high_db: 0.0,
            },
            2,
            48_000.0,
        );
        let mut r = rows_of(2, 200, |c, i| ((i * 31 + c * 7) % 97) as f32 / 48.0 - 1.0);
        let want = r.clone();
        for _ in 0..3 {
            e.run(&mut r, 2, 200);
        }
        assert_eq!(r, want);
    }

    #[test]
    fn eq_state_survives_any_block_split_bit_for_bit() {
        let rate = 48_000.0;
        let cfg = Fx::Eq {
            low_hz: 200.0,
            low_db: 9.0,
            high_hz: 6000.0,
            high_db: -7.0,
        };
        let sig = |i: usize| ((i * 2654435761usize) % 2001) as f32 / 1000.0 - 1.0;
        let mut outs: Vec<Vec<f32>> = Vec::new();
        for split in [1usize, 7, 64, 256, 1000] {
            let mut e = fx(cfg.clone(), 1, rate);
            let mut out = Vec::new();
            for start in (0..2000).step_by(split) {
                let m = split.min(2000 - start);
                let mut r = rows_of(1, m, |_, i| sig(start + i));
                e.run(&mut r, 1, m);
                out.extend_from_slice(&r[..m]);
            }
            outs.push(out);
        }
        for o in &outs[1..] {
            assert!(
                outs[0]
                    .iter()
                    .zip(o)
                    .all(|(a, b)| a.to_bits() == b.to_bits())
            );
        }
    }

    #[test]
    fn an_eq_parameter_move_glides_the_gain_by_the_ramp_rate() {
        let rate = 48_000.0;
        let mut e = fx(
            Fx::Eq {
                low_hz: 120.0,
                low_db: 0.0,
                high_hz: 8000.0,
                high_db: 0.0,
            },
            1,
            rate,
        );
        let Effect::Eq(q) = &mut e else {
            unreachable!()
        };
        q.high.db.target = 18.0; // 18 dB at 36 dB per 480 frames: 240 frames
        let mut dc_out = Vec::new();
        for _ in 0..6 {
            let mut r = rows_of(1, 64, |_, i| if i % 2 == 0 { 1.0 } else { -1.0 }); // Nyquist: full shelf gain
            e.run(&mut r, 1, 64);
            dc_out.push(r[63].abs());
        }
        assert!(
            dc_out[0] > 1.0 && dc_out[0] < dc_out[2] && dc_out[2] < dc_out[5],
            "{dc_out:?}"
        );
        let Effect::Eq(q) = &e else { unreachable!() };
        assert_eq!(q.high.db.cur, 18.0);
    }

    #[test]
    fn a_nan_burst_cannot_latch_in_the_filter_memory_and_tails_flush() {
        let mut e = fx(
            Fx::Eq {
                low_hz: 120.0,
                low_db: 12.0,
                high_hz: 8000.0,
                high_db: 12.0,
            },
            1,
            48_000.0,
        );
        let mut r = rows_of(1, 64, |_, _| f32::NAN);
        e.run(&mut r, 1, 64);
        let mut clean = rows_of(1, 64, |_, i| (i as f32 * 0.1).sin());
        e.run(&mut clean, 1, 64);
        assert!(
            clean[..64].iter().all(|y| y.is_finite()),
            "recovers in one block"
        );
        // silence: the tail decays to an exact zero state
        for _ in 0..4000 {
            let mut z = rows_of(1, 64, |_, _| 0.0);
            e.run(&mut z, 1, 64);
        }
        let Effect::Eq(q) = &e else { unreachable!() };
        assert!(
            q.low.z[0]
                .iter()
                .chain(&q.high.z[0])
                .all(|z| *z == 0.0 || z.abs() >= FLUSH)
        );
    }

    #[test]
    fn a_delay_of_d_samples_moves_an_impulse_exactly_d_frames_for_every_block_split() {
        let rate = 48_000.0;
        for d in [0usize, 1, 63, 64, 65, 4800] {
            for split in [1usize, 64, 256, 4096] {
                let ms = d as f32 * 1000.0 / rate;
                let mut e = fx(Fx::Delay { ms }, 1, rate);
                let Effect::Delay(dl) = &e else {
                    unreachable!()
                };
                assert_eq!(dl.frames(), d);
                let total = 6000;
                let mut out = Vec::new();
                for start in (0..total).step_by(split) {
                    let m = split.min(total - start);
                    let mut r = rows_of(1, m, |_, i| if start + i == 10 { 1.0 } else { 0.0 });
                    e.run(&mut r, 1, m);
                    out.extend_from_slice(&r[..m]);
                }
                let at: Vec<usize> = (0..total).filter(|&i| out[i] != 0.0).collect();
                assert_eq!(at, [10 + d], "d={d} split={split}");
                assert_eq!(out[10 + d], 1.0);
            }
        }
    }

    #[test]
    fn a_delay_retime_crossfades_without_a_click_and_settles_exactly() {
        let rate = 48_000.0;
        let mut e = fx(Fx::Delay { ms: 10.0 }, 1, rate);
        let sig = |i: usize| (i as f32 * 0.02).sin() * 0.5;
        let mut t = 0;
        let mut last = 0.0f32;
        let mut max_step = 0.0f32;
        for blk in 0..40 {
            if blk == 10 {
                let Effect::Delay(d) = &mut e else {
                    unreachable!()
                };
                d.target = 1200; // 25 ms
            }
            let mut r = rows_of(1, 64, |_, i| sig(t + i));
            e.run(&mut r, 1, 64);
            for y in &r[..64] {
                if blk > 12 {
                    max_step = max_step.max((y - last).abs());
                }
                last = *y;
            }
            t += 64;
        }
        // the signal itself moves 0.01 per sample; a fade of 480 frames between positions 720 frames
        // apart adds at most a few times that, not a jump
        assert!(max_step < 0.05, "{max_step}");
        let Effect::Delay(d) = &e else { unreachable!() };
        assert_eq!((d.d, d.fade), (1200, 0));
        // settled: output is the input 1200 frames ago
        let mut r = rows_of(1, 64, |_, i| sig(t + i));
        e.run(&mut r, 1, 64);
        for (i, y) in r.iter().enumerate().take(64) {
            assert_eq!(*y, sig(t + i - 1200));
        }
    }

    #[test]
    fn the_delay_ring_is_inherited_by_swap_or_copy_and_a_resized_one_starts_cold() {
        let rate = 48_000.0;
        let filled = || {
            let mut a = fx(Fx::Delay { ms: 5.0 }, 1, rate);
            let mut r = rows_of(1, 64, |_, i| i as f32 + 1.0);
            a.run(&mut r, 1, 64);
            a
        };
        let sum = |e: &Effect| match e {
            Effect::Delay(d) => (d.write, d.buf[..64].iter().sum::<f32>()),
            _ => unreachable!(),
        };
        for keep in [false, true] {
            let mut a = filled();
            let mut b = fx(Fx::Delay { ms: 5.0 }, 1, rate);
            b.inherit(&mut a, keep);
            assert_eq!(sum(&b), (64, 64.0 * 65.0 / 2.0), "keep={keep}");
            // A swap hands the old plan the new one's empty ring; a copy leaves it running on its own.
            assert_eq!(sum(&a).1 != 0.0, keep, "keep={keep}");
        }
        let mut a = filled();
        let mut other = fx(Fx::Delay { ms: 5.0 }, 1, 96_000.0);
        other.inherit(&mut a, false);
        assert_eq!(sum(&other), (0, 0.0), "another rate is another ring");
    }

    #[test]
    fn the_delay_budget_drops_the_ninth_channel_and_the_ring_has_the_documented_size() {
        let mut budget = 8;
        assert!(Effect::new(&Fx::Delay { ms: 1.0 }, 8, 48_000.0, &mut budget).is_some());
        assert!(Effect::new(&Fx::Delay { ms: 1.0 }, 1, 48_000.0, &mut budget).is_none());
        let d = Delay::new(1, 48_000.0, 1.0);
        assert_eq!(d.cap, 9600 + MAX_FRAMES);
        assert_eq!(d.buf.len() * 4, 54_784);
    }

    #[test]
    fn a_reset_clears_every_memory() {
        let mut e = fx(Fx::Delay { ms: 1.0 }, 1, 48_000.0);
        let mut r = rows_of(1, 64, |_, _| 1.0);
        e.run(&mut r, 1, 64);
        e.reset();
        let Effect::Delay(d) = &e else { unreachable!() };
        assert!(d.buf.iter().all(|x| *x == 0.0));
    }

    /// ns per call of each effect over a stereo node. `cargo test --release -p sr-core --lib -- --ignored
    /// --nocapture bench_effects`
    #[test]
    #[ignore = "benchmark"]
    fn bench_effects() {
        use std::hint::black_box;
        use std::time::Instant;
        fn time(e: &mut Effect, rows: &[f32], n: usize, reset_each: bool) -> f64 {
            let mut buf = rows.to_vec();
            let iters = 50_000;
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..iters {
                        if reset_each {
                            buf.copy_from_slice(rows);
                        }
                        black_box(e.run(black_box(&mut buf), 2, n));
                    }
                    t.elapsed().as_nanos() as f64 / f64::from(iters)
                })
                .fold(f64::MAX, f64::min)
        }
        let rate = 48_000.0;
        for n in [64usize, 128, 256] {
            let audio = rows_of(2, n, |c, i| {
                (((i * 7 + c * 13) % 101) as f32 / 101.0 - 0.5) * 1.6
            });
            // 1e-40 is subnormal: every multiply in the filter works on subnormals
            let tiny = rows_of(2, n, |c, i| {
                (((i * 7 + c * 13) % 101) as f32 / 101.0 - 0.5) * 1e-40
            });
            let mut mono = fx(Fx::Mono, 2, rate);
            let mut lim_below = fx(
                Fx::Limiter {
                    ceiling_db: 0.0,
                    knee_db: 6.0,
                },
                2,
                rate,
            );
            let mut lim_above = fx(
                Fx::Limiter {
                    ceiling_db: -12.0,
                    knee_db: 6.0,
                },
                2,
                rate,
            );
            let mut eq = fx(
                Fx::Eq {
                    low_hz: 120.0,
                    low_db: 6.0,
                    high_hz: 8000.0,
                    high_db: -6.0,
                },
                2,
                rate,
            );
            let mut eq_tiny = fx(
                Fx::Eq {
                    low_hz: 120.0,
                    low_db: 6.0,
                    high_hz: 8000.0,
                    high_db: -6.0,
                },
                2,
                rate,
            );
            let mut eq_audio = fx(
                Fx::Eq {
                    low_hz: 120.0,
                    low_db: 6.0,
                    high_hz: 8000.0,
                    high_db: -6.0,
                },
                2,
                rate,
            );
            let mut delay = fx(Fx::Delay { ms: 5.0 }, 2, rate);
            let mut retime = fx(Fx::Delay { ms: 5.0 }, 2, rate);
            // a delay that never stops fading: re-target it every call
            let mut flip = false;
            let retime_ns = {
                let mut buf = audio.clone();
                let iters = 50_000;
                (0..5)
                    .map(|_| {
                        let t = Instant::now();
                        for _ in 0..iters {
                            flip = !flip;
                            if let Effect::Delay(d) = &mut retime {
                                d.target = if flip { 240 } else { 480 };
                                d.fade = 0;
                            }
                            retime.run(black_box(&mut buf), 2, n);
                        }
                        t.elapsed().as_nanos() as f64 / f64::from(iters)
                    })
                    .fold(f64::MAX, f64::min)
            };
            println!(
                "n {n:>3}: mono {:>6.0}  limiter below knee {:>6.0}  above {:>6.0}  eq {:>6.0}  eq refreshed {:>6.0}  eq on subnormals {:>6.0}  delay {:>6.0}  delay retiming {:>6.0} ns",
                time(&mut mono, &audio, n, false),
                time(&mut lim_below, &audio, n, false),
                time(&mut lim_above, &audio, n, true),
                time(&mut eq, &audio, n, false),
                time(&mut eq_audio, &audio, n, true),
                time(&mut eq_tiny, &tiny, n, true),
                time(&mut delay, &audio, n, false),
                retime_ns,
            );
        }
    }
}
