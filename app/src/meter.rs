//! Level mapping and peak hold for the meters.

pub const FLOOR_DB: f32 = -60.0;
/// Seconds the peak marker stays put before it falls.
pub const HOLD_SECS: f32 = 1.0;
const FALL_PER_SEC: f32 = 0.8;

/// Linear peak -> 0..=1 bar position over -60..0 dBFS.
pub fn level(lin: f32) -> f32 {
    if lin.is_nan() || lin <= 0.0 {
        return 0.0;
    }
    ((20.0 * lin.log10() - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PeakHold {
    pub value: f32,
    held: f32,
}

impl PeakHold {
    /// Feed the current bar position; returns the peak marker position.
    pub fn update(&mut self, level: f32, dt: f32) -> f32 {
        if level >= self.value {
            self.value = level;
            self.held = 0.0;
        } else {
            self.held += dt;
            if self.held > HOLD_SECS {
                self.value = (self.value - FALL_PER_SEC * dt).max(level);
            }
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_mapping() {
        assert_eq!(level(0.0), 0.0);
        assert_eq!(level(1.0), 1.0);
        assert_eq!(level(2.0), 1.0);
        assert!((level(0.5) - (1.0 - 6.0206 / 60.0)).abs() < 1e-4);
        assert_eq!(level(10f32.powf(-60.0 / 20.0)), 0.0);
        assert_eq!(level(1e-6), 0.0);
        assert!((level(10f32.powf(-30.0 / 20.0)) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn peak_holds_then_falls() {
        let mut p = PeakHold::default();
        assert_eq!(p.update(0.8, 0.033), 0.8);
        for _ in 0..25 {
            assert_eq!(p.update(0.1, 0.033), 0.8); // 0.825 s: still held
        }
        let mut last = 0.8;
        for _ in 0..10 {
            let v = p.update(0.1, 0.033);
            assert!(v <= last);
            last = v;
        }
        assert!(last < 0.8);
        assert_eq!(p.update(0.95, 0.033), 0.95);
    }

    #[test]
    fn level_edge_inputs_stay_in_unit_range() {
        for v in [
            f32::NAN,
            f32::NEG_INFINITY,
            -1.0,
            -0.0,
            f32::INFINITY,
            1e30,
            f32::MIN_POSITIVE,
        ] {
            let l = level(v);
            assert!((0.0..=1.0).contains(&l), "level({v}) = {l}");
        }
        assert_eq!(level(-1.0), 0.0);
        assert_eq!(level(f32::INFINITY), 1.0);
        assert_eq!(level(10f32.powf(6.0 / 20.0)), 1.0); // +6 dBFS clips at the top
    }

    #[test]
    fn level_is_monotonic() {
        let mut last = 0.0;
        for i in 0..=400 {
            let l = level(10f32.powf((i as f32 - 300.0) / 100.0));
            assert!(l >= last);
            last = l;
        }
    }

    #[test]
    fn peak_hold_never_falls_below_current_level_or_zero() {
        let mut p = PeakHold::default();
        p.update(1.0, 0.033);
        for _ in 0..1000 {
            let v = p.update(0.3, 0.033);
            assert!(v >= 0.3);
        }
        assert_eq!(p.value, 0.3);
        let mut q = PeakHold::default();
        q.update(0.2, 0.0);
        for _ in 0..200 {
            assert!(q.update(0.0, 0.1) >= 0.0);
        }
        assert_eq!(q.value, 0.0);
    }

    #[test]
    fn peak_hold_hold_time_is_frame_rate_independent() {
        for dt in [0.01f32, 0.1, 0.5] {
            let mut p = PeakHold::default();
            p.update(0.9, dt);
            let mut t = 0.0;
            while t + dt <= HOLD_SECS {
                assert_eq!(p.update(0.0, dt), 0.9, "dt={dt} t={t}");
                t += dt;
            }
            assert!(p.update(0.0, dt + 0.01) < 0.9 || p.update(0.0, dt) < 0.9);
        }
    }

    #[test]
    fn peak_hold_survives_nan_without_sticking() {
        let mut p = PeakHold::default();
        p.update(0.5, 0.033);
        p.update(f32::NAN, 0.033);
        // A NaN frame must not wedge the marker: a real level afterwards still registers.
        assert_eq!(p.update(0.7, 0.033), 0.7);
    }
}
