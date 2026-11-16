//! I/O entry points. `do_io`/`zero_ts` run on the HAL's real-time threads: atomics only, no allocation,
//! no locks, no logging.
use crate::state::*;
use coreaudio_sys::*;
use std::ffi::c_void;
use std::sync::atomic::Ordering::*;

const BAD_OBJECT: OSStatus = kAudioHardwareBadObjectError as OSStatus;

pub unsafe extern "C" fn start_io(
    _: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _: u32,
) -> OSStatus {
    let Some((slot, _, side, Part::Device)) = lookup(dev) else {
        return BAD_OBJECT;
    };
    let _g = io_lock();
    // The first client on either side starts the shared timeline; later starters join it.
    if slot.clients.iter().all(|c| c.load(Acquire) == 0) {
        slot.anchor.store(now_ticks(), Release);
        slot.seed.fetch_add(1, AcqRel);
    }
    slot.clients[side as usize].fetch_add(1, AcqRel);
    0
}

pub unsafe extern "C" fn stop_io(
    _: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _: u32,
) -> OSStatus {
    let Some((slot, _, side, Part::Device)) = lookup(dev) else {
        return BAD_OBJECT;
    };
    let _g = io_lock();
    let left = slot.clients[side as usize]
        .fetch_update(AcqRel, Acquire, |n| n.checked_sub(1))
        .map_or(0, |n| n - 1);
    if left == 0 {
        // The ring this side writes goes silent: V writes ring A, C writes ring B.
        if let Some(r) = slot.ring(side == Side::Visible) {
            r.reset();
        }
    }
    0
}

pub unsafe extern "C" fn zero_ts(
    _: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _: u32,
    sample_time: *mut f64,
    host_time: *mut u64,
    seed: *mut u64,
) -> OSStatus {
    let Some((slot, _, side, Part::Device)) = lookup(dev) else {
        return BAD_OBJECT;
    };
    if sample_time.is_null() || host_time.is_null() || seed.is_null() {
        return BAD_OBJECT;
    }
    let per = ticks_per_second() / slot.rate(side) * RING_FRAMES as f64; // host ticks per period
    let anchor = slot.anchor.load(Acquire);
    let n = (now_ticks().saturating_sub(anchor) as f64 / per) as u64;
    unsafe {
        *sample_time = (n * RING_FRAMES as u64) as f64;
        *host_time = anchor + (n as f64 * per) as u64;
        *seed = slot.seed.load(Acquire).max(1);
    }
    0
}

pub unsafe extern "C" fn will_do(
    _: AudioServerPlugInDriverRef,
    _: AudioObjectID,
    _: u32,
    op: u32,
    will: *mut Boolean,
    in_place: *mut Boolean,
) -> OSStatus {
    unsafe {
        if !will.is_null() {
            *will = (op == kAudioServerPlugInIOOperationReadInput
                || op == kAudioServerPlugInIOOperationWriteMix) as Boolean;
        }
        if !in_place.is_null() {
            *in_place = 1;
        }
    }
    0
}

pub unsafe extern "C" fn begin_end(
    _: AudioServerPlugInDriverRef,
    _: AudioObjectID,
    _: u32,
    _: u32,
    _: u32,
    _: *const AudioServerPlugInIOCycleInfo,
) -> OSStatus {
    0
}

