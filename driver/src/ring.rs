//! Loopback ring: interleaved f32 frames addressed by absolute sample time.
//!
//! One writer (a device's WriteMix), any number of readers. Reads never consume, so every reader
//! sees the same frames. Memory is allocated once and never freed. Everything on the I/O path is
//! wait-free: relaxed atomic stores of sample bits plus one release store of the write head.
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering::*};

const EMPTY: i64 = i64::MIN;

pub struct Ring {
    data: Box<[AtomicU32]>,
    cap: i64, // frames
    max_ch: usize,
    /// Sample time just past the last written frame, or EMPTY.
    end: AtomicI64,
    /// First frame of the contiguous run ending at `end` (a gap in writes restarts the run).
    start: AtomicI64,
    /// Frames in the last `write` call: the writer's IO buffer size, for read-ahead calibration.
    last_frames: AtomicU32,
}

impl Ring {
    /// `cap` frames of up to `max_ch` channels. Zeroed pages are only touched when used.
    pub fn new(cap: usize, max_ch: usize) -> Self {
        let words = vec![0u32; cap * max_ch].into_boxed_slice();
        // SAFETY: AtomicU32 has the same size, alignment and bit validity as u32.
        let data = unsafe { Box::from_raw(Box::into_raw(words) as *mut [AtomicU32]) };
        Self {
            data,
            cap: cap as i64,
            max_ch,
            end: AtomicI64::new(EMPTY),
            start: AtomicI64::new(0),
            last_frames: AtomicU32::new(0),
        }
    }

    /// Forget all content: reads return silence until the next write. Safe from any thread.
    pub fn reset(&self) {
        self.end.store(EMPTY, Release);
    }

    /// Frames the writer stored per call, or 0 before the first write.
    pub fn writer_frames(&self) -> usize {
        self.last_frames.load(Relaxed) as usize
    }

    /// Written frames past the end of a read of `frames` frames at `t`: how far that read could move
    /// later in time and still find data. `None` while the ring is empty.
    pub fn slack(&self, t: i64, frames: usize) -> Option<i64> {
        let end = self.end.load(Acquire);
        (end != EMPTY).then(|| end.saturating_sub(t.saturating_add(frames as i64)))
    }

    /// Store `src` (interleaved, `ch` channels) at sample time `t`. Single writer only.
    pub fn write(&self, ch: usize, t: i64, src: &[f32]) {
        self.write_mid(ch, t, src, || {});
    }

    /// `write`, running `mid` after the data is stored and before the new end is published: where a
    /// `reset` from another thread can land (tests drive it from there).
    fn write_mid(&self, ch: usize, t: i64, src: &[f32], mid: impl FnOnce()) {
        if ch == 0 || ch > self.max_ch {
            return;
        }
        let frames = (src.len() / ch).min(self.cap as usize);
        let src = &src[..frames * ch];
        self.last_frames.store(frames as u32, Relaxed);
        let mut end = self.end.load(Relaxed);
        let new_end = t.saturating_add(frames as i64);
        if end != EMPTY && new_end <= end.saturating_sub(self.cap / 2) {
            // Older than anything a reader may take, so not a late block: the timeline restarted behind an end
            // that was never reset. Writing it would also overwrite the frames one lap ahead of it.
            self.end.store(EMPTY, Release);
            end = EMPTY;
        }
        if end == EMPTY || t > end {
            self.start.store(t, Relaxed); // first write or a gap: older ring content is stale
        }
        self.span(ch, t, frames, |i, k| {
            self.data[i].store(src[k].to_bits(), Relaxed)
        });
        mid();
        if end == EMPTY || new_end > end {
            // Not a store: a `reset` since the load above (StopIO on another thread) must stay in force, or
            // this block would bring back the run it ended, and the next stream would read its frames.
            let _ = self.end.compare_exchange(end, new_end, Release, Relaxed);
        }
    }

    /// Fill `dst` from sample time `t`. Frames outside the valid window read as silence.
    pub fn read(&self, ch: usize, t: i64, dst: &mut [f32]) {
        dst.fill(0.0);
        if ch == 0 || ch > self.max_ch {
            return;
        }
        let end = self.end.load(Acquire);
        if end == EMPTY {
            return;
        }
        let start = self.start.load(Relaxed);
        // Half the ring is the valid window: the far half may be overwritten while we read.
        let lo = start.max(end - self.cap / 2).max(t);
        let hi = end.min(t.saturating_add((dst.len() / ch) as i64));
        if lo >= hi {
            return;
        }
        let off = ((lo - t) as usize) * ch;
        let dst = &mut dst[off..];
        self.span(ch, lo, (hi - lo) as usize, |i, k| {
            dst[k] = f32::from_bits(self.data[i].load(Relaxed))
        });
    }

