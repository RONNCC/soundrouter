// SLICE B OWNS THIS FILE
// Scaffold stub: frozen signatures from plan §5. Bodies are placeholders until Slice B lands.
use crate::config::VirtualDevice;
use crate::meters::Meters;
use std::sync::Arc;

pub enum OutPort {
    Device(u16),
    Monitor { uid: String, ch: u16 },
}
/// index = aggregate planar ch; None = ignore (e.g. vdev input)
pub struct IoLayout {
    pub inputs: Vec<Option<(String, u16)>>,
    pub outputs: Vec<Option<OutPort>>,
}
/// Precomputed sparse taps: (in_idx, out_idx, gain) + per-output gain.
#[derive(Default)]
pub struct MixPlan {
    _private: (),
}
impl MixPlan {
    pub fn compile(_dev: &VirtualDevice, _layout: &IoLayout) -> MixPlan {
        MixPlan::default()
    }
}
/// Audio-thread side; zero alloc in process.
pub struct Mixer;
/// Control side.
pub struct MixerHandle;
pub fn mixer(_meters: Arc<Meters>) -> (Mixer, MixerHandle) {
    (Mixer, MixerHandle)
}
impl MixerHandle {
    /// rtrb push Box<MixPlan>; old boxes come back on a 2nd rtrb and drop here.
    pub fn set_plan(&mut self, _p: MixPlan) {}
}
impl Mixer {
    /// Planar; zero outputs first; no plan = silence.
    pub fn process(&mut self, _inputs: &[&[f32]], outputs: &mut [&mut [f32]], _frames: usize) {
        for out in outputs.iter_mut() {
            out.fill(0.0);
        }
    }
}
