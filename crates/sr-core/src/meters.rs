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
    /// Same peaks as `inputs`, drained by [`Meters::drain_source_peak`] (the engine's own watch) instead
    /// of the window's [`Meters::take`], so neither reader takes values from the other.
    watched: Box<[AtomicU32]>,
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
            watched: slots(),
            outputs: slots(),
            mismatch_count: AtomicU64::new(0),
            glitch_count: AtomicU64::new(0),
        }
    }
}

/// Max of `f(|x|)` over `block` with 8 independent lanes, so the loop vectorises: a serial
/// `fold(max)` is one dependent chain. `max` is exact and order-free for non-NaN values, so the
/// result equals the serial fold bit for bit.
#[inline(always)]
fn lane_max(block: &[f32], f: impl Fn(f32) -> f32) -> f32 {
    let mut lanes = [0.0f32; 8];
    let chunks = block.chunks_exact(8);
    let tail = chunks.remainder();
    for c in chunks {
        for (m, x) in lanes.iter_mut().zip(c) {
            *m = m.max(f(x.abs()));
        }
    }
    let m = tail.iter().fold(0.0f32, |m, x| m.max(f(x.abs())));
    lanes.iter().fold(m, |m, l| m.max(*l))
}

/// Peak |x| of a block. NaN and infinite samples are ignored, as the mixer treats them as silence.
/// Finite values above the mixer's clamp still count (the meter shows the raw input).
pub fn peak(block: &[f32]) -> f32 {
    // `a <= MAX` is false for NaN and inf, so this is `is_finite` as a branch-free select.
    lane_max(block, |a| if a <= f32::MAX { a } else { 0.0 })
}

/// `peak` for a block that is known to hold only finite samples (skips the select).
/// Not meaningful otherwise: inf would be reported as inf.
pub(crate) fn peak_finite(block: &[f32]) -> f32 {
    lane_max(block, |a| a)
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
        bump(self.watched.get(idx), p);
    }

    /// Largest input peak of the slots that carry source `source_id` (per `layout`) since the last call
    /// for that source; resets them. Independent of [`Meters::take`].
    pub fn drain_source_peak(&self, layout: &IoLayout, source_id: &str) -> f32 {
        layout
            .inputs
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.as_ref().is_some_and(|(id, _)| id == source_id))
            .filter_map(|(i, _)| self.watched.get(i))
            .map(|a| f32::from_bits(a.swap(0, Relaxed)))
            .fold(0.0, f32::max)
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The pre-vectorisation implementation, kept as the oracle.
    fn peak_ref(block: &[f32]) -> f32 {
        block
            .iter()
            .fold(0.0, |m, x| if x.is_finite() { m.max(x.abs()) } else { m })
    }

    /// Every bit pattern (NaN payloads, denormals, inf) plus the awkward values by name.
    fn sample() -> impl Strategy<Value = f32> {
        prop_oneof![
            any::<u32>().prop_map(f32::from_bits),
            Just(f32::NAN),
            Just(f32::INFINITY),
            Just(f32::NEG_INFINITY),
            Just(1e31),
            Just(-1e31),
            Just(f32::MAX),
            Just(-f32::MAX),
            Just(f32::MIN_POSITIVE / 2.0),
            Just(-0.0),
            -1.0f32..=1.0,
        ]
    }

    #[test]
    fn watch_and_window_drain_independently() {
        let m = Meters::new();
        let layout = IoLayout {
            inputs: vec![
                None,
                Some(("a".into(), 0)),
                Some(("b".into(), 0)),
                Some(("a".into(), 1)),
            ],
            outputs: vec![],
        };
        m.update_input(1, 0.25);
        m.update_input(3, 0.5);
        m.update_input(2, 0.75);
        let dev: VirtualDevice = serde_json::from_str(r#"{"id":"v1","name":"v1"}"#).unwrap();
        m.take(&layout, &dev); // the window reads first and must not hide the peaks from the watch
        assert_eq!(m.drain_source_peak(&layout, "a"), 0.5);
        assert_eq!(m.drain_source_peak(&layout, "a"), 0.0, "drained");
        assert_eq!(m.drain_source_peak(&layout, "b"), 0.75);
        assert_eq!(m.drain_source_peak(&layout, "none"), 0.0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]
        #[test]
        fn peak_matches_reference(block in prop::collection::vec(sample(), 0..70)) {
            prop_assert_eq!(peak(&block).to_bits(), peak_ref(&block).to_bits());
        }

        #[test]
        fn peak_finite_matches_reference_on_finite(
            block in prop::collection::vec(sample().prop_filter("finite", |x| x.is_finite()), 0..70)
        ) {
            prop_assert_eq!(peak_finite(&block).to_bits(), peak_ref(&block).to_bits());
        }
    }

    #[test]
    fn peak_edge_cases() {
        for n in 0..=17 {
            for pos in 0..n {
                for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                    let mut b = vec![0.25f32; n];
                    b[pos] = bad;
                    let want = if n > 1 { 0.25 } else { 0.0 };
                    assert_eq!(peak(&b), want, "n={n} pos={pos} bad={bad}");
                }
                let mut b = vec![0.0f32; n];
                b[pos] = -1e31;
                assert_eq!(peak(&b), 1e31);
                assert_eq!(peak_finite(&b), 1e31);
            }
        }
        assert_eq!(peak(&[]), 0.0);
    }

    /// ns per call of the old and new peak. `cargo test --release -p sr-core --lib -- --ignored --nocapture bench_peak`
    #[test]
    #[ignore = "benchmark"]
    fn bench_peak() {
        use std::hint::black_box;
        use std::time::Instant;
        fn time(f: fn(&[f32]) -> f32, b: &[f32]) -> f64 {
            let iters = 200_000;
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..iters {
                        black_box(f(black_box(b)));
                    }
                    t.elapsed().as_nanos() as f64 / iters as f64
                })
                .fold(f64::MAX, f64::min)
        }
        for n in [64, 128, 256] {
            let b: Vec<f32> = (0..n)
                .map(|i| ((i * 7) % 101) as f32 / 101.0 - 0.5)
                .collect();
            println!(
                "n {n:>3}: old {:>6.1} ns  guarded {:>6.1} ns  finite {:>6.1} ns",
                time(peak_ref, &b),
                time(peak, &b),
                time(peak_finite, &b)
            );
        }
    }
}
