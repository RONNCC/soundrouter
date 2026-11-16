//! Level mapping and peak hold for the meters.

pub const FLOOR_DB: f32 = -60.0;
/// Seconds the peak marker stays put before it falls.
pub const HOLD_SECS: f32 = 1.0;
const FALL_PER_SEC: f32 = 0.8;

/// Linear peak -> 0..=1 bar position over -60..0 dBFS.
pub fn level(lin: f32) -> f32 {
    if lin <= 0.0 {
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
}
