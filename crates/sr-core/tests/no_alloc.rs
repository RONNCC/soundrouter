mod common;
use common::*;
use sr_core::config::MAX_FRAMES;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

thread_local! { static ALLOCS: Cell<usize> = const { Cell::new(0) }; }

struct Counting;
// SAFETY: delegates to System; the counter is a const-initialised thread-local Cell (no allocation).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCS.with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static A: Counting = Counting;

#[test]
fn process_does_not_allocate() {
    let mut d = dev();
    d.monitors.push(mon("spk", true, -3.0));
    let l = layout();
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&d, &l));

    let frames = MAX_FRAMES + 100; // exercises chunking
    let ins = vec![vec![0.5f32; frames]; 2];
    let ins: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut outs = vec![vec![0.0f32; frames]; 4];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    let small_ins: Vec<&[f32]> = ins.iter().map(|s| &s[..256]).collect();

    m.process(&ins, &mut refs, frames); // warm-up
    let before = ALLOCS.with(|c| c.get());
    for i in 0..1000 {
        if i % 2 == 0 {
            m.process(&ins, &mut refs, frames);
        } else {
            m.process(&small_ins, &mut refs, 256);
        }
    }
    m.process(&ins[..1], &mut refs, 256); // mismatch path
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
}

#[test]
fn plan_swap_on_audio_thread_does_not_allocate() {
    let d = dev();
    let l = layout();
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&d, &l));
    let ins = vec![vec![0.5f32; 64]; 2];
    let ins: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut outs = vec![vec![0.0f32; 64]; 4];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    m.process(&ins, &mut refs, 64);
    h.set_plan(MixPlan::compile(&d, &l));
    let before = ALLOCS.with(|c| c.get());
    m.process(&ins, &mut refs, 64);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
}
