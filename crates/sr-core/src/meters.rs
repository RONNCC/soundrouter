use crate::backend::MeterReading;
use crate::config::{MAX_CH, VirtualDevice};
use crate::mixer::IoLayout;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// Meter slots per direction. An aggregate has the vdev, hardware and tap inputs, so it can
/// exceed `MAX_CH` input buffers; slots at or above this index are not metered.
pub const METER_SLOTS: usize = 4 * MAX_CH;

/// Peak |x| per channel since the last `take`, stored as f32 bits.
/// Non-negative floats order like their bits, so `fetch_max` on the bits is a float max.
/// Inputs are indexed by aggregate input slot; outputs by virtual-device channel (post gain).
pub struct Meters {
    inputs: Box<[AtomicU32]>,
    outputs: Box<[AtomicU32]>,
    /// Blocks silenced because the buffer layout did not match the plan. Written by the audio thread.
    pub mismatch_count: AtomicU64,
    /// IOProc cycles whose sample time did not follow the previous one: the HAL skipped audio (CPU
    /// overload or a stalled thread). Written by the audio thread.
    pub glitch_count: AtomicU64,
}

impl Default for Meters {
    fn default() -> Self {
        let slots = || (0..METER_SLOTS).map(|_| AtomicU32::new(0)).collect();
        Meters {
            inputs: slots(),
            outputs: slots(),
            mismatch_count: AtomicU64::new(0),
            glitch_count: AtomicU64::new(0),
        }
    }
}

/// Peak |x| of a block. NaN and infinite samples are ignored, as the mixer treats them as silence.
pub fn peak(block: &[f32]) -> f32 {
    block
        .iter()
        .fold(0.0, |m, x| if x.is_finite() { m.max(x.abs()) } else { m })
}

fn bump(slot: Option<&AtomicU32>, p: f32) {
    if let Some(s) = slot {
        s.fetch_max(p.to_bits(), Relaxed);
    }
}

impl Meters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Audio thread: record a peak for aggregate input slot `idx`. Out-of-range is ignored.
    pub fn update_input(&self, idx: usize, p: f32) {
        bump(self.inputs.get(idx), p);
    }

    /// Audio thread: record a peak for virtual-device channel `ch`. Out-of-range is ignored.
    pub fn update_output(&self, ch: usize, p: f32) {
        bump(self.outputs.get(ch), p);
    }

    pub fn mismatch_count(&self) -> u64 {
        self.mismatch_count.load(Relaxed)
    }

    pub fn glitch_count(&self) -> u64 {
        self.glitch_count.load(Relaxed)
    }

    /// Swaps peaks to 0 and maps indices to names via the layout.
    /// Source input slots are matched to `layout.inputs` entries `(source id, channel)`.
    pub fn take(&self, layout: &IoLayout, dev: &VirtualDevice) -> MeterReading {
        let take = |a: &AtomicU32| f32::from_bits(a.swap(0, Relaxed));
        let ins: Vec<f32> = self.inputs.iter().map(take).collect();
        let outs: Vec<f32> = self.outputs.iter().map(take).collect();
        let sources = dev
            .sources
            .iter()
            .map(|s| {
                let mut chans = vec![0.0; usize::from(s.channels)];
                for (i, slot) in layout.inputs.iter().enumerate() {
                    if let Some((id, ch)) = slot
                        && *id == s.id
                        && let (Some(c), Some(p)) = (chans.get_mut(usize::from(*ch)), ins.get(i))
                    {
                        *c = f32::max(*c, *p);
                    }
                }
                (s.id.clone(), chans)
            })
            .collect();
        let channels = (0..usize::from(dev.channels))
            .map(|c| outs.get(c).copied().unwrap_or(0.0))
            .collect();
        MeterReading { sources, channels }
    }
}
