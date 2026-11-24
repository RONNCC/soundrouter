mod common;
use common::*;
use sr_core::config::{MAX_FRAMES, Node, Wire};
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
    h.set_plan(MixPlan::compile(&d, &l, 48_000.0));

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
    h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
    let ins = vec![vec![0.5f32; 64]; 2];
    let ins: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut outs = vec![vec![0.0f32; 64]; 4];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    m.process(&ins, &mut refs, 64);
    h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
    let before = ALLOCS.with(|c| c.get());
    m.process(&ins, &mut refs, 64);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
    // A different routing (a monitor appears) runs the crossfade over the first block.
    let mut d2 = dev();
    d2.monitors.push(mon("spk", true, 0.0));
    h.set_plan(MixPlan::compile(&d2, &l, 48_000.0));
    let before = ALLOCS.with(|c| c.get());
    m.process(&ins, &mut refs, 64);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
}

/// Two chained buses: `s1 -> a -> b -> out`, with the second also fed straight from the source.
fn bus_dev() -> sr_core::config::VirtualDevice {
    let node = |id: &str| Node {
        id: id.into(),
        name: id.into(),
        channels: 2,
        gain_db: -1.0,
        muted: false,
    };
    let via = |s: &str, c, to: &str| Wire {
        to: Some(to.into()),
        ..Wire::new(s, c, c)
    };
    let mut d = dev();
    d.nodes = vec![node("b"), node("a")];
    d.wires = vec![
        via("s1", 0, "a"),
        via("s1", 1, "a"),
        via("a", 0, "b"),
        via("a", 1, "b"),
        via("s1", 0, "b"),
        Wire::new("b", 0, 0),
        Wire::new("b", 1, 1),
    ];
    d
}

#[test]
fn nodes_do_not_allocate_in_process_or_in_a_plan_swap() {
    let d = bus_dev();
    let l = layout();
    let (mut m, mut h) = mixer(Arc::new(Meters::new()));
    h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
    let frames = MAX_FRAMES + 100;
    let ins = vec![vec![0.5f32; frames]; 2];
    let ins: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
    let mut outs = vec![vec![0.0f32; frames]; 4];
    let mut refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
    m.process(&ins, &mut refs, frames); // warm-up
    let before = ALLOCS.with(|c| c.get());
    for _ in 0..200 {
        m.process(&ins, &mut refs, frames);
        m.process(&ins, &mut refs, 64);
    }
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
    // A parameter edit (same route), then a structural one (a node goes away: the crossfade block runs
    // the old plan once more), then back.
    let mut edit = bus_dev();
    edit.nodes[0].gain_db = -9.0;
    h.set_plan(MixPlan::compile(&edit, &l, 48_000.0));
    let before = ALLOCS.with(|c| c.get());
    m.process(&ins, &mut refs, 64);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
    h.set_plan(MixPlan::compile(&dev(), &l, 48_000.0));
    let before = ALLOCS.with(|c| c.get());
    m.process(&ins, &mut refs, 64);
    m.process(&ins, &mut refs, 64);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
    h.set_plan(MixPlan::compile(&d, &l, 48_000.0));
    let before = ALLOCS.with(|c| c.get());
    m.process(&ins, &mut refs, 64);
    assert_eq!(ALLOCS.with(|c| c.get()) - before, 0);
}
