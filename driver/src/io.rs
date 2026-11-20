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
        slot.restart_read_ahead();
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
    // Shared anchor and seed, but each device's own rate: identical to the peer while the rates agree.
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

/// V.out -> ring A -> C.in, C.out -> ring B -> V.in. With no I/O running on C (no engine attached),
/// V.in reads ring A instead: apps recording from V hear what is played into V (Soundflower-style).
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
    // A writes: V output. A reads: C input, or V input while C is idle. B writes: C output.
    // B reads: V input while C runs.
    let direct =
        side == Side::Visible && !write && slot.clients[Side::Engine as usize].load(Acquire) == 0;
    let ring_a = (side == Side::Visible) == write || direct;
    let t = unsafe {
        if write {
            (*cycle).mOutputTime.mSampleTime
        } else {
            (*cycle).mInputTime.mSampleTime
        }
    } as i64;
    // SAFETY: the HAL hands us `frames` interleaved frames of the stream format we advertised.
    let buf = unsafe { std::slice::from_raw_parts_mut(buf as *mut f32, frames as usize * ch) };
    if write {
        // Late (scheduled before the device's current time) or too big for the ring: counted. Real-time safe.
        // SAFETY: checked non-null above.
        let late = t < unsafe { (*cycle).mCurrentTime.mSampleTime } as i64;
        if late || frames > RING_FRAMES / 2 {
            slot.overloads[side as usize].fetch_add(1, Relaxed);
        }
    }
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
        ring.write(ch, t, buf);
        return 0;
    }
    // Read ahead of the timeline by what the writer leaves unused (calibrated per reader, see ring::ReadAhead).
    let mut at = t;
    if read_ahead_on() {
        let epoch = slot.ra_epoch.load(Relaxed);
        let (ahead, short) = slot.read_ahead[side as usize].plan(ring, epoch, t, frames as usize);
        if short {
            slot.overloads[side as usize].fetch_add(1, Relaxed);
        }
        at = t.saturating_add(ahead);
    }
    ring.read(ch, at, buf);
    // V's levels are applied on the read side, so both consumers of what apps play (the engine on C.in and
    // V.in's direct loop) and what apps record from V honour them. C itself has no controls.
    let out = || slot.levels[1].effective();
    let g = match (side, direct) {
        (Side::Engine, _) => out(),
        (Side::Visible, true) => out() * slot.levels[0].effective(),
        (Side::Visible, false) => slot.levels[0].effective(),
    };
    if g == 0.0 {
        buf.fill(0.0); // exact silence, also for non-finite input
    } else if g != 1.0 {
        buf.iter_mut().for_each(|x| *x *= g);
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
        run_at(dev, op, t, 0.0, buf, n)
    }
    /// `now` is the device's current sample time in the cycle info.
    fn run_at(dev: u32, op: u32, t: f64, now: f64, buf: &mut Vec<f32>, n: usize) -> usize {
        buf.resize(n + GUARD, SENTINEL);
        buf[n..].fill(SENTINEL);
        let mut cycle: AudioServerPlugInIOCycleInfo = unsafe { std::mem::zeroed() };
        cycle.mInputTime.mSampleTime = t;
        cycle.mOutputTime.mSampleTime = t;
        cycle.mCurrentTime.mSampleTime = now;
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
        SLOTS[15].clients[Side::Engine as usize].store(1, Release); // engine attached: B feeds V.in
        // The list asks for 8 ch; V's format change lands first; C's I/O keeps running on its 2-channel buffer.
        SLOTS[15].want(&Change::Channels(8));
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
        SLOTS[14].want(&Change::Rate(2)); // a client asked for 96 kHz; only V's Perform has run
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
        SLOTS[13].want(&Change::Rate(2));
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
        let before = now_ticks();
        assert_eq!(
            unsafe { zero_ts(std::ptr::null_mut(), v, 0, &mut t, &mut h, &mut s) },
            0
        );
        let after = now_ticks();
        assert_eq!(t, 2.0 * RING_FRAMES as f64);
        assert!(h <= after, "timestamp is not in the future");
        assert!(
            before - h <= per as u64 + 100_000, // slack: the thread may be preempted
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
        // V's own input: with no engine on C, V hears what it played (ring A)...
        run(v, READ, t, &mut buf, FRAMES * 2);
        assert_eq!(&buf[..FRAMES * 2], &want[..]);
        // ...and once C runs, V's input is ring B, which nobody wrote: silent.
        SLOTS[10].clients[Side::Engine as usize].store(1, Release);
        run(v, READ, t, &mut buf, FRAMES * 2);
        assert!(buf[..FRAMES * 2].iter().all(|&x| x == 0.0));
    }

    /// One app doing output and input on V with no engine attached gets its own output back on its
    /// input (srctl loop); when the engine starts C, V's input switches to C's output, and back.
    #[test]
    fn v_loops_to_itself_until_the_engine_runs_c() {
        let (v, c) = activate(9, 2);
        let start = |dev| unsafe { start_io(std::ptr::null_mut(), dev, 0) };
        let stop = |dev| unsafe { stop_io(std::ptr::null_mut(), dev, 0) };
        assert_eq!(start(v), 0);
        let mut out = ramp(2);
        assert_eq!(run(v, WRITE, 1024.0, &mut out, FRAMES * 2), 0);
        let mut buf = vec![];
        run(v, READ, 1024.0, &mut buf, FRAMES * 2);
        assert_eq!(
            &buf[..FRAMES * 2],
            &out[..FRAMES * 2],
            "V.out -> V.in while C is idle"
        );
        assert_eq!(start(c), 0);
        let mut mix: Vec<f32> = ramp(2).iter().map(|x| -x).collect();
        assert_eq!(run(c, WRITE, 1024.0, &mut mix, FRAMES * 2), 0);
        run(v, READ, 1024.0, &mut buf, FRAMES * 2);
        assert_eq!(
            &buf[..FRAMES * 2],
            &mix[..FRAMES * 2],
            "C.out -> V.in while C runs"
        );
        run(c, READ, 1024.0, &mut buf, FRAMES * 2);
        assert_eq!(
            &buf[..FRAMES * 2],
            &out[..FRAMES * 2],
            "V.out still reaches C.in"
        );
        assert_eq!(stop(c), 0);
        run(v, READ, 1024.0, &mut buf, FRAMES * 2);
        assert_eq!(
            &buf[..FRAMES * 2],
            &out[..FRAMES * 2],
            "back to the direct loop"
        );
        assert_eq!(stop(v), 0);
    }

    fn zero_time(dev: u32) -> (f64, u64, u64) {
        let (mut t, mut h, mut s) = (0.0, 0, 0);
        assert_eq!(
            unsafe { zero_ts(std::ptr::null_mut(), dev, 0, &mut t, &mut h, &mut s) },
            0
        );
        (t, h, s)
    }

    /// V and C share anchor and seed; at one rate they also report one timestamp (the HAL syncs the
    /// pair as one clock domain), while a rate change moves only the device that changed.
    #[test]
    fn v_and_c_report_identical_zero_timestamps_at_one_rate() {
        let (v, c) = activate(5, 2);
        let per = ticks_per_second() / 48_000.0 * RING_FRAMES as f64;
        SLOTS[5]
            .anchor
            .store(now_ticks() - (7.5 * per) as u64, Release);
        // Both calls land in the same period unless a boundary passes in between; retry across one.
        let same = (0..50).any(|_| {
            let a = zero_time(v);
            let b = zero_time(c);
            let a2 = zero_time(v);
            a == a2 && a == b
        });
        assert!(same, "V and C disagree at the same rate");
    }

    /// The engine path (V.out -> C.in) and the direct loop (V.out -> V.in) both apply V's output level;
    /// V.in via the engine applies V's input level only; C.out is stored untouched.
    #[test]
    fn levels_apply_on_the_read_side_bit_exactly() {
        let (v, c) = activate(8, 2);
        let n = FRAMES * 2;
        let out = ramp(2);
        let mut buf = vec![];
        let db = -6.0206f32;
        let g = 10f64.powf(db as f64 / 20.0) as f32;
        assert!((g - 0.5).abs() < 1e-4);
        let scaled = |x: &[f32], g: f32| x.iter().map(|&s| s * g).collect::<Vec<_>>();

        // Defaults: unity, bit-exact pass-through.
        assert_eq!(run(v, WRITE, 1000.0, &mut out.clone(), n), 0);
        run(c, READ, 1000.0, &mut buf, n);
        assert_eq!(&buf[..n], &out[..]);

        // Engine path honours the output level; the stored samples stay raw.
        SLOTS[8].levels[1].set_db(db);
        run(c, READ, 1000.0, &mut buf, n);
        assert_eq!(&buf[..n], &scaled(&out, g)[..]);
        SLOTS[8].levels[1].set_db(0.0);
        run(c, READ, 1000.0, &mut buf, n);
        assert_eq!(&buf[..n], &out[..], "stored data is not attenuated");

        // Direct loop (C idle): output level times input level.
        SLOTS[8].levels[1].set_db(db);
        SLOTS[8].levels[0].set_db(db);
        run(v, READ, 1000.0, &mut buf, n);
        assert_eq!(&buf[..n], &scaled(&out, g * g)[..]);
        SLOTS[8].levels[0].set_db(0.0);
        run(v, READ, 1000.0, &mut buf, n);
        assert_eq!(&buf[..n], &scaled(&out, g)[..]);

        // Engine running: V.in is ring B, which only the input level touches; C.out is not scaled.
        SLOTS[8].clients[Side::Engine as usize].store(1, Release);
        let mix = ramp(2).iter().map(|x| -x).collect::<Vec<_>>();
        assert_eq!(run(c, WRITE, 2000.0, &mut mix.clone(), n), 0);
        run(v, READ, 2000.0, &mut buf, n);
        assert_eq!(&buf[..n], &mix[..], "output level must not touch V.in");
        SLOTS[8].levels[0].set_db(db);
        run(v, READ, 2000.0, &mut buf, n);
        assert_eq!(&buf[..n], &scaled(&mix, g)[..]);

        // Mute: exact zeros, on either scope, with and without a non-finite sample in the data.
        SLOTS[8].levels[0].set_db(0.0);
        SLOTS[8].levels[0].set_muted(true);
        run(v, READ, 2000.0, &mut buf, n);
        assert!(buf[..n].iter().all(|&x| x.to_bits() == 0));
        SLOTS[8].levels[0].set_muted(false);
        SLOTS[8].levels[1].set_muted(true);
        let mut nan = out.clone();
        nan[0] = f32::NAN;
        assert_eq!(run(v, WRITE, 3000.0, &mut nan, n), 0);
        run(c, READ, 3000.0, &mut buf, n);
        assert!(buf[..n].iter().all(|&x| x.to_bits() == 0));
        SLOTS[8].levels[1].set_muted(false);
        run(c, READ, 3000.0, &mut buf, n);
        assert!(buf[0].is_nan(), "unmuted unity passes data through");
    }

    /// V.out -> C.in under the HAL's call pattern (read at now-F, then write at now+F): after the calibration
    /// C.in gets the frames F-guard sooner, bit-exact; a stream restart or switching it off goes back to the
    /// exact timeline.
    #[test]
    fn read_ahead_delivers_frames_sooner_after_calibrating() {
        const F: usize = FRAMES;
        let (v, c) = activate(2, 2);
        let val = |t: i64| t as f32 + 1.0;
        let frame_data =
            |t: i64| -> Vec<f32> { (0..F as i64).flat_map(|i| [val(t + i); 2]).collect() };
        // One HAL cycle; returns what C.in read and the sample time of the cycle's input.
        let cycle = |now: f64| {
            let mut rd = vec![];
            assert_eq!(run_at(c, READ, now - F as f64, now, &mut rd, F * 2), 0);
            let mut wr = frame_data(now as i64 + F as i64);
            assert_eq!(run_at(v, WRITE, now + F as f64, now, &mut wr, F * 2), 0);
            rd.truncate(F * 2);
            (rd, now as i64 - F as i64)
        };
        let start = |dev| unsafe { start_io(std::ptr::null_mut(), dev, 0) };
        let stop = |dev| unsafe { stop_io(std::ptr::null_mut(), dev, 0) };
        let ra = &SLOTS[2].read_ahead[Side::Engine as usize];
        let mut now = 10_000.0;
        set_read_ahead(true);
        assert_eq!((start(v), start(c)), (0, 0));
        for k in 0..101 {
            let (rd, t) = cycle(now);
            now += F as f64;
            if k >= 3 {
                assert_eq!(rd, frame_data(t), "exact timeline while calibrating");
            }
        }
        let guard = (F as i64 / 8).max(16);
        assert_eq!(ra.ahead(), F as i64 - guard);
        for _ in 0..20 {
            let (rd, t) = cycle(now);
            now += F as f64;
            assert_eq!(
                rd,
                frame_data(t + F as i64 - guard),
                "frames arrive F - guard sooner"
            );
        }
        assert_eq!(SLOTS[2].overloads[Side::Engine as usize].load(Relaxed), 0);
        // The engine stops and starts again: back to the exact timeline until it has calibrated.
        assert_eq!((stop(c), start(c)), (0, 0));
        let (rd, t) = cycle(now);
        now += F as f64;
        assert_eq!(rd, frame_data(t));
        for _ in 0..101 {
            cycle(now);
            now += F as f64;
        }
        assert!(ra.ahead() > 0);
        // Switched off: exact again, and switching on again recalibrates from scratch.
        set_read_ahead(false);
        let (rd, t) = cycle(now);
        now += F as f64;
        assert_eq!(rd, frame_data(t), "off means off");
        set_read_ahead(true);
        cycle(now);
        now += F as f64;
        assert_eq!(ra.ahead(), 0, "calibration restarted");
        for _ in 0..101 {
            cycle(now);
            now += F as f64;
        }
        assert_eq!(ra.ahead(), F as i64 - guard);
        // A rate change resets the rings (perform_change): calibration restarts without a stop.
        SLOTS[2].want(&Change::Rate(0));
        change(v, Change::Rate(0));
        change(c, Change::Rate(0));
        cycle(now);
        now += F as f64;
        assert_eq!(ra.ahead(), 0, "rate change recalibrates");
        for _ in 0..101 {
            cycle(now);
            now += F as f64;
        }
        assert_eq!(ra.ahead(), F as i64 - guard);
        // A writer that stalls (its cycle never comes) costs one counted shortfall: a silent tail, never
        // frames from past the written ones.
        let mut rd = vec![];
        let read = |now: f64, rd: &mut Vec<f32>| run_at(c, READ, now - F as f64, now, rd, F * 2);
        assert_eq!(read(now, &mut rd), 0); // this cycle's write is skipped
        assert_eq!(SLOTS[2].overloads[Side::Engine as usize].load(Relaxed), 0);
        assert_eq!(read(now + F as f64, &mut rd), 0);
        assert_eq!(SLOTS[2].overloads[Side::Engine as usize].load(Relaxed), 1);
        let real = guard as usize; // frames still written: [now + F - guard, now + F)
        assert_eq!(
            &rd[..real * 2],
            &frame_data(now as i64 + F as i64 - guard)[..real * 2]
        );
        assert!(rd[real * 2..F * 2].iter().all(|&x| x == 0.0), "silent tail");
        set_read_ahead(false);
        assert_eq!((stop(c), stop(v)), (0, 0));
    }

    #[test]
    fn writes_scheduled_in_the_past_or_too_big_are_counted() {
        let (v, c) = activate(6, 2);
        let n = FRAMES * 2;
        let mut out = ramp(2);
        let count = |side: Side| SLOTS[6].overloads[side as usize].load(Relaxed);
        assert_eq!(run_at(v, WRITE, 5000.0, 4000.0, &mut out, n), 0); // ahead of now: on time
        assert_eq!(run_at(v, WRITE, 5000.0, 5000.0, &mut out, n), 0); // exactly now: on time
        assert_eq!(count(Side::Visible), 0);
        assert_eq!(run_at(v, WRITE, 5000.0, 5001.0, &mut out, n), 0); // behind now: late
        assert_eq!(count(Side::Visible), 1);
        let mut big = vec![0.0; (RING_FRAMES as usize / 2 + 1) * 2];
        let k = big.len();
        assert_eq!(run_at(c, WRITE, 9000.0, 0.0, &mut big, k), 0); // C.out has its own counter
        assert_eq!((count(Side::Visible), count(Side::Engine)), (1, 1));
        // Reads never count.
        run_at(c, READ, 5000.0, 9999.0, &mut out, n);
        assert_eq!((count(Side::Visible), count(Side::Engine)), (1, 1));
    }

    proptest::proptest! {
        /// A side's channel count only moves when the host performs a change on that side, so a host that sizes
        /// each I/O buffer by the format it last read is never written past, whatever the device list, the rate,
        /// the other side and I/O start/stop do in between.
        #[test]
        fn io_stays_inside_the_buffer_the_host_sized(
            ops in proptest::collection::vec((0u8..6, proptest::num::u32::ANY, 0usize..2), 1..48),
        ) {
            let (v, c) = activate(3, 2);
            let devs = [v, c];
            let mut view = [2usize; 2]; // per side (index = `Side as usize`): the channel count the host last read
            for (k, x, side) in ops {
                match k {
                    // The device list asks for a channel count; a client asks for a rate.
                    0 => {
                        SLOTS[3].want(&Change::Channels((x % crate::config::MAX_CHANNELS as u32 + 1) as u16));
                    }
                    1 => {
                        SLOTS[3].want(&Change::Rate(x as usize % 3));
                    }
                    // The host performs a (maybe stale) request on one side, then re-reads that side's format.
                    2 => {
                        change(devs[side], if x % 2 == 0 { Change::Channels(1) } else { Change::Rate(0) });
                        view[side] = SLOTS[3].channels[side].load(Acquire) as usize;
                    }
                    3 => unsafe {
                        if x % 2 == 0 {
                            start_io(std::ptr::null_mut(), devs[side], 0);
                        } else {
                            stop_io(std::ptr::null_mut(), devs[side], 0);
                        }
                    },
                    _ => {
                        let frames = if x % 50 == 0 { RING_FRAMES / 2 + 1 } else { 1 + x % 600 };
                        let n = frames as usize * view[side];
                        let mut buf = vec![SENTINEL; n + GUARD];
                        let mut cycle: AudioServerPlugInIOCycleInfo = unsafe { std::mem::zeroed() };
                        cycle.mInputTime.mSampleTime = (x / 64) as f64;
                        cycle.mOutputTime.mSampleTime = (x / 64) as f64;
                        let op = if k == 4 { READ } else { WRITE };
                        let st = unsafe {
                            do_io(std::ptr::null_mut(), devs[side], 0, 0, op, frames, &cycle, buf.as_mut_ptr().cast(), std::ptr::null_mut())
                        };
                        proptest::prop_assert_eq!(st, 0);
                        proptest::prop_assert!(buf[n..].iter().all(|&s| s == SENTINEL), "guard words overwritten");
                    }
                }
                for (have, &want) in SLOTS[3].channels.iter().zip(&view) {
                    proptest::prop_assert_eq!(have.load(Acquire) as usize, want);
                }
            }
        }
    }
}
