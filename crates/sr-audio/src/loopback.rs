//! Direct IOProc on one device: optional 1 kHz output tone, input RMS/peak + dropout accounting.
//! Used by `sr-cli loopback-test` and `sr-cli meter`. `measure_delay` plays one click instead of the
//! tone and times its way back.
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use sr_core::config::MAX_CH;
use sr_core::meters::peak;

use crate::hal;

/// The tone may only go to a virtual device (driver or BlackHole): never a speaker or headphones.
/// Unknown transport fails closed.
fn tone_allowed(transport: Option<u32>) -> bool {
    transport == Some(kAudioDeviceTransportTypeVirtual)
}

pub const TONE_HZ: f32 = 1000.0;
pub const TONE_AMP: f32 = 0.5;

/// Accumulates input statistics; single writer (IO thread), any reader.
pub struct Stats {
    peaks: [AtomicU32; MAX_CH],
    sumsq: AtomicU64, // f64 bits
    samples: AtomicU64,
    pub callbacks: AtomicU64,
    pub dropouts: AtomicU64,
    last_end: AtomicU64, // f64 bits of expected next input sample time; NaN = none
}

impl Stats {
    fn new() -> Stats {
        Stats {
            peaks: std::array::from_fn(|_| AtomicU32::new(0)),
            sumsq: AtomicU64::new(0),
            samples: AtomicU64::new(0),
            callbacks: AtomicU64::new(0),
            dropouts: AtomicU64::new(0),
            last_end: AtomicU64::new(f64::NAN.to_bits()),
        }
    }
    /// Feed one interleaved input block (`frames` x `ch`) plus its start sample time.
    pub fn feed(&self, data: &[f32], ch: usize, sample_time: f64) {
        let frames = data.len() / ch.max(1);
        let expect = f64::from_bits(self.last_end.load(Relaxed));
        if !expect.is_nan() && (sample_time - expect).abs() > 0.5 {
            self.dropouts.fetch_add(1, Relaxed);
        }
        self.last_end
            .store((sample_time + frames as f64).to_bits(), Relaxed);
        let mut ss = 0f64;
        for (i, &x) in data.iter().enumerate() {
            // A NaN or inf sample (a buggy app playing into the device) would poison the energy for good.
            let x = if x.is_finite() { x } else { 0.0 };
            ss += f64::from(x * x);
            let c = i % ch;
            if c < MAX_CH {
                self.peaks[c].fetch_max(x.abs().to_bits(), Relaxed);
            }
        }
        self.sumsq.store(
            (f64::from_bits(self.sumsq.load(Relaxed)) + ss).to_bits(),
            Relaxed,
        );
        self.samples.fetch_add(data.len() as u64, Relaxed);
        self.callbacks.fetch_add(1, Relaxed);
    }
    /// Peak per channel since the last call.
    pub fn take_peaks(&self, ch: usize) -> Vec<f32> {
        (0..ch.min(MAX_CH))
            .map(|c| f32::from_bits(self.peaks[c].swap(0, Relaxed)))
            .collect()
    }
    pub fn rms_dbfs(&self) -> f64 {
        let n = self.samples.load(Relaxed).max(1) as f64;
        20.0 * (f64::from_bits(self.sumsq.load(Relaxed)) / n)
            .sqrt()
            .max(1e-10)
            .log10()
    }
    fn reset_energy(&self) {
        self.sumsq.store(0, Relaxed);
        self.samples.store(0, Relaxed);
    }
}

/// Sine generator; phase in radians, advanced once per frame.
#[derive(Clone, Copy)]
struct Tone {
    phase: f64,
    step: f64,
}

impl Tone {
    fn new(rate: f64) -> Self {
        Tone {
            phase: 0.0,
            step: std::f64::consts::TAU * TONE_HZ as f64 / rate,
        }
    }

    /// Fill interleaved `buf` (`ch` channels); silence when `!on`, phase still advances.
    fn fill(&mut self, buf: &mut [f32], ch: usize, on: bool) {
        for frame in buf.chunks_mut(ch) {
            let v = if on {
                (TONE_AMP as f64 * self.phase.sin()) as f32
            } else {
                0.0
            };
            self.phase = (self.phase + self.step) % std::f64::consts::TAU;
            frame.fill(v);
        }
    }
}

