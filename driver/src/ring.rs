//! Loopback ring: interleaved f32 frames addressed by absolute sample time.
//!
//! One writer (a device's WriteMix), any number of readers. Reads never consume, so every reader
//! sees the same frames. Memory is allocated once and never freed. Everything on the I/O path is
//! wait-free: relaxed atomic stores of sample bits plus one release store of the write head.
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering::*};

const EMPTY: i64 = i64::MIN;

pub struct Ring {
    data: Box<[AtomicU32]>,
    cap: i64, // frames
    max_ch: usize,
    /// Sample time just past the last written frame, or EMPTY.
    end: AtomicI64,
    /// First frame of the contiguous run ending at `end` (a gap in writes restarts the run).
    start: AtomicI64,
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
        }
    }

    /// Forget all content: reads return silence until the next write. Safe from any thread.
    pub fn reset(&self) {
        self.end.store(EMPTY, Release);
    }

    /// Store `src` (interleaved, `ch` channels) at sample time `t`. Single writer only.
    pub fn write(&self, ch: usize, t: i64, src: &[f32]) {
        if ch == 0 || ch > self.max_ch {
            return;
        }
        let frames = (src.len() / ch).min(self.cap as usize);
        let src = &src[..frames * ch];
        let end = self.end.load(Relaxed);
        if end == EMPTY || t > end {
            self.start.store(t, Relaxed); // first write or a gap: older ring content is stale
        }
        self.span(ch, t, frames, |i, k| {
            self.data[i].store(src[k].to_bits(), Relaxed)
        });
        let new_end = t.saturating_add(frames as i64);
        if end == EMPTY || new_end > end {
            self.end.store(new_end, Release);
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
    }
}
