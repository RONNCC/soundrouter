//! Hand-over of tapped audio from the tap aggregate's IOProc to the main aggregate's IOProc.
//!
//! Both aggregates run on the companion's clock, so there is no drift to absorb: a fixed cushion of
//! frames sits in an SPSC ring (frame-major, every tap's channels back to back) and counters tell when
//! the scheduling jitter exceeded it. Everything here is allocation-free on the audio threads; the
//! receiver reaches the main IOProc through a mailbox like the mixer's plan.
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicPtr, AtomicU32, AtomicU64,
    Ordering::{AcqRel, Relaxed},
};

use rtrb::{Consumer, Producer, RingBuffer};
use sr_core::config::MAX_FRAMES;

/// Ring capacity in cushions: 8 periods of the requested buffer is far more than jitter needs.
const RING_PERIODS: usize = 8;
/// Retired receivers wait here for the control thread to drop them.
const RETURN_QUEUE: usize = 8;

/// Counters of one vdev's tap path, cumulative across tap aggregate rebuilds.
#[derive(Default)]
pub struct TapStats {
    /// Blocks the receiver had to replace with silence because the ring ran dry.
    pub underruns: AtomicU64,
    /// Blocks the tap IOProc dropped because the ring was full (the receiver stopped reading).
    pub dropped: AtomicU64,
    /// Times the receiver discarded the oldest frames because too many had piled up.
    pub trims: AtomicU64,
    /// Tap IOProc cycles skipped because the buffer shape was not the expected one.
    pub mismatches: AtomicU64,
    /// Frames left in the ring after the receiver's last read (the cushion it actually runs at).
    pub level: AtomicU32,
}

/// A snapshot of [`TapStats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TapCounters {
    pub underruns: u64,
    pub dropped: u64,
    pub trims: u64,
    pub mismatches: u64,
}

impl TapStats {
    pub fn counters(&self) -> TapCounters {
        TapCounters {
            underruns: self.underruns.load(Relaxed),
            dropped: self.dropped.load(Relaxed),
            trims: self.trims.load(Relaxed),
            mismatches: self.mismatches.load(Relaxed),
        }
    }
}

/// Producer end, owned by the tap aggregate's IOProc.
pub struct TapTx {
    prod: Producer<f32>,
    ch: usize,
    /// Frame-major staging for one chunk, so the ring takes it in one push.
    stage: Vec<f32>,
    stats: Arc<TapStats>,
}

/// Consumer end, owned by the main aggregate's IOProc once published.
pub struct TapRx {
    cons: Option<Consumer<f32>>,
    ch: usize,
    /// Frames to keep in the ring after each read.
    cushion: usize,
    /// False until `cushion` frames are buffered, and again after an underrun.
    primed: bool,
    tmp: Vec<f32>,
    stats: Arc<TapStats>,
}

/// A connected pair for `ch` interleaved channels per frame. `cushion` is the frames of margin the
/// receiver keeps; the ring holds [`RING_PERIODS`] times that (at least `MAX_FRAMES` frames, so a
/// chunked cycle always fits).
pub fn channel(ch: usize, cushion: usize, stats: Arc<TapStats>) -> (TapTx, TapRx) {
    let cap = (RING_PERIODS * cushion).max(2 * MAX_FRAMES) * ch;
    let (prod, cons) = RingBuffer::new(cap);
    (
        TapTx {
            prod,
            ch,
            stage: vec![0.0; MAX_FRAMES * ch],
            stats: stats.clone(),
        },
        TapRx {
            cons: Some(cons),
            ch,
            cushion,
            primed: false,
            tmp: vec![0.0; MAX_FRAMES * ch],
            stats,
        },
    )
}

impl TapTx {
    pub fn channels(&self) -> usize {
        self.ch
    }

    /// Frame-major staging for up to `MAX_FRAMES` frames; fill the first `frames * channels()` floats,
    /// then [`TapTx::push`].
    pub fn stage(&mut self) -> &mut [f32] {
        &mut self.stage
    }

    /// Appends the first `frames` staged frames. A full ring drops the whole block and counts it.
    pub fn push(&mut self, frames: usize) {
        let n = frames.min(MAX_FRAMES) * self.ch;
        if self.prod.push_entire_slice(&self.stage[..n]).is_err() {
            self.stats.dropped.fetch_add(1, Relaxed);
        }
    }
}

impl TapRx {
    /// A receiver with no channels: the vdev has no taps.
    pub fn none(stats: Arc<TapStats>) -> TapRx {
        TapRx {
            cons: None,
            ch: 0,
            cushion: 0,
            primed: false,
            tmp: Vec::new(),
            stats,
        }
    }

    pub fn channels(&self) -> usize {
        self.ch
    }

