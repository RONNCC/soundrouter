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
    let Some((slot, _, _, Part::Device)) = lookup(dev) else {
        return BAD_OBJECT;
    };
    if sample_time.is_null() || host_time.is_null() || seed.is_null() {
        return BAD_OBJECT;
    }
    let per = ticks_per_second() / slot.rate() * RING_FRAMES as f64; // host ticks per period
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
    let ch = slot.channels.load(Acquire) as usize;
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
    let ring = slot.ring(ring_a).filter(|_| frames <= RING_FRAMES / 2);
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
