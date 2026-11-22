//! The plan mailbox with a real audio thread: the control thread publishes plans as fast as it can while
//! the audio thread processes blocks. Run under ThreadSanitizer (see docs/TESTING.md) this is what would
//! show a missing acquire/release; in a plain run it checks that the audio thread never allocates or frees
//! and that the newest plan is the one that ends up applied.
mod common;
use common::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

thread_local! { static HEAP_OPS: Cell<usize> = const { Cell::new(0) }; }
struct Counting;
// SAFETY: delegates to System; the counter is a const thread-local Cell (no allocation).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        HEAP_OPS.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        HEAP_OPS.with(|c| c.set(c.get() + 1));
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Counting = Counting;

#[test]
fn plans_published_while_the_audio_thread_runs_are_applied_without_heap_use() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, -3.0));
    let l = layout();
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&d, &l));
    let done = Arc::new(AtomicBool::new(false));
    let audio = {
        let done = done.clone();
        std::thread::spawn(move || {
            let ins = [vec![0.5f32; 64], vec![0.5f32; 64]];
            let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
            let mut outs = vec![vec![0.0f32; 64]; 4];
            let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
            m.process(&ins, &mut refs, 64);
            HEAP_OPS.with(|c| c.set(0));
            let mut blocks = 0u32;
            // After `done`, the newest plan is published: two more blocks apply it completely.
            let mut after = 0;
            while after < 2 {
                after += u32::from(done.load(Relaxed));
                m.process(&ins, &mut refs, 64);
                assert!(
                    refs[0]
                        .iter()
                        .all(|x| x.is_finite() && x.abs() <= 0.5 * 2.0)
                );
                blocks += 1;
            }
            (HEAP_OPS.with(Cell::get), blocks, refs[0][63])
        })
    };
    for i in 0..5_000 {
        d.volume_db = -((i % 40) as f32);
        h.set_plan(MixPlan::compile(&d, &l));
    }
    d.volume_db = -12.0;
    h.set_plan(MixPlan::compile(&d, &l));
    done.store(true, std::sync::atomic::Ordering::Release);
    let (heap, blocks, last) = audio.join().unwrap();
    assert_eq!(
        heap, 0,
        "the audio thread allocated or freed in {blocks} blocks"
    );
    let want = 0.5 * 10f32.powf(-12.0 / 20.0);
    assert!((last - want).abs() < 1e-5, "{last} vs {want}");
}
