//! Direct IOProc on one device: optional 1 kHz output tone, input RMS/peak + dropout accounting.
//! Used by `sr-cli loopback-test` and `sr-cli meter`.
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioBufferList, AudioTimeStamp};
use sr_core::config::MAX_CH;

use crate::hal;

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

struct Ctx {
    stats: Arc<Stats>,
    enabled: bool,
    tone: Tone,
}

unsafe extern "C-unwind" fn ioproc(
    _d: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    it: NonNull<AudioTimeStamp>,
    output: NonNull<AudioBufferList>,
    _ot: NonNull<AudioTimeStamp>,
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
                ctx.stats
                    .feed(s, b.mNumberChannels as usize, it.as_ref().mSampleTime);
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
        let dev = hal::device_by_uid(uid).ok_or_else(|| anyhow!("no device with uid '{uid}'"))?;
        let stats = Arc::new(Stats::new());
        let rate = hal::nominal_rate(dev);
        let ctx = Box::into_raw(Box::new(Ctx {
            stats: stats.clone(),
            enabled: tone,
            tone: Tone::new(if rate > 0.0 { rate } else { 48_000.0 }),
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

#[cfg(test)]
mod tests {
    use super::*;

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
