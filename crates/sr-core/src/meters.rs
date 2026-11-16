// SLICE B OWNS THIS FILE
// Scaffold stub: frozen signatures from plan §5. Bodies are placeholders until Slice B lands.
use crate::backend::MeterReading;
use crate::config::{MAX_CH, VirtualDevice};
use crate::mixer::IoLayout;
use std::sync::atomic::AtomicU32;

/// [AtomicU32; MAX_CH] in + out, f32 bits.
pub struct Meters {
    pub inputs: [AtomicU32; MAX_CH],
    pub outputs: [AtomicU32; MAX_CH],
}
impl Meters {
    /// Swaps peaks to 0 and maps indices to names via the layout.
    pub fn take(&self, _layout: &IoLayout, _dev: &VirtualDevice) -> MeterReading {
        MeterReading::default()
    }
}