    /// Moves the next `n` frames into `tmp`; false (and a counter) when there is nothing to play.
    fn take(&mut self, n: usize) -> bool {
        let Some(cons) = self.cons.as_mut() else {
            return false;
        };
        let ch = self.ch;
        let mut avail = cons.slots() / ch;
        if !self.primed {
            if avail < n + self.cushion {
                return false; // still prefilling: not an underrun
            }
            self.primed = true;
        }
        if avail < n {
            self.primed = false;
            self.stats.underruns.fetch_add(1, Relaxed);
            return false;
        }
        // More than two blocks beyond the cushion means a burst or a stalled reader: drop the oldest
        // frames rather than carry the extra delay for good.
        if avail > self.cushion + 3 * n {
            let drop_frames = avail - self.cushion - n;
            if let Ok(c) = cons.read_chunk(drop_frames * ch) {
                c.commit_all();
            }
            avail -= drop_frames;
            self.stats.trims.fetch_add(1, Relaxed);
        }
        let ok = cons.pop_entire_slice(&mut self.tmp[..n * ch]).is_ok();
        self.stats.level.store((avail - n) as u32, Relaxed);
        ok
    }

    /// Writes the next `n` frames (`n <= MAX_FRAMES`) into the planar rows `c0..c0 + channels()` of
    /// `dst` (row stride `MAX_FRAMES`); silence when [`TapRx::take`] has nothing.
    pub fn pull(&mut self, n: usize, dst: &mut [f32], c0: usize) {
        let ch = self.ch;
        if !self.take(n) {
            for c in 0..ch {
                dst[(c0 + c) * MAX_FRAMES..][..n].fill(0.0);
            }
            return;
        }
        for c in 0..ch {
            let row = &mut dst[(c0 + c) * MAX_FRAMES..][..n];
            for (f, d) in row.iter_mut().enumerate() {
                *d = self.tmp[f * ch + c];
            }
        }
    }
}

/// One-slot mailbox for the newest receiver, like the mixer's plan mailbox.
struct RxSlot(AtomicPtr<TapRx>);

impl RxSlot {
    fn swap(&self, new: *mut TapRx) -> Option<Box<TapRx>> {
        let old = self.0.swap(new, AcqRel);
        // SAFETY: the slot only holds null or a pointer from `Box::into_raw` in `put`, and `swap`
        // hands each stored pointer out exactly once.
        (!old.is_null()).then(|| unsafe { Box::from_raw(old) })
    }
}

impl Drop for RxSlot {
    fn drop(&mut self) {
        drop(self.swap(null_mut()));
    }
}

/// Audio-thread side of the mailbox: the receiver in use plus the retire queue.
pub struct TapIn {
    slot: Arc<RxSlot>,
    ret: Producer<Box<TapRx>>,
    active: Box<TapRx>,
}

/// Control side.
pub struct TapInHandle {
    slot: Arc<RxSlot>,
    ret: Consumer<Box<TapRx>>,
}

/// A mailbox that starts with no taps.
pub fn tap_in(stats: Arc<TapStats>) -> (TapIn, TapInHandle) {
    let slot = Arc::new(RxSlot(AtomicPtr::new(null_mut())));
    let (ret, back) = RingBuffer::new(RETURN_QUEUE);
    (
        TapIn {
            slot: slot.clone(),
            ret,
            active: Box::new(TapRx::none(stats)),
        },
        TapInHandle { slot, ret: back },
    )
}

impl TapIn {
    /// Channels of the receiver in use, after picking up a newly published one.
    pub fn poll(&mut self) -> usize {
        let new = self.slot.swap(null_mut());
        if let Some(new) = new {
            let old = std::mem::replace(&mut self.active, new);
            let _ = self.ret.push(old); // full: drops here, cannot happen by sizing
        }
        self.active.ch
    }

    /// Fills `n` frames of `want` planar rows from `c0`: the receiver's, or silence when its channel
    /// count is not the one the mix plan expects (a swap in flight).
    pub fn pull(&mut self, want: usize, n: usize, dst: &mut [f32], c0: usize) {
        if self.active.ch == want {
            self.active.pull(n, dst, c0);
        } else {
            for c in 0..want {
                dst[(c0 + c) * MAX_FRAMES..][..n].fill(0.0);
            }
        }
    }
}

impl TapInHandle {
    /// Publishes `rx`; the audio thread takes it at its next block, replacing one it has not picked up.
    pub fn publish(&mut self, rx: TapRx) {
        self.flush();
        drop(self.slot.swap(Box::into_raw(Box::new(rx))));
    }

    /// Drops receivers the audio thread retired.
    pub fn flush(&mut self) {
        while self.ret.pop().is_ok() {}
    }
}

#[cfg(test)]
mod tests;