/// Level of the delay probe's click: one sample, so it is not an audible sound, and it only ever
/// enters a virtual device (`tone_allowed`).
pub const CLICK_AMP: f32 = 0.25;
/// The least that counts as the click coming back (-80 dBFS). The path is digital, so silence is exact
/// zeros and the click only has to beat what else is on the input; V's volume control alone can take
/// `CLICK_AMP` down to 0.00015 (-64 dB).
const CLICK_MIN: f32 = 1e-4;
/// The click must exceed the loudest input seen before it was played by this factor: other audio
/// playing into the device (music, a call) is not the click.
const FLOOR_FACTOR: f32 = 3.0;

/// Sample times of the delay probe's click, shared by the IO thread and `measure_delay`.
/// Times are f64 bits; NaN = not happened yet.
pub struct ImpulseTimes {
    /// Set by the caller once the route has settled; the IO thread plays no click before.
    armed: AtomicBool,
    /// Output sample time of the click's one frame.
    out: AtomicU64,
    /// Input sample time of the first sample over the threshold at or after `out`.
    back: AtomicU64,
    /// Loudest input sample before the click was played, as f32 bits (non-negative floats order like
    /// their bits, so `fetch_max` is a float max).
    floor: AtomicU32,
}

impl ImpulseTimes {
    fn new() -> ImpulseTimes {
        ImpulseTimes {
            armed: AtomicBool::new(false),
            out: AtomicU64::new(f64::NAN.to_bits()),
            back: AtomicU64::new(f64::NAN.to_bits()),
            floor: AtomicU32::new(0),
        }
    }

    fn floor(&self) -> f32 {
        f32::from_bits(self.floor.load(Relaxed))
    }

    fn out(&self) -> f64 {
        f64::from_bits(self.out.load(Relaxed))
    }

    fn back(&self) -> f64 {
        f64::from_bits(self.back.load(Relaxed))
    }

    /// True once, at the first output buffer (starting at sample time `time`) after arming: play the
    /// click in that buffer's first frame.
    fn fire(&self, time: f64) -> bool {
        if !self.armed.load(Relaxed) || !self.out().is_nan() {
            return false;
        }
        self.out.store(time.to_bits(), Relaxed);
        true
    }

    /// Look for the click in one interleaved input block starting at sample time `time`.
    fn scan(&self, data: &[f32], ch: usize, time: f64) {
        let out = self.out();
        if out.is_nan() {
            self.floor.fetch_max(peak(data).to_bits(), Relaxed);
            return;
        }
        if !self.back().is_nan() {
            return;
        }
        let threshold = CLICK_MIN.max(FLOOR_FACTOR * self.floor());
        let hit = data.chunks(ch.max(1)).enumerate().find_map(|(i, f)| {
            let t = time + i as f64;
            (t >= out && f.iter().any(|x| x.abs() > threshold)).then_some(t)
        });
        if let Some(t) = hit {
            self.back.store(t.to_bits(), Relaxed);
        }
    }

    /// Frames from the click's output time to its input time, once it has come back.
    fn frames(&self) -> Option<f64> {
        let d = self.back() - self.out();
        (!d.is_nan()).then_some(d)
    }
}

struct Ctx {
    stats: Arc<Stats>,
    enabled: bool,
    tone: Tone,
    /// `Some` in the delay probe: no tone, one click.
    impulse: Option<Arc<ImpulseTimes>>,
}

