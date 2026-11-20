//! The tap-only aggregate of a split vdev: every process tap lives here, apart from the aggregate that
//! mixes. Any tap in an aggregate makes its safety offset 1024 frames in each direction for every path
//! in it (48 ms against 5 ms at 128 frames), so the taps get their own aggregate on the same clock and
//! hand their audio over through a ring (`crate::tapring`); mic, pass-thru and monitor paths stay fast.
use super::*;
use crate::tapring::{TapRx, TapStats, TapTx};

/// Env: `0` builds one aggregate per vdev with the taps inside, as before the split.
pub const TAP_SPLIT_ENV: &str = "SR_TAP_SPLIT";
/// Env: frames of cushion the ring keeps between the two aggregates (default: the IO buffer).
pub const TAP_CUSHION_ENV: &str = "SR_TAP_CUSHION";
/// What an unset [`TAP_SPLIT_ENV`] means.
pub const TAP_SPLIT_DEFAULT: bool = true;

/// `value` is the env value: `0` off, `1` on, anything else (or unset) the default.
pub fn split_setting(value: Option<&str>, default: bool) -> bool {
    match value {
        Some("0") => false,
        Some("1") => true,
        _ => default,
    }
}

pub(super) fn tap_split_enabled() -> bool {
    split_setting(
        std::env::var(TAP_SPLIT_ENV).ok().as_deref(),
        TAP_SPLIT_DEFAULT,
    )
}

/// Frames the ring keeps behind each read: `SR_TAP_CUSHION` if set to a number, else one IO buffer.
fn cushion_frames(buffer_frames: u32) -> usize {
    std::env::var(TAP_CUSHION_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(buffer_frames as usize)
}

/// Audio-thread state of the tap IOProc.
pub(super) struct TapCb {
    pub tx: TapTx,
    pub shared: Arc<Shared>,
    pub stats: Arc<TapStats>,
    /// Tap buffers are the last this many input buffers.
    pub n_tap_bufs: usize,
}

pub(super) struct TapAgg {
    pub agg: u32,
    proc_id: AudioDeviceIOProcID,
    cb: *mut TapCb,
    pub shared: Arc<Shared>,
    /// Unique per build; names this start in [`Event::IoStart`].
    pub build: u64,
    /// Channels of each tap buffer, in tap-list order.
    pub bufs: Vec<u16>,
    starter: Option<JoinHandle<()>>,
    /// Taps dropped from the vdev while this aggregate still lists them: destroyed after it is gone.
    pub zombies: Vec<Tap>,
    _lease: Lease,
}
// SAFETY: `cb` is only dereferenced by the IOProc thread; control-side access is limited to the atomics in `Shared`.
unsafe impl Send for TapAgg {}

impl TapAgg {
    /// True while the starter thread is still inside `AudioDeviceStart` (TCC prompt open).
    pub fn starting(&self) -> bool {
        self.starter.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Waits out the start, then stops the IOProc: after this no cycle runs and none will.
    pub fn stop(&mut self) {
        if let Some(h) = self.starter.take() {
            let _ = h.join();
        }
        if self.proc_id.is_some() {
            unsafe { AudioDeviceStop(self.agg, self.proc_id) };
        }
    }
}

impl Drop for TapAgg {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            if self.proc_id.is_some() {
                AudioDeviceDestroyIOProcID(self.agg, self.proc_id);
            }
            AudioHardwareDestroyAggregateDevice(self.agg);
            for t in &self.zombies {
                AudioHardwareDestroyProcessTap(t.id);
            }
            drop(Box::from_raw(self.cb));
        }
    }
}

/// Drop a tap aggregate without blocking the caller: one still starting is torn down on a thread.
pub(super) fn retire_tap(t: Option<TapAgg>) {
    let Some(t) = t else { return };
    if t.starting() {
        let _ = std::thread::Builder::new()
            .name("sr-audio-reap".into())
            .spawn(move || drop(t));
    }
}

/// What a vdev needs to build its tap aggregate.
pub(super) struct TapAggSpec<'a> {
    pub dev_id: &'a str,
    /// The companion: the aggregate's one sub-device, main and clock, drift compensation off.
    pub main: &'a str,
    pub taps: &'a [Tap],
    pub buffer_frames: u32,
    pub stats: &'a Arc<TapStats>,
    pub events: Option<&'a Sender<Event>>,
    pub activity: &'a Arc<Activity>,
}