    /// Call `f(ring word index, linear offset)` for every sample of `frames` frames at time `t`.
    fn span(&self, ch: usize, t: i64, frames: usize, mut f: impl FnMut(usize, usize)) {
        let mut k = 0;
        for n in 0..frames {
            let base = (t + n as i64).rem_euclid(self.cap) as usize * ch;
            for c in 0..ch {
                f(base + c, k);
                k += 1;
            }
        }
    }
}

/// Read cycles observed before the read-ahead is frozen (more when the buffer sizes beat slowly, see `plan`).
const CAL_CYCLES: u32 = 100;
/// Longest beat, in reader cycles, between two buffer sizes that calibration still covers.
const MAX_BEAT: usize = 1024;
/// Clean cycles after which earlier shortfalls no longer count as "repeated" (~90 s at 64 frames, 48 kHz):
/// long enough that a rare writer/reader order flip, which keeps coming, is caught by its second hit.
const CLEAN_CYCLES: u32 = 1 << 16;

/// Per-reader read-ahead for a loopback ring.
///
/// The writer stores `frames` at output time ~now+B and the reader reads at input time ~now-B, so a
/// frame sits in the ring about two buffers before anyone asks for it. Reading `ahead` frames later in
/// time (`ring.read(ch, t + ahead, ..)`) delivers it that much sooner. `ahead` is the smallest slack
/// (written frames past the end of the read, `Ring::slack`) seen over the first cycles of a stream,
/// minus a guard, and then stays fixed: a changing `ahead` replays or drops audio. Only repeated
/// shortfalls (a read that reaches past the written frames and gets silence there) halve it.
///
/// All state is atomics owned by the reader's real-time thread; other threads only bump the epoch
/// they pass in. No allocation, locks or logging.
pub struct ReadAhead {
    epoch: AtomicU32,
    frames: AtomicU32,
    writer_frames: AtomicU32,
    prev_t: AtomicI64,
    cycles: AtomicU32,
    frozen: AtomicBool,
    min_slack: AtomicI64,
    ahead: AtomicI64,
    misses: AtomicU32,
    clean: AtomicU32,
}

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl ReadAhead {
    pub const fn new() -> Self {
        Self {
            epoch: AtomicU32::new(0),
            frames: AtomicU32::new(0),
            writer_frames: AtomicU32::new(0),
            prev_t: AtomicI64::new(EMPTY),
            cycles: AtomicU32::new(0),
            frozen: AtomicBool::new(false),
            min_slack: AtomicI64::new(i64::MAX),
            ahead: AtomicI64::new(0),
            misses: AtomicU32::new(0),
            clean: AtomicU32::new(0),
        }
    }

    /// Frames the reader currently reads ahead (0 while calibrating).
    #[cfg(test)]
    pub fn ahead(&self) -> i64 {
        self.ahead.load(Relaxed)
    }

    fn restart(&self, epoch: u32, frames: usize, writer_frames: usize) {
        self.epoch.store(epoch, Relaxed);
        self.frames.store(frames as u32, Relaxed);
        self.writer_frames.store(writer_frames as u32, Relaxed);
        self.cycles.store(0, Relaxed);
        self.frozen.store(false, Relaxed);
        self.min_slack.store(i64::MAX, Relaxed);
        self.ahead.store(0, Relaxed);
        self.misses.store(0, Relaxed);
        self.clean.store(0, Relaxed);
    }

    /// Decide where this read of `frames` frames at `t` goes: returns `(ahead, shortfall)`. The caller
    /// reads `ring` at `t + ahead` and counts a shortfall. `epoch` changes when a stream stops. A new
    /// epoch, a different `frames` or writer buffer size, a jump of `t` (not one cycle later) or an empty
    /// ring restarts calibration.
    pub fn plan(&self, ring: &Ring, epoch: u32, t: i64, frames: usize) -> (i64, bool) {
        let fw = ring.writer_frames();
        let jump = {
            let d = t.saturating_sub(self.prev_t.swap(t, Relaxed));
            !(0..=4 * frames as i64).contains(&d)
        };
        let Some(slack) = ring.slack(t, frames) else {
            self.restart(epoch, frames, fw);
            return (0, false);
        };
        if jump
            || epoch != self.epoch.load(Relaxed)
            || frames != self.frames.load(Relaxed) as usize
            || fw != self.writer_frames.load(Relaxed) as usize
        {
            self.restart(epoch, frames, fw);
        }
        if !self.frozen.load(Relaxed) {
            let min = self.min_slack.load(Relaxed).min(slack);
            self.min_slack.store(min, Relaxed);
            let n = self.cycles.load(Relaxed) + 1;
            self.cycles.store(n, Relaxed);
            // Reader and writer positions repeat every fw / gcd(fw, frames) reader cycles; the minimum
            // over a shorter run could miss the tightest phase.
            let beat = fw / gcd(fw, frames).max(1);
            if n as usize >= beat.max(CAL_CYCLES as usize) || beat > MAX_BEAT {
                let guard = (frames as i64 / 8).max(16);
                let cap = 4 * frames.max(fw) as i64;
                let ahead = if beat > MAX_BEAT {
                    0
                } else {
                    (min - guard).clamp(0, cap)
                };
                self.ahead.store(ahead, Relaxed);
                self.frozen.store(true, Relaxed);
            }
            return (0, false);
        }
        let ahead = self.ahead.load(Relaxed);
        if ahead == 0 {
            return (0, false); // today's behaviour: nothing to protect
        }
        if slack >= ahead {
            if self.clean.fetch_add(1, Relaxed) >= CLEAN_CYCLES {
                self.misses.store(0, Relaxed);
            }
            return (ahead, false);
        }
        self.clean.store(0, Relaxed);
        let misses = self.misses.load(Relaxed) + 1;
        if misses < 2 {
            self.misses.store(misses, Relaxed);
            return (ahead, true);
        }
        self.misses.store(0, Relaxed);
        self.ahead.store(ahead / 2, Relaxed);
        (ahead / 2, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize, ch: usize, off: f32) -> Vec<f32> {
        (0..n * ch).map(|i| off + i as f32).collect()
    }

    #[test]
    fn loopback_is_bit_exact_and_readers_share() {
        let r = Ring::new(1024, 4);
        let w = ramp(64, 2, 1.0);
        r.write(2, 100, &w);
        for _ in 0..2 {
            let mut o = vec![9.0; 128];
            r.read(2, 100, &mut o);
            assert_eq!(o, w);
        }
    }

    #[test]
    fn reads_outside_window_are_silent() {
        let r = Ring::new(1024, 2);
        let mut o = vec![1.0; 16];
        r.read(2, 0, &mut o);
        assert!(o.iter().all(|&x| x == 0.0), "empty ring");
        r.write(2, 100, &ramp(8, 2, 1.0));
        r.read(2, 108, &mut o); // entirely past the write head
        assert!(o.iter().all(|&x| x == 0.0));
        r.read(2, 50, &mut o); // entirely before the first write
        assert!(o.iter().all(|&x| x == 0.0));
        r.read(2, 96, &mut o); // straddles: 4 silent frames, then 4 real
        assert!(o[..8].iter().all(|&x| x == 0.0));
        assert_eq!(&o[8..], &ramp(4, 2, 1.0)[..]);
    }

    #[test]
    fn wraps_around_capacity() {
        let r = Ring::new(64, 2);
        let w = ramp(16, 2, 1.0);
        r.write(2, 60, &w); // crosses the wrap point at 64
        let mut o = vec![0.0; 32];
        r.read(2, 60, &mut o);
        assert_eq!(o, w);
    }

    #[test]
    fn old_frames_beyond_half_ring_are_dropped() {
        let r = Ring::new(64, 1);
        for k in 0..8 {
            r.write(1, k * 16, &ramp(16, 1, k as f32 * 100.0));
        }
        let mut o = vec![5.0; 16];
        r.read(1, 0, &mut o); // 128 frames behind the head
        assert!(o.iter().all(|&x| x == 0.0));
        r.read(1, 112, &mut o);
        assert_eq!(o, ramp(16, 1, 700.0));
    }

    #[test]
    fn reset_silences_and_gap_restarts_run() {
        let r = Ring::new(1024, 1);
        r.write(1, 0, &[1.0; 16]);
        r.reset();
        let mut o = vec![7.0; 16];
        r.read(1, 0, &mut o);
        assert!(o.iter().all(|&x| x == 0.0));
        r.write(1, 500, &[2.0; 16]);
        r.write(1, 600, &[3.0; 16]); // gap 516..600 must not expose stale data
        r.read(1, 590, &mut o);
        assert!(o[..10].iter().all(|&x| x == 0.0));
        assert!(o[10..].iter().all(|&x| x == 3.0));
    }

    #[test]
    fn bad_channel_counts_are_ignored() {
        let r = Ring::new(64, 2);
        r.write(0, 0, &[1.0; 4]);
        r.write(3, 0, &[1.0; 6]);
        let mut o = vec![1.0; 6];
        r.read(3, 0, &mut o);
        assert!(o.iter().all(|&x| x == 0.0));
        // The rejected write left nothing behind for a valid reader to find.
        r.read(2, 0, &mut o);
        assert!(o.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_gap_hides_stale_frames_that_alias_the_gap_in_the_ring() {
        let r = Ring::new(64, 1);
        r.write(1, 0, &[1.0; 32]); // fills ring slots 0..32
        r.write(1, 100, &[2.0; 10]); // gap 32..100; slots 36.. alias the gap, slots 14..32 still hold 1.0
        let mut o = vec![9.0; 32];
        r.read(1, 78, &mut o); // inside the half-ring window, but before the new run began
        assert!(
            o[..22].iter().all(|&x| x == 0.0),
            "stale data leaked: {o:?}"
        );
        assert!(o[22..32].iter().all(|&x| x == 2.0));
    }

    #[test]
    fn sample_times_may_be_negative_and_cross_zero() {
        let r = Ring::new(64, 1);
        let w = ramp(20, 1, 1.0);
        r.write(1, -10, &w); // slots must not mirror: t = -1 and t = 1 are different frames
        let mut o = vec![0.0; 20];
        r.read(1, -10, &mut o);
        assert_eq!(o, w);
    }

    /// After several wraps, every frame a read returns is either silence or exactly what was written
    /// for that sample time (never newer data aliased onto the same ring slot), and the most recent
    /// half ring is always intact.
    #[test]
    fn stale_reads_after_wraps_are_silent_never_aliased() {
        const CAP: i64 = 64;
        let val = |t: i64, c: usize| (t + 1) as f32 * 10.0 + c as f32;
        let r = Ring::new(CAP as usize, 2);
        let end = 3 * CAP + 24; // three and a bit wraps, in blocks of 8
        for t in (0..end).step_by(8) {
            let w: Vec<f32> = (0..8)
                .flat_map(|i| [val(t + i, 0), val(t + i, 1)])
                .collect();
            r.write(2, t, &w);
        }
        for t in -10..end + 10 {
            for n in [1usize, 5, 16, 40] {
                let mut o = vec![-1.0; n * 2];
                r.read(2, t, &mut o);
                for i in 0..n as i64 {
                    let fr = &o[i as usize * 2..][..2];
                    let ts = t + i;
                    if (end - CAP / 2..end).contains(&ts) {
                        assert_eq!(fr, [val(ts, 0), val(ts, 1)], "t={t} frame {ts}");
                    } else {
                        assert_eq!(fr, [0.0, 0.0], "t={t} frame {ts} is outside the window");
                    }
                }
            }
        }
    }

    // ---- read-ahead: a model of the HAL driving one writer and one reader of a ring ----

    /// Cycle k of a device with buffer `b` has current time k*b; WriteMix targets now+b, ReadInput reads
    /// at now-b. Calls run in real-time order: nominal time + `phase` (reader only) + a wake error of
    /// up to `jitter` frames each. A cycle's data is the sample time + 1, so a frame is real iff it matches.
    #[derive(Clone)]
    struct Model {
        fw: usize,
        fr: usize,
        phase: i64,
        jitter: i64,
        seed: u64,
        cycles: usize,
        tie_writer_first: bool,
    }
    #[derive(Default)]
    struct Outcome {
        /// Reader cycles that reported a shortfall.
        shortfalls: Vec<usize>,
        /// `ahead` after each reader cycle.
        ahead: Vec<i64>,
        /// Frames that are not exactly what was written for their sample time, or non-silent past the end.
        bad_frames: usize,
        /// Reads that ran past the written frames (silent tail) without reporting a shortfall, while ahead > 0.
        unflagged: usize,
    }
    fn val(t: i64) -> f32 {
        t as f32 + 1.0
    }
    fn simulate(m: &Model) -> Outcome {
        let ring = Ring::new(1 << 15, 1);
        let ra = ReadAhead::new();
        let mut rng = m.seed | 1;
        let mut wake = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if m.jitter == 0 {
                0
            } else {
                (rng >> 33) as i64 % (2 * m.jitter + 1) - m.jitter
            }
        };
        let writes = m.cycles * m.fr / m.fw + 4;
        // (real time, 0 = writer / 1 = reader, cycle)
        let mut ev: Vec<(i64, u8, usize)> = vec![];
        // One thread per device: its calls never overtake each other.
        let (mut last_w, mut last_r) = (i64::MIN, i64::MIN);
        for k in 0..writes {
            last_w = ((k * m.fw) as i64 + wake()).max(last_w.saturating_add(1));
            ev.push((last_w, 0, k));
        }
        for k in 0..m.cycles {
            last_r = ((k * m.fr) as i64 + m.phase + wake()).max(last_r.saturating_add(1));
            ev.push((last_r, 1, k));
        }
        ev.sort_by_key(|&(t, who, _)| (t, (who == 1) == m.tie_writer_first));
        let first = m.fw as i64; // sample time of the first write
        let mut out = Outcome::default();
        for (_, who, k) in ev {
            if who == 0 {
                let t = (k * m.fw) as i64 + m.fw as i64;
                let data: Vec<f32> = (t..t + m.fw as i64).map(val).collect();
                ring.write(1, t, &data);
                continue;
            }
            let t = (k * m.fr) as i64 - m.fr as i64;
            let end = ring.slack(t, m.fr).map(|s| s + t + m.fr as i64);
            let (ahead, short) = ra.plan(&ring, 0, t, m.fr);
            let mut buf = vec![f32::NAN; m.fr];
            ring.read(1, t + ahead, &mut buf);
            let end = end.unwrap_or(i64::MIN);
            for (i, &x) in buf.iter().enumerate() {
                let ts = t + ahead + i as i64;
                let want = if ts >= first && ts < end {
                    val(ts)
                } else {
                    0.0
                };
                out.bad_frames += (x != want) as usize;
            }
            out.unflagged += (ahead > 0 && t + ahead + m.fr as i64 > end && !short) as usize;
            if short {
                out.shortfalls.push(k);
            }
            out.ahead.push(ahead);
        }
        out
    }
    fn model(fw: usize, fr: usize, phase: i64, jitter: i64) -> Model {
        Model {
            fw,
            fr,
            phase,
            jitter,
            seed: 1,
            cycles: 3000,
            tie_writer_first: true,
        }
    }
    fn guard(b: usize) -> i64 {
        (b as i64 / 8).max(16)
    }

    /// Equal buffers, a stable call order (jitter well below the distance between the two calls): no
    /// shortfall at all, and `ahead` is exactly what the order leaves free minus the guard: one buffer
    /// when the reader runs first, two when the writer's current cycle has already landed.
    #[test]
    fn stable_order_never_falls_short_and_reads_ahead_by_the_free_slack() {
        for b in [64usize, 128, 256, 512, 1024, 2048] {
            let j = guard(b) / 2;
            for (phase, free) in [(b as i64 / 2, 2 * b as i64), (-(b as i64) / 2, b as i64)] {
                let o = simulate(&model(b, b, phase, j));
                assert!(o.shortfalls.is_empty(), "b={b} phase={phase}");
                assert_eq!((o.bad_frames, o.unflagged), (0, 0), "b={b} phase={phase}");
                assert_eq!(
                    *o.ahead.last().unwrap(),
                    free - guard(b),
                    "b={b} phase={phase}"
                );
            }
        }
    }

    /// Different writer and reader buffers beat against each other; calibration spans the whole beat.
    #[test]
    fn unequal_buffers_calibrate_over_the_whole_beat() {
        for (fw, fr) in [
            (512, 64),
            (64, 512),
            (480, 512),
            (441, 512),
            (128, 96),
            (1024, 256),
            (1000, 64),
            (511, 512),
            (1023, 512),
        ] {
            for phase in [0, 7, 33] {
                let o = simulate(&model(fw, fr, phase, 0));
                assert!(o.shortfalls.is_empty(), "fw={fw} fr={fr} phase={phase}");
                assert_eq!((o.bad_frames, o.unflagged), (0, 0), "fw={fw} fr={fr}");
            }
        }
        // A beat longer than the calibration can cover: no read-ahead rather than a guess.
        let o = simulate(&model(1031, 64, 0, 0));
        assert!(o.ahead.iter().all(|&a| a == 0) && o.shortfalls.is_empty());
    }

    /// A writer far ahead of the reader (a stale or foreign timeline) never lets `ahead` run away.
    #[test]
    fn ahead_is_capped() {
        let b = 64;
        let ring = Ring::new(1 << 15, 1);
        let ra = ReadAhead::new();
        for k in 0..101 {
            ring.write(1, 10_000 + k * b as i64, &vec![1.0; b]);
            ra.plan(&ring, 0, k * b as i64, b);
        }
        assert_eq!(ra.ahead(), 4 * b as i64);
    }

    /// Shortfalls far apart are separate hiccups and leave `ahead` alone; two close together halve it.
    #[test]
    fn only_repeated_shortfalls_back_off() {
        let b = 64usize;
        let ring = Ring::new(1 << 15, 1);
        let ra = ReadAhead::new();
        let data = vec![1.0; b];
        let mut t = 0i64;
        // One HAL cycle: the writer's call lands first unless it stalls.
        let mut step = |stall: bool| {
            if !stall {
                ring.write(1, t + 2 * b as i64, &data);
            }
            let r = ra.plan(&ring, 0, t, b);
            t += b as i64;
            r
        };
        for _ in 0..101 {
            step(false);
        }
        let a = ra.ahead();
        assert_eq!(a, 2 * b as i64 - guard(b));
        assert_eq!(step(true), (a, true), "a stalled writer falls short");
        for _ in 0..CLEAN_CYCLES + 2 {
            assert_eq!(step(false), (a, false));
        }
        assert_eq!(step(true), (a, true));
        assert_eq!(ra.ahead(), a, "far from the first: not repeated");
        step(false);
        assert_eq!(step(true), (a / 2, true));
        assert_eq!(ra.ahead(), a / 2, "close to the last: repeated");
    }

    /// The two calls land in a random order (writer and reader wake together): `ahead` is sized for
    /// the reader-first order, so neither order falls short.
    #[test]
    fn calls_racing_each_other_do_not_fall_short() {
        for b in [64usize, 256, 1024] {
            for seed in 1..40 {
                let m = Model {
                    seed,
                    ..model(b, b, 0, b as i64 / 4)
                };
                let o = simulate(&m);
                assert!(o.shortfalls.is_empty(), "b={b} seed={seed}");
                assert_eq!(
                    *o.ahead.last().unwrap(),
                    b as i64 - guard(b),
                    "b={b} seed={seed}"
                );
            }
        }
    }

    /// A rare order flip that calibration missed falls short, but the second shortfall halves `ahead`
    /// below what the slow order leaves, so there are never more than two and the stream is clean after.
    #[test]
    fn a_rarely_flipping_order_backs_off_and_converges() {
        let mut falls_short = 0;
        for b in [64usize, 512] {
            let j = guard(b) / 2;
            for seed in 1..40 {
                // The reader runs a hair behind the writer: it only sees the writer-first slack unless a
                // wake error swaps them, which is rare.
                let m = Model {
                    seed,
                    cycles: 12_000,
                    ..model(b, b, 2 * j - 1, j)
                };
                let o = simulate(&m);
                assert_eq!((o.bad_frames, o.unflagged), (0, 0), "b={b} seed={seed}");
                assert!(
                    o.shortfalls.len() <= 2,
                    "b={b} seed={seed}: {:?}",
                    o.shortfalls
                );
                falls_short += (o.shortfalls.len() == 2) as usize;
                if let Some(&k) = o.shortfalls.first() {
                    assert_eq!(o.ahead[k], o.ahead[k - 1], "one shortfall is not repeated");
                }
                if let Some(&k) = o.shortfalls.get(1) {
                    assert!(o.ahead[k] < 2 * b as i64 - guard(b), "backed off");
                }
            }
        }
        assert!(falls_short > 0, "the model never exercised the back-off");
    }

    /// Wake errors larger than the guard and the call distance together, any buffers: reads stay
    /// inside what was written (never beyond `end`), every silent tail is reported, and the number of
    /// shortfalls stays bounded because only back-off ever changes `ahead`.
    #[test]
    fn heavy_jitter_stays_safe_and_bounded() {
        for fr in [64usize, 256, 1024] {
            for fw in [fr, fr * 2, fr / 2] {
                for seed in 1..30 {
                    let m = Model {
                        seed,
                        cycles: 6000,
                        ..model(fw, fr, seed as i64 % 7, fr as i64)
                    };
                    let o = simulate(&m);
                    assert_eq!(
                        (o.bad_frames, o.unflagged),
                        (0, 0),
                        "fw={fw} fr={fr} seed={seed}"
                    );
                    let frozen = o.ahead.iter().skip_while(|&&a| a == 0).collect::<Vec<_>>();
                    assert!(
                        frozen.windows(2).all(|w| w[1] <= w[0]),
                        "ahead only ever shrinks"
                    );
                    assert!(
                        o.shortfalls.len() <= 40,
                        "fw={fw} fr={fr} seed={seed}: {}",
                        o.shortfalls.len()
                    );
                }
            }
        }
    }

    proptest::proptest! {
        #[test]
        fn read_ahead_never_reads_past_the_written_frames(
            fw in 16usize..1200, fr in 16usize..1200, phase in -2000i64..2000,
            jitter in 0i64..1500, seed in 1u64..u64::MAX, first in proptest::bool::ANY,
        ) {
            let m = Model { fw, fr, phase, jitter, seed, cycles: 1500, tie_writer_first: first };
            let o = simulate(&m);
            proptest::prop_assert_eq!((o.bad_frames, o.unflagged), (0, 0));
            proptest::prop_assert!(o.shortfalls.len() <= 60, "{}", o.shortfalls.len());
        }
    }

    /// Calibration restarts, and `ahead` drops to 0, on anything that changes the relation between
    /// reader and writer: a new epoch, another reader buffer, a different writer buffer, a time jump,
    /// an empty ring.
    #[test]
    fn calibration_restarts_when_the_stream_changes() {
        let b = 256usize;
        let ring = Ring::new(1 << 15, 1);
        let ra = ReadAhead::new();
        let mut t = 0i64;
        let run = |ring: &Ring, epoch: u32, fr: usize, t: &mut i64, n: usize| {
            let mut a = 0;
            for _ in 0..n {
                ring.write(1, *t + 2 * b as i64, &vec![1.0; b]);
                a = ra.plan(ring, epoch, *t, fr).0;
                *t += b as i64;
            }
            a
        };
        ring.write(1, 0, &vec![1.0; 3 * b]);
        assert_eq!(
            run(&ring, 0, b, &mut t, 100),
            0,
            "calibrating for 100 cycles"
        );
        let a = run(&ring, 0, b, &mut t, 1);
        assert!(a > 0, "frozen after 100 cycles");
        assert_eq!(run(&ring, 0, b, &mut t, 50), a, "ahead does not drift");
        assert_eq!(run(&ring, 1, b, &mut t, 1), 0, "epoch");
        assert_eq!(run(&ring, 1, b, &mut t, 100), a, "same stream, same value");
        assert_eq!(run(&ring, 1, b / 2, &mut t, 1), 0, "reader buffer");
        assert_eq!(run(&ring, 1, b, &mut t, 1), 0, "reader buffer back");
        assert_eq!(run(&ring, 1, b, &mut t, 100), a);
        t += 100 * b as i64;
        assert_eq!(run(&ring, 1, b, &mut t, 1), 0, "time jump");
        assert_eq!(run(&ring, 1, b, &mut t, 100), a);
        ring.write(1, t + 2 * b as i64, &vec![1.0; b / 2]); // the writer's buffer size changed
        assert_eq!(ra.plan(&ring, 1, t, b).0, 0, "writer buffer");
        assert_eq!(ra.ahead(), 0);
        assert_eq!(run(&ring, 1, b, &mut t, 101), a);
        ring.reset();
        let next = t + b as i64;
        assert_eq!(ra.plan(&ring, 1, next, b), (0, false), "empty ring");
        assert_eq!(ra.ahead(), 0);
    }

    /// A reset that lands while a write is in flight must not be undone by that write: the next stream
    /// starts on a ring that holds nothing of the old one (here the old run's frames at the new stream's
    /// sample times).
    #[test]
    fn a_reset_during_a_write_is_not_undone_by_it() {
        let r = Ring::new(1024, 1);
        for k in 0..10 {
            r.write(1, k * 64, &[7.0; 64]); // the old stream: frames 0..640
        }
        r.write_mid(1, 640, &[7.0; 64], || r.reset()); // its last cycle races the StopIO that resets the ring
        let mut o = vec![9.0; 64];
        r.read(1, 200, &mut o); // the new stream starts over at time 0 and reads around here
        assert!(o.iter().all(|&x| x == 0.0), "stale frames came back: {o:?}");
        r.write(1, 0, &[1.0; 64]); // the new stream's first write
        r.read(1, 0, &mut o);
        assert!(
            o.iter().all(|&x| x == 1.0),
            "new stream is readable at once"
        );
        r.read(1, 200, &mut o);
        assert!(o.iter().all(|&x| x == 0.0));
    }

    /// A timeline that restarts behind a ring that kept its old end (a reset that never came) must not
    /// silence the new stream until it catches up, and a block from a lap ago must not overwrite the
    /// frames one lap ahead of it.
    #[test]
    fn a_block_older_than_the_window_restarts_the_run_and_never_overwrites_newer_frames() {
        let r = Ring::new(1024, 1);
        r.write(1, 1_000_000, &[5.0; 64]);
        for k in 0..4 {
            r.write(1, k * 64, &[(k + 1) as f32; 64]); // the timeline starts over
        }
        let mut o = vec![0.0; 256];
        r.read(1, 0, &mut o);
        for k in 0..4 {
            assert!(
                o[k * 64..][..64].iter().all(|&x| x == (k + 1) as f32),
                "block {k}"
            );
        }
        r.write(1, 5000, &[2.0; 64]);
        r.write(1, 5000 - 1024, &[3.0; 64]); // a lap behind: the same ring slots
        r.read(1, 5000, &mut o[..64]);
        assert!(
            o[..64].iter().all(|&x| x != 3.0),
            "a stale block overwrote newer frames"
        );
    }

    proptest::proptest! {
        /// The ring against a map of what was written per sample time, for writes at any offset from the
        /// end (late, overlapping, gapped, a lap or more behind) and reads anywhere: a read returns exactly
        /// the frames of the current run inside the readable window and silence elsewhere.
        #[test]
        fn ring_matches_a_model_for_any_write_offsets(
            ops in proptest::collection::vec((-200i64..40, 1usize..16, proptest::bool::ANY, 0i64..90), 1..80),
        ) {
            use std::collections::HashMap;
            const CAP: i64 = 64;
            let r = Ring::new(CAP as usize, 1);
            let mut m: HashMap<i64, f32> = HashMap::new();
            let (mut start, mut end) = (0i64, None::<i64>);
            let mut n = 0.0f32;
            for (delta, len, is_read, back) in ops {
                if is_read {
                    let t = end.map_or(0, |e| e - back);
                    let mut o = vec![-7.0; len];
                    r.read(1, t, &mut o);
                    for (i, &x) in o.iter().enumerate() {
                        let f = t + i as i64;
                        let want = match end {
                            Some(e) if f >= start.max(e - CAP / 2) && f < e => m[&f],
                            _ => 0.0,
                        };
                        proptest::prop_assert_eq!(x, want, "frame {} (run {}..{:?})", f, start, end);
                    }
                } else {
                    let t = end.map_or(100, |e| e + delta);
                    let w: Vec<f32> = (0..len).map(|_| { n += 1.0; n }).collect();
                    r.write(1, t, &w);
                    let new_end = t + len as i64;
                    if end.is_some_and(|e| new_end <= e - CAP / 2) {
                        end = None; // wholly older than the window: the timeline restarted
                    }
                    if end.is_none_or(|e| t > e) {
                        start = t;
                    }
                    for (i, &x) in w.iter().enumerate() {
                        m.insert(t + i as i64, x);
                    }
                    end = Some(end.map_or(new_end, |e| e.max(new_end)));
                }
            }
        }
    }
}