unsafe extern "C-unwind" fn ioproc(
    _d: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    it: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    ot: NonNull<AudioTimeStamp>,
    ud: *mut c_void,
) -> i32 {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let ctx = &mut *(ud as *mut Ctx);
        let (inp, out) = (input.as_ref(), output.as_ptr());
        if inp.mNumberBuffers > 0 {
            let b = &*inp.mBuffers.as_ptr();
            if !b.mData.is_null() && b.mNumberChannels > 0 {
                let s =
                    std::slice::from_raw_parts(b.mData as *const f32, b.mDataByteSize as usize / 4);
                let ch = b.mNumberChannels as usize;
                ctx.stats.feed(s, ch, it.as_ref().mSampleTime);
                if let Some(i) = &ctx.impulse {
                    i.scan(s, ch, it.as_ref().mSampleTime);
                }
            }
        }
        let outs = std::slice::from_raw_parts_mut(
            (*out).mBuffers.as_mut_ptr(),
            (*out).mNumberBuffers as usize,
        );
        // Every buffer carries the same tone; phase advances once per frame, not per buffer.
        let start = ctx.tone;
        let mut end = start;
        for b in outs {
            if b.mData.is_null() {
                continue;
            }
            let ch = (b.mNumberChannels as usize).max(1);
            let s =
                std::slice::from_raw_parts_mut(b.mData as *mut f32, b.mDataByteSize as usize / 4);
            end = start;
            end.fill(s, ch, ctx.enabled);
        }
        ctx.tone = end;
        if let Some(i) = &ctx.impulse
            && i.fire(ot.as_ref().mSampleTime)
        {
            let outs = std::slice::from_raw_parts_mut(
                (*out).mBuffers.as_mut_ptr(),
                (*out).mNumberBuffers as usize,
            );
            for b in outs.iter_mut().filter(|b| !b.mData.is_null()) {
                let s = std::slice::from_raw_parts_mut(
                    b.mData as *mut f32,
                    b.mDataByteSize as usize / 4,
                );
                s.iter_mut()
                    .take((b.mNumberChannels as usize).max(1))
                    .for_each(|x| *x = CLICK_AMP);
            }
        }
    }));
    0
}

pub struct Probe {
    dev: u32,
    proc_id: AudioDeviceIOProcID,
    ctx: *mut Ctx,
    pub stats: Arc<Stats>,
    pub in_channels: usize,
    pub rate: f64,
}

impl Probe {
    /// Start an IOProc on the device with `uid`. Blocks while a TCC prompt is open.
    pub fn start(uid: &str, tone: bool) -> Result<Probe> {
        Self::start_with(uid, tone, None)
    }

    fn start_with(uid: &str, tone: bool, impulse: Option<Arc<ImpulseTimes>>) -> Result<Probe> {
        let dev = hal::device_by_uid(uid).ok_or_else(|| anyhow!("no device with uid '{uid}'"))?;
        if (tone || impulse.is_some())
            && !tone_allowed(hal::get_global(dev, kAudioDevicePropertyTransportType).ok())
        {
            bail!("refusing to play a sound on '{uid}': not a virtual device");
        }
        let stats = Arc::new(Stats::new());
        let rate = hal::nominal_rate(dev);
        let ctx = Box::into_raw(Box::new(Ctx {
            stats: stats.clone(),
            enabled: tone,
            tone: Tone::new(if rate > 0.0 { rate } else { 48_000.0 }),
            impulse,
        }));
        let mut p = Probe {
            dev,
            proc_id: None,
            ctx,
            stats,
            in_channels: hal::total_channels(dev, kAudioObjectPropertyScopeInput) as usize,
            rate,
        };
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                dev,
                Some(ioproc),
                ctx as *mut c_void,
                NonNull::from(&mut p.proc_id),
            )
        };
        hal::check("AudioDeviceCreateIOProcID", st)?;
        hal::check("AudioDeviceStart", unsafe {
            AudioDeviceStart(dev, p.proc_id)
        })?;
        Ok(p)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.dev, self.proc_id);
            AudioDeviceDestroyIOProcID(self.dev, self.proc_id);
            drop(Box::from_raw(self.ctx));
        }
    }
}

#[derive(Debug)]
pub struct LoopbackReport {
    pub rms_dbfs: f64,
    pub callbacks: u64,
    pub dropouts: u64,
    pub rate: f64,
}

impl LoopbackReport {
    pub fn passed(&self) -> bool {
        self.rms_dbfs > -20.0 && self.dropouts == 0
    }

    pub fn to_self_test(&self) -> sr_core::backend::SelfTest {
        sr_core::backend::SelfTest {
            rms_dbfs: self.rms_dbfs,
            dropouts: self.dropouts,
            rate: self.rate,
            passed: self.passed(),
        }
    }
}

/// True while some process (an engine) has IO running on `uid`'s hidden companion. Then the device's
/// input carries that engine's mix, not the driver's direct loop of its output.
pub fn engine_holds_companion(uid: &str) -> bool {
    hal::device_by_uid(&sr_core::driver_proto::companion_uid(uid)).is_some_and(|d| {
        hal::get_global::<u32>(d, kAudioDevicePropertyDeviceIsRunningSomewhere)
            .is_ok_and(|v| v != 0)
    })
}