/// Builds and starts the tap aggregate. Its receiver comes back for the caller to publish to the main
/// IOProc; until then (and until the start returns, which blocks on the TCC prompt) the ring just fills.
pub(super) fn build(s: &TapAggSpec) -> Result<(TapAgg, TapRx)> {
    let Some(main_dev) = hal::device_by_uid(s.main) else {
        bail!("tap aggregate main '{}' vanished", s.main);
    };
    let comp_in = hal::stream_channels(main_dev, kAudioObjectPropertyScopeInput);
    let comp_out = hal::stream_channels(main_dev, kAudioObjectPropertyScopeOutput);
    let (u, off) = (NSString::from_str(s.main), NSNumber::new_i32(0));
    let sub = hal::cdict(&[
        (kAudioSubDeviceUIDKey, &u),
        (kAudioSubDeviceDriftCompensationKey, &off),
    ]);
    let tap_dicts: Vec<Retained<_>> = s
        .taps
        .iter()
        .map(|t| {
            let (u, d) = (NSString::from_str(&t.uid), hal::bool_obj(true));
            hal::cdict(&[
                (kAudioSubTapUIDKey, &u),
                (kAudioSubTapDriftCompensationKey, &d),
            ])
        })
        .collect();
    let (subs_arr, taps_arr) = (
        NSArray::from_retained_slice(&[sub]),
        NSArray::from_retained_slice(&tap_dicts),
    );
    let seq = AGG_SEQ.fetch_add(1, Relaxed);
    let agg_uid = NSString::from_str(&format!("{AGG_UID_PREFIX}{}.tap.{seq}", s.dev_id));
    let name = NSString::from_str(&format!("SoundRouter {} taps", s.dev_id));
    let main = NSString::from_str(s.main);
    let (yes, no) = (hal::bool_obj(true), hal::bool_obj(false));
    let d = hal::cdict(&[
        (kAudioAggregateDeviceUIDKey, &agg_uid),
        (kAudioAggregateDeviceNameKey, &name),
        (kAudioAggregateDeviceIsPrivateKey, &yes),
        (kAudioAggregateDeviceMainSubDeviceKey, &main),
        (kAudioAggregateDeviceClockDeviceKey, &main),
        (kAudioAggregateDeviceIsStackedKey, &no),
        (kAudioAggregateDeviceSubDeviceListKey, &subs_arr),
        (kAudioAggregateDeviceTapListKey, &taps_arr),
        (kAudioAggregateDeviceTapAutoStartKey, &no),
    ]);
    let mut agg = 0u32;
    let st = unsafe {
        AudioHardwareCreateAggregateDevice(
            &*(Retained::as_ptr(&d) as *const CFDictionary),
            NonNull::from(&mut agg),
        )
    };
    hal::check("create tap aggregate", st)?;
    let destroy = |what: anyhow::Error| {
        unsafe { AudioHardwareDestroyAggregateDevice(agg) };
        what
    };
    // The tap streams show up a moment after the aggregate itself.
    let want = comp_in.len() + s.taps.len();
    for _ in 0..100 {
        if hal::stream_channels(agg, kAudioObjectPropertyScopeInput).len() == want {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let (ain, aout) = (
        hal::stream_channels(agg, kAudioObjectPropertyScopeInput),
        hal::stream_channels(agg, kAudioObjectPropertyScopeOutput),
    );
    if ain.len() != want || ain[..comp_in.len()] != comp_in[..] || aout != comp_out {
        return Err(destroy(anyhow!(
            "tap aggregate shape in {ain:?} out {aout:?} does not match '{}' ({comp_in:?} / {comp_out:?}) + {} taps",
            s.main,
            s.taps.len()
        )));
    }
    let bufs: Vec<u16> = ain[comp_in.len()..].to_vec();
    let ch: usize = bufs.iter().map(|&c| usize::from(c)).sum();
    if ch == 0 || ch > MAX_CH {
        return Err(destroy(anyhow!(
            "tap aggregate carries {ch} channels, the engine mixes 1..={MAX_CH}"
        )));
    }
    let (tx, rx) = crate::tapring::channel(ch, cushion_frames(s.buffer_frames), s.stats.clone());
    let shared = Arc::new(Shared {
        expected: AtomicU64::new(layout::shape_key_of(&ain, &aout)),
        state: AtomicU64::new(0),
        delay: AtomicU32::new(NO_DELAY),
        lead: AtomicU32::new(NO_DELAY),
    });
    let cb = Box::into_raw(Box::new(TapCb {
        tx,
        shared: shared.clone(),
        stats: s.stats.clone(),
        n_tap_bufs: bufs.len(),
    }));
    let mut t = TapAgg {
        agg,
        proc_id: None,
        cb,
        shared: shared.clone(),
        build: seq,
        bufs,
        starter: None,
        zombies: Vec::new(),
        _lease: s.activity.lease(),
    };
    // From here `Drop` cleans up the aggregate and the callback state; the taps stay the vdev's.
    set_buffer_frames(agg, s.buffer_frames);
    let st = unsafe {
        AudioDeviceCreateIOProcID(
            agg,
            Some(tap_ioproc),
            cb as *mut c_void,
            NonNull::from(&mut t.proc_id),
        )
    };
    hal::check("AudioDeviceCreateIOProcID (taps)", st).context("creating tap IOProc")?;
    t.starter = Some(spawn_starter(
        agg,
        t.proc_id,
        shared,
        s.events.cloned(),
        s.dev_id.to_owned(),
        seq,
    )?);
    Ok((t, rx))
}

unsafe extern "C-unwind" fn tap_ioproc(
    _dev: AudioObjectID,
    now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    it: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _ot: NonNull<AudioTimeStamp>,
    ud: *mut c_void,
) -> i32 {
    let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
        let cb = &mut *(ud as *mut TapCb);
        // The companion's output belongs to the main aggregate: add nothing to it.
        zero_buffers(bufs(output.as_ptr()));
        let valid = AudioTimeStampFlags::SampleTimeValid;
        let (now, it) = (now.as_ref(), it.as_ref());
        if now.mFlags.contains(valid)
            && it.mFlags.contains(valid)
            && let Some(d) = in_out_distance(it.mSampleTime, now.mSampleTime)
        {
            cb.shared.delay.store(d, Relaxed); // how stale the tapped audio is when this IOProc gets it
        }
        tap_run(cb, input.as_ptr(), output.as_ptr());
    }));
    0
}

