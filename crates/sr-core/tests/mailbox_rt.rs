//! The plan mailbox and its retire queue under every interleaving of the control and audio sides.
//!
//! One thread cannot interleave inside a call, but both sides only meet at single atomic swaps, so any
//! order of whole `set_plan` / `process` calls covers what two threads can do. The audio side must never
//! allocate or free (a plan dropped there would be a free), and every plan must be freed exactly once.
mod common;
use common::*;
use proptest::prelude::*;
use sr_core::meters::Meters;
use sr_core::mixer::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};

thread_local! { static AUDIO: Cell<bool> = const { Cell::new(false) }; static HEAP_OPS: Cell<usize> = const { Cell::new(0) }; }
static LIVE: AtomicIsize = AtomicIsize::new(0);

struct Counting;
// SAFETY: delegates to System; the counters are a const thread-local and an atomic (no allocation).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note(1);
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        note(1);
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note(-1);
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note(0);
        unsafe { System.realloc(p, l, n) }
    }
}
fn note(delta: isize) {
    LIVE.fetch_add(delta, Relaxed);
    if AUDIO.with(Cell::get) {
        HEAP_OPS.with(|c| c.set(c.get() + 1));
    }
}
#[global_allocator]
static A: Counting = Counting;

#[derive(Clone, Debug)]
enum Op {
    Set(f32),
    Process,
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn the_audio_side_never_touches_the_heap_and_every_plan_is_freed_once(
        ops in prop::collection::vec(prop_oneof![(-30.0f32..6.0).prop_map(Op::Set), Just(Op::Process)], 1..200),
    ) {
        let mut d = dev();
        d.monitors.push(mon("spk", true, -3.0));
        let l = layout();
        let ins = [vec![0.5f32; 64], vec![0.5f32; 64]];
        let ins: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
        let mut outs = vec![vec![0.0f32; 64]; 4];
        let base = LIVE.load(Relaxed);
        {
            let (mut m, mut h) = mixer(Arc::new(Meters::new()));
            h.set_plan(MixPlan::compile(&d, &l));
            m.process(&ins, &mut outs.iter_mut().map(Vec::as_mut_slice).collect::<Vec<_>>(), 64);
            let mut want = None;
            for op in &ops {
                match op {
                    Op::Set(db) => {
                        d.volume_db = *db;
                        h.set_plan(MixPlan::compile(&d, &l));
                        want = Some(*db);
                    }
                    Op::Process => {
                        let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
                        HEAP_OPS.with(|c| c.set(0));
                        AUDIO.with(|a| a.set(true));
                        m.process(&ins, &mut refs, 64);
                        AUDIO.with(|a| a.set(false));
                        prop_assert_eq!(HEAP_OPS.with(Cell::get), 0, "the audio side allocated or freed");
                    }
                }
            }
            // Whatever order that was, enough blocks to finish the 10 ms fade apply the newest plan completely.
            for _ in 0..100 {
                let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
                m.process(&ins, &mut refs, 64);
            }
            if let Some(db) = want {
                let g = 10f32.powf(db / 20.0);
                prop_assert!((outs[0][63] - 0.5 * g).abs() < 1e-5, "{} vs {}", outs[0][63], 0.5 * g);
            }
        }
        prop_assert_eq!(LIVE.load(Relaxed), base, "a plan leaked or was freed twice");
    }
}