/// Play the tone into `uid`'s output and measure what comes back on its input.
pub fn loopback_test(uid: &str, secs: u64) -> Result<LoopbackReport> {
    let p = Probe::start(uid, true)?;
    std::thread::sleep(Duration::from_millis(500)); // let the ring fill
    p.stats.reset_energy();
    p.stats.dropouts.store(0, Relaxed);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(LoopbackReport {
        rms_dbfs: p.stats.rms_dbfs(),
        callbacks: p.stats.callbacks.load(Relaxed),
        dropouts: p.stats.dropouts.load(Relaxed),
        rate: p.rate,
    })
}

/// Play one click into `uid`'s output and time its way back to the input, in milliseconds. Like the
/// tone test it only plays into a virtual device, and while an engine runs for the device the caller
/// must route the click through it (`sr_core::selftest::run_routed`) to measure the engine's path.
pub fn measure_delay(uid: &str) -> Result<f64> {
    let times = Arc::new(ImpulseTimes::new());
    let p = Probe::start_with(uid, false, Some(times.clone()))?;
    std::thread::sleep(Duration::from_millis(500)); // let the route settle
    times.armed.store(true, Relaxed);
    let t0 = Instant::now();
    let frames = loop {
        if let Some(f) = times.frames() {
            break f;
        }
        if t0.elapsed() > Duration::from_secs(2) {
            if FLOOR_FACTOR * times.floor() >= CLICK_AMP {
                bail!(
                    "Other audio is playing into this device, so the click cannot be told apart from it. \
                     Pause it and measure again."
                );
            }
            bail!("The click did not come back through the device.");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if p.rate <= 0.0 || frames < 0.0 {
        bail!("The device reported an unusable clock, so the delay cannot be read.");
    }
    Ok(frames * 1000.0 / p.rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive an `ImpulseTimes` with fake callbacks over a loop that returns audio `delay` frames
    /// late; the output clock leads the input clock by `lead` frames, like a real device's.
    fn simulate(delay: usize, block: usize, lead: f64) -> Option<f64> {
        simulate_with(delay, block, lead, 0.0, 1.0)
    }

    /// `bed`: other audio on the input (a square wave of this peak); `gain`: what V's volume does to the click.
    fn simulate_with(delay: usize, block: usize, lead: f64, bed: f32, gain: f32) -> Option<f64> {
        let times = ImpulseTimes::new();
        let mut wire = vec![0.0f32; 100 * block + 1000]; // played samples by output sample time
        for k in 0..100usize {
            let in_time = (k * block) as f64;
            let input: Vec<f32> = (0..block)
                .map(|i| {
                    let t = k * block + i;
                    let echo = t.checked_sub(delay).map_or(0.0, |w| wire[w] * gain);
                    echo + if t % 7 < 3 { bed } else { -bed }
                })
                .collect();
            times.scan(&input, 1, in_time);
            if k == 3 {
                times.armed.store(true, Relaxed);
            }
            let out_time = in_time + lead;
            if times.fire(out_time) {
                wire[out_time as usize] = CLICK_AMP;
            }
        }
        times.frames()
    }

    #[test]
    fn click_delay_is_the_distance_between_output_and_input_time() {
        for (delay, block, lead) in [
            (300, 128, 0.0),
            (1100, 64, 256.0),
            (420, 512, 100.0), // the click returns in a later buffer than it was played in
            (128, 128, 0.0),
        ] {
            assert_eq!(
                simulate(delay, block, lead),
                Some(delay as f64),
                "{delay} {block} {lead}"
            );
        }
    }

    #[test]
    fn other_audio_on_the_input_is_not_the_click() {
        // Music at 0.05 peak plays into the device: the old fixed threshold (0.02) took its first loud
        // sample for the click and reported a delay of a few frames.
        for (delay, block, lead) in [(300, 128, 0.0), (1100, 64, 256.0), (420, 512, 100.0)] {
            assert_eq!(
                simulate_with(delay, block, lead, 0.05, 1.0),
                Some(delay as f64),
                "{delay} {block}"
            );
        }
    }

    #[test]
    fn a_click_through_a_turned_down_volume_is_still_found() {
        // V's volume at -50 dB leaves 0.25 * 0.0032 = 0.0008, under the old 0.02 threshold.
        for gain in [0.1, 0.01, 0.0032] {
            assert_eq!(
                simulate_with(300, 128, 0.0, 0.0, gain),
                Some(300.0),
                "{gain}"
            );
        }
    }

    #[test]
    fn a_bed_louder_than_the_click_finds_nothing_rather_than_a_wrong_delay() {
        assert_eq!(simulate_with(300, 128, 0.0, 0.3, 1.0), None);
    }

    #[test]
    fn no_click_before_arming_and_none_when_nothing_returns() {
        let t = ImpulseTimes::new();
        assert!(!t.fire(10.0));
        t.armed.store(true, Relaxed);
        assert!(t.fire(10.0));
        assert!(!t.fire(20.0), "one click only");
        assert_eq!(t.frames(), None);
        t.scan(&[0.0, 0.00005, 0.0], 1, 10.0); // under the threshold
        assert_eq!(t.frames(), None);
        t.scan(&[0.5, 0.0], 1, 0.0); // loud, but before the click was played
        assert_eq!(t.frames(), None);
        t.scan(&[0.0, 0.0, 0.0, 0.0, 0.2, 0.0], 2, 10.0); // frame 2 of 3 stereo frames
        assert_eq!(t.frames(), Some(2.0));
    }

    #[test]
    fn rms_and_dropouts() {
        let s = Stats::new();
        let block = vec![0.5f32; 8]; // 4 frames x 2
        s.feed(&block, 2, 0.0);
        s.feed(&block, 2, 4.0);
        assert_eq!(s.dropouts.load(Relaxed), 0);
        s.feed(&block, 2, 12.0); // gap
        assert_eq!(s.dropouts.load(Relaxed), 1);
        assert!((s.rms_dbfs() - 20.0 * 0.5f64.log10()).abs() < 1e-6);
        assert_eq!(s.take_peaks(2), [0.5, 0.5]);
        assert_eq!(s.take_peaks(2), [0.0, 0.0]);
    }

    #[test]
    fn a_nan_sample_does_not_turn_the_rms_into_silence() {
        let s = Stats::new();
        s.feed(&[0.5, f32::NAN, f32::INFINITY, 0.5], 2, 0.0);
        s.feed(&[0.5; 4], 2, 2.0);
        assert!(s.rms_dbfs() > -20.0, "{}", s.rms_dbfs());
        assert_eq!(s.take_peaks(2), [0.5, 0.5]);
    }

    #[test]
    fn tone_continuous_across_buffers() {
        for rate in [44_100.0, 48_000.0, 96_000.0] {
            let mut t = Tone::new(rate);
            let mut all = Vec::new();
            for n in [64usize, 100, 37, 512] {
                let mut b = vec![0.0f32; n * 2];
                t.fill(&mut b, 2, true);
                assert!(b.chunks(2).all(|f| f[0] == f[1]));
                all.extend(b.chunks(2).map(|f| f[0]));
            }
            let max_step = TONE_AMP as f64 * t.step * 1.01;
            for w in all.windows(2) {
                assert!(((w[1] - w[0]) as f64).abs() <= max_step, "rate {rate}");
            }
            let peak = all.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!((peak - TONE_AMP).abs() < 0.01);
        }
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    #[test]
    fn tone_only_on_virtual_transport() {
        assert!(tone_allowed(Some(kAudioDeviceTransportTypeVirtual)));
        for t in [
            kAudioDeviceTransportTypeBuiltIn,
            kAudioDeviceTransportTypeUSB,
            kAudioDeviceTransportTypeBluetooth,
            kAudioDeviceTransportTypeHDMI,
            kAudioDeviceTransportTypeAggregate,
        ] {
            assert!(!tone_allowed(Some(t)), "{t:#x}");
        }
        assert!(!tone_allowed(None));
    }

    /// Needs the SoundRouter driver installed. Reads transport types only: never starts a tone.
    #[test]
    #[ignore = "needs installed driver"]
    fn driver_device_is_virtual() {
        let dev = hal::device_by_uid("soundrouter:v1").expect("soundrouter:v1 installed");
        assert!(tone_allowed(
            hal::get_global(dev, kAudioDevicePropertyTransportType).ok()
        ));
        for d in hal::all_devices() {
            let t = hal::get_global::<u32>(d, kAudioDevicePropertyTransportType).ok();
            if t == Some(kAudioDeviceTransportTypeBuiltIn) {
                assert!(!tone_allowed(t), "{}", hal::device_uid(d));
            }
        }
    }
}