/// Realtime body: no allocation, locks, logging or ObjC. Copies the tap buffers (the last
/// `n_tap_bufs` of the input list) into the ring, frame-major.
pub(super) unsafe fn tap_run(
    cb: &mut TapCb,
    input: *mut AudioBufferList,
    output: *mut AudioBufferList,
) {
    if input.is_null() {
        return;
    }
    let (ins, outs) = unsafe { (bufs(input), bufs(output)) };
    let key = layout::shape_key(
        ins.iter().map(|b| b.mNumberChannels),
        outs.iter().map(|b| b.mNumberChannels),
    );
    let ch = cb.tx.channels();
    let frames_of =
        |b: &AudioBuffer| b.mDataByteSize as usize / 4 / (b.mNumberChannels as usize).max(1);
    let Some(first) = ins.len().checked_sub(cb.n_tap_bufs) else {
        return;
    };
    let taps = &ins[first..];
    let frames = taps.first().map_or(0, frames_of);
    let expected = cb.shared.expected.load(Acquire);
    if key != expected
        || taps
            .iter()
            .map(|b| b.mNumberChannels as usize)
            .sum::<usize>()
            != ch
        || taps
            .iter()
            .any(|b| b.mData.is_null() || b.mNumberChannels == 0 || frames_of(b) != frames)
    {
        cb.stats.mismatches.fetch_add(1, Relaxed);
        return;
    }
    let mut start = 0;
    while start < frames {
        let n = (frames - start).min(MAX_FRAMES);
        let stage = cb.tx.stage();
        let mut off = 0;
        for b in taps {
            let bch = b.mNumberChannels as usize;
            let src = unsafe { std::slice::from_raw_parts(b.mData as *const f32, frames * bch) };
            for f in 0..n {
                stage[f * ch + off..f * ch + off + bch]
                    .copy_from_slice(&src[(start + f) * bch..(start + f + 1) * bch]);
            }
            off += bch;
        }
        cb.tx.push(n);
        start += n;
    }
}