/// V.out -> ring A -> C.in, C.out -> ring B -> V.in.
pub unsafe extern "C" fn do_io(
    _: AudioServerPlugInDriverRef,
    dev: AudioObjectID,
    _: AudioObjectID,
    _: u32,
    op: u32,
    frames: u32,
    cycle: *const AudioServerPlugInIOCycleInfo,
    buf: *mut c_void,
    _: *mut c_void,
) -> OSStatus {
    let Some((slot, _, side, Part::Device)) = lookup(dev) else {
        return BAD_OBJECT;
    };
    if buf.is_null() || cycle.is_null() || frames == 0 {
        return 0;
    }
    // Own side only: the buffer matches the format the HAL last configured on this device.
    let ch = slot.channels(side) as usize;
    let write = op == kAudioServerPlugInIOOperationWriteMix;
    if !write && op != kAudioServerPlugInIOOperationReadInput {
        return 0;
    }
    // A writes: V output. A reads: C input. B writes: C output. B reads: V input.
    let ring_a = (side == Side::Visible) == write;
    let t = unsafe {
        if write {
            (*cycle).mOutputTime.mSampleTime
        } else {
            (*cycle).mInputTime.mSampleTime
        }
    } as i64;
    // SAFETY: the HAL hands us `frames` interleaved frames of the stream format we advertised.
    let buf = unsafe { std::slice::from_raw_parts_mut(buf as *mut f32, frames as usize * ch) };
    // While V and C disagree (a format change landed on one side only) the pair stays silent.
    let ring = slot
        .ring(ring_a)
        .filter(|_| frames <= RING_FRAMES / 2 && slot.in_sync());
    let Some(ring) = ring else {
        if !write {
            buf.fill(0.0);
        }
        return 0;
    };
    if write {
        ring.write(ch, t, buf)
    } else {
        ring.read(ch, t, buf)
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::perform_change;
    use sr_core::driver_proto::DriverDevice;

    const FRAMES: usize = 64;
    const GUARD: usize = 1024;
    const SENTINEL: f32 = -12345.5;

    /// Each test owns one slot of the global pool, so parallel tests cannot interfere.
    fn activate(slot: usize, channels: u16) -> (u32, u32) {
        SLOTS[slot].activate(DriverDevice {
            uid: format!("t{slot}"),
            name: "t".into(),
            channels,
        });
        (
            encode(slot, Side::Visible, Part::Device),
            encode(slot, Side::Engine, Part::Device),
        )
    }

    fn change(dev: u32, c: Change) {
        assert_eq!(
            unsafe {
                perform_change(
                    std::ptr::null_mut(),
                    dev,
                    change_action(&c),
                    std::ptr::null_mut(),
                )
            },
            0
        );
    }

    /// One WriteMix/ReadInput call on `buf` (`n` real samples). Returns the guard words that were
    /// overwritten, which must be none.
    fn run(dev: u32, op: u32, t: f64, buf: &mut Vec<f32>, n: usize) -> usize {
        buf.resize(n + GUARD, SENTINEL);
        buf[n..].fill(SENTINEL);
        let mut cycle: AudioServerPlugInIOCycleInfo = unsafe { std::mem::zeroed() };
        cycle.mInputTime.mSampleTime = t;
        cycle.mOutputTime.mSampleTime = t;
        let st = unsafe {
            do_io(
                std::ptr::null_mut(),
                dev,
                0,
                0,
                op,
                (n / ch_of(dev)) as u32,
                &cycle,
                buf.as_mut_ptr().cast(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(st, 0);
        buf[n..].iter().filter(|&&x| x != SENTINEL).count()
    }
    fn ch_of(dev: u32) -> usize {
        let (slot, _, side, _) = lookup(dev).unwrap();
        slot.channels(side) as usize
    }

    const WRITE: u32 = kAudioServerPlugInIOOperationWriteMix;
    const READ: u32 = kAudioServerPlugInIOOperationReadInput;

    fn ramp(ch: usize) -> Vec<f32> {
        (0..FRAMES * ch).map(|i| 1.0 + i as f32).collect()
    }

    #[test]
    fn channel_change_on_one_side_never_overruns_the_other_buffer() {
        let (v, c) = activate(15, 2);
        // V's format change lands first; C's I/O keeps running on its 2-channel buffer.
        change(v, Change::Channels(8));
        let mut out = ramp(8);
        assert_eq!(run(v, WRITE, 1000.0, &mut out, FRAMES * 8), 0);
        let mut buf = vec![];
        assert_eq!(
            run(c, READ, 1000.0, &mut buf, FRAMES * 2),
            0,
            "guard words overwritten"
        );
        assert!(
            buf[..FRAMES * 2].iter().all(|&x| x == 0.0),
            "mismatched pair must be silent"
        );
        // C -> V direction: C's 2-channel write must not reach V's 8-channel reader.
        let mut w = ramp(2);
        assert_eq!(run(c, WRITE, 5000.0, &mut w, FRAMES * 2), 0);
        assert_eq!(run(v, READ, 5000.0, &mut buf, FRAMES * 8), 0);
        assert!(buf[..FRAMES * 8].iter().all(|&x| x == 0.0));
        // Once C follows, the pair is bit-exact again in both directions.
        change(c, Change::Channels(8));
        assert_eq!(run(v, WRITE, 2000.0, &mut out, FRAMES * 8), 0);
        assert_eq!(run(c, READ, 2000.0, &mut buf, FRAMES * 8), 0);
        assert_eq!(&buf[..FRAMES * 8], &out[..FRAMES * 8]);
        let mut w = ramp(8);
        assert_eq!(run(c, WRITE, 3000.0, &mut w, FRAMES * 8), 0);
        assert_eq!(run(v, READ, 3000.0, &mut buf, FRAMES * 8), 0);
        assert_eq!(&buf[..FRAMES * 8], &w[..FRAMES * 8]);
    }

    #[test]
    fn rate_change_on_one_side_silences_the_pair() {
        let (v, c) = activate(14, 2);
        let mut out = ramp(2);
        assert_eq!(run(v, WRITE, 100.0, &mut out, FRAMES * 2), 0);
        let mut buf = vec![];
        run(c, READ, 100.0, &mut buf, FRAMES * 2);
        assert_eq!(&buf[..FRAMES * 2], &out[..FRAMES * 2]);
        change(v, Change::Rate(2));
        assert!(!SLOTS[14].in_sync());
        run(c, READ, 100.0, &mut buf, FRAMES * 2);
        assert!(buf[..FRAMES * 2].iter().all(|&x| x == 0.0));
        change(c, Change::Rate(2));
        assert!(SLOTS[14].in_sync());
    }

    #[test]
    fn zero_timestamp_uses_the_devices_own_rate() {
        let (v, c) = activate(13, 2);
        change(v, Change::Rate(2)); // V 96 kHz, C stays 48 kHz
        let per48 = ticks_per_second() / 48_000.0 * RING_FRAMES as f64;
        SLOTS[13]
            .anchor
            .store(now_ticks() - (10.5 * per48) as u64, Release);
        let ts = |dev| {
            let (mut t, mut h, mut s) = (0.0, 0, 0);
            assert_eq!(
                unsafe { zero_ts(std::ptr::null_mut(), dev, 0, &mut t, &mut h, &mut s) },
                0
            );
            t
        };
        assert_eq!(ts(c), 10.0 * RING_FRAMES as f64);
        assert_eq!(ts(v), 21.0 * RING_FRAMES as f64);
    }

    #[test]
    fn zero_timestamp_is_the_latest_period_boundary_not_after_now() {
        let (v, _) = activate(12, 2);
        let per = ticks_per_second() / 48_000.0 * RING_FRAMES as f64;
        let anchor = now_ticks() - (2.5 * per) as u64;
        SLOTS[12].anchor.store(anchor, Release);
        let (mut t, mut h, mut s) = (0.0, 0, 0);
        let before = now_ticks(); // slack below: the thread may be preempted between reads
        assert_eq!(
            unsafe { zero_ts(std::ptr::null_mut(), v, 0, &mut t, &mut h, &mut s) },
            0
        );
        let after = now_ticks();
        assert_eq!(t, 2.0 * RING_FRAMES as f64);
        assert!(h <= after, "timestamp is not in the future");
        assert!(
            before - h <= per as u64 + 1,
            "timestamp is the latest boundary"
        );
        assert!(s >= 1, "seed is never 0");
    }

    #[test]
    fn will_do_only_read_input_and_write_mix_in_place() {
        let (v, _) = activate(11, 2);
        for (op, expect) in [
            (kAudioServerPlugInIOOperationThread, 0),
            (kAudioServerPlugInIOOperationCycle, 0),
            (kAudioServerPlugInIOOperationConvertInput, 0),
            (kAudioServerPlugInIOOperationProcessInput, 0),
            (kAudioServerPlugInIOOperationReadInput, 1),
            (kAudioServerPlugInIOOperationProcessOutput, 0),
            (kAudioServerPlugInIOOperationMixOutput, 0),
            (kAudioServerPlugInIOOperationProcessMix, 0),
            (kAudioServerPlugInIOOperationConvertMix, 0),
            (kAudioServerPlugInIOOperationWriteMix, 1),
        ] {
            let (mut will, mut in_place) = (9, 9);
            let st = unsafe { will_do(std::ptr::null_mut(), v, 0, op, &mut will, &mut in_place) };
            assert_eq!((st, will, in_place), (0, expect, 1), "op {op}");
            // Ops the driver declined must not touch the buffer even if the host calls anyway.
            if expect == 0 {
                let mut buf = vec![];
                assert_eq!(run(v, op, 0.0, &mut buf, FRAMES * 2), 0);
                assert!(buf[..FRAMES * 2].iter().all(|&x| x == SENTINEL), "op {op}");
            }
        }
    }

    #[test]
    fn many_readers_share_a_wrapped_ring_without_consuming() {
        let (v, c) = activate(10, 2);
        let cap = 2.0 * RING_FRAMES as f64;
        let block = |k: usize| -> Vec<f32> {
            (0..FRAMES * 2)
                .map(|i| (k * 1000 + i) as f32 + 1.0)
                .collect()
        };
        // Four consecutive blocks around the physical wrap point at `cap`.
        let start = cap - 2.0 * FRAMES as f64;
        let mut out = vec![];
        for k in 0..4 {
            out.clear();
            out.extend(block(k));
            let t = start + (k * FRAMES) as f64;
            assert_eq!(run(v, WRITE, t, &mut out, FRAMES * 2), 0);
        }
        // 64 frames starting mid-block-1: second half of block 1, first half of block 2.
        let t = cap - FRAMES as f64 / 2.0;
        let want: Vec<f32> = block(1)[FRAMES..]
            .iter()
            .chain(&block(2)[..FRAMES])
            .copied()
            .collect();
        let mut buf = vec![];
        for reader in 0..4 {
            assert_eq!(run(c, READ, t, &mut buf, FRAMES * 2), 0, "reader {reader}");
            assert_eq!(
                &buf[..FRAMES * 2],
                &want[..],
                "reader {reader} saw different data"
            );
        }
        // An earlier block is still readable after the others have read (nothing was consumed).
        run(c, READ, start, &mut buf, FRAMES * 2);
        assert_eq!(&buf[..FRAMES * 2], &block(0)[..]);
        // V's own input is the other ring and stays silent.
        run(v, READ, t, &mut buf, FRAMES * 2);
        assert!(buf[..FRAMES * 2].iter().all(|&x| x == 0.0));
    }
}
