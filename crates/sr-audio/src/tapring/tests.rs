use super::*;

const B: usize = 128;

fn pair(ch: usize) -> (TapTx, TapRx, Arc<TapStats>) {
    let stats = Arc::new(TapStats::default());
    let (tx, rx) = channel(ch, B, stats.clone());
    (tx, rx, stats)
}

/// Pushes `n` frames whose sample is `start + frame` on channel 0 and `-(start + frame)` on channel 1.
fn push(tx: &mut TapTx, start: usize, n: usize) {
    let ch = tx.channels();
    for f in 0..n {
        let v = (start + f) as f32;
        tx.stage()[f * ch] = v;
        tx.stage()[f * ch + 1] = -v;
    }
    tx.set_block(n);
    tx.push(n);
}

fn planar() -> Vec<f32> {
    vec![9.0; 8 * MAX_FRAMES]
}

#[test]
fn waits_for_the_cushion_then_plays_in_order() {
    let (mut tx, mut rx, stats) = pair(2);
    let mut dst = planar();
    push(&mut tx, 0, B);
    // One block is not a block plus a cushion: silence, and not an underrun.
    rx.pull(B, &mut dst, 2);
    assert!(dst[2 * MAX_FRAMES..][..B].iter().all(|&x| x == 0.0));
    push(&mut tx, B, B);
    rx.pull(B, &mut dst, 2);
    for f in 0..B {
        assert_eq!(dst[2 * MAX_FRAMES + f], f as f32);
        assert_eq!(dst[3 * MAX_FRAMES + f], -(f as f32));
    }
    assert_eq!(stats.counters(), TapCounters::default());
    assert_eq!(stats.level.load(Relaxed) as usize, B);
    // Steady state: one block in, one block out, the cushion stays.
    for k in 2..40 {
        push(&mut tx, k * B, B);
        rx.pull(B, &mut dst, 2);
        assert_eq!(dst[2 * MAX_FRAMES], ((k - 1) * B) as f32, "block {k}");
    }
    assert_eq!(stats.counters(), TapCounters::default());
}

#[test]
fn a_cushion_below_one_block_is_kept_as_a_whole_block() {
    // The tap IOProc delivers whole blocks of B frames and the main IOProc takes whole blocks, so a
    // margin under one block is no margin: it rounds up, and the surplus at the start is dropped.
    assert_eq!(
        [0, 1, B / 4, B, B + 1, 2 * B].map(|c| keep_frames(c, B)),
        [0, B, B, B, 2 * B, 2 * B]
    );
    let stats = Arc::new(TapStats::default());
    let (mut tx, mut rx) = channel(2, B / 4, stats.clone());
    let mut dst = planar();
    push(&mut tx, 0, B);
    rx.pull(B, &mut dst, 0);
    assert!(
        dst[..B].iter().all(|&x| x == 0.0),
        "B < 2B: still prefilling"
    );
    push(&mut tx, B, B);
    rx.pull(B, &mut dst, 0);
    assert_eq!(dst[0], 0.0, "nothing to drop: the ring held exactly 2B");
    assert_eq!(stats.level.load(Relaxed) as usize, B);
    for k in 2..30 {
        push(&mut tx, k * B, B);
        rx.pull(B, &mut dst, 0);
        assert_eq!(stats.level.load(Relaxed) as usize, B, "block {k}");
    }
    assert_eq!(stats.counters(), TapCounters::default());
}

#[test]
fn a_late_tap_push_is_covered_by_a_one_block_cushion() {
    // Every cycle the main IOProc reads before the tap IOProc's block for that cycle arrives.
    let (mut tx, mut rx, stats) = pair(2);
    let mut dst = planar();
    push(&mut tx, 0, B);
    push(&mut tx, B, B);
    rx.pull(B, &mut dst, 0); // primes, plays block 0
    push(&mut tx, 2 * B, B);
    for k in 1..40 {
        rx.pull(B, &mut dst, 0); // late push: this cycle's block is not in yet
        assert_eq!(dst[0], (k * B) as f32, "cycle {k} must not be silent");
        push(&mut tx, (k + 2) * B, B);
    }
    assert_eq!(stats.counters(), TapCounters::default());
}

#[test]
fn a_margin_below_one_block_loses_the_block_on_a_late_push() {
    // What the old quarter-block default did: with only B/4 frames left behind, a late cycle finds
    // less than a block and is replaced by silence. `keep_frames` never lets the receiver get there,
    // except for the explicit cushion of 0.
    let (mut tx, mut rx, stats) = pair(2);
    let mut dst = planar();
    push(&mut tx, 0, B);
    push(&mut tx, B, B);
    rx.pull(B, &mut dst, 0); // primes, leaves B ...
    rx.pull(B - B / 4, &mut dst, 0); // ... and a short read leaves B/4
    assert_eq!(stats.level.load(Relaxed) as usize, B / 4);
    rx.pull(B, &mut dst, 0); // late push: the old margin is short
    assert!(dst[..B].iter().all(|&x| x == 0.0));
    assert_eq!(stats.counters().underruns, 1);

    let stats = Arc::new(TapStats::default());
    let (mut tx, mut rx) = channel(2, 0, stats.clone());
    push(&mut tx, 0, B);
    rx.pull(B, &mut dst, 0); // c = 0 primes on one block and keeps nothing
    assert_eq!(dst[0], 0.0);
    assert_eq!(stats.level.load(Relaxed), 0);
    rx.pull(B, &mut dst, 0); // late push: no margin at all
    assert!(dst[..B].iter().all(|&x| x == 0.0));
    assert_eq!(stats.counters().underruns, 1);
}

#[test]
fn a_dry_ring_is_one_underrun_then_silence_until_the_cushion_is_back() {
    let (mut tx, mut rx, stats) = pair(2);
    let mut dst = planar();
    push(&mut tx, 0, B);
    push(&mut tx, B, B);
    rx.pull(B, &mut dst, 0); // primes and plays
    rx.pull(B, &mut dst, 0); // plays the cushion
    rx.pull(B, &mut dst, 0); // nothing left
    assert_eq!(stats.counters().underruns, 1);
    assert!(dst[..B].iter().all(|&x| x == 0.0));
    for _ in 0..3 {
        rx.pull(B, &mut dst, 0); // not re-primed: silent, but no further underruns
    }
    assert_eq!(stats.counters().underruns, 1);
    push(&mut tx, 2 * B, B);
    push(&mut tx, 3 * B, B);
    rx.pull(B, &mut dst, 0);
    assert_eq!(dst[0], (2 * B) as f32);
    assert_eq!(stats.counters().underruns, 1);
}

#[test]
fn a_pile_up_drops_the_oldest_frames() {
    let (mut tx, mut rx, stats) = pair(2);
    let mut dst = planar();
    for k in 0..10 {
        push(&mut tx, k * B, B);
    }
    rx.pull(B, &mut dst, 0);
    assert_eq!(stats.counters().trims, 1);
    // It plays the block that leaves exactly the cushion behind.
    assert_eq!(dst[0], (8 * B) as f32);
    assert_eq!(stats.level.load(Relaxed) as usize, B);
    push(&mut tx, 10 * B, B);
    rx.pull(B, &mut dst, 0);
    assert_eq!(stats.counters().trims, 1);
    assert_eq!(dst[0], (9 * B) as f32);
}

#[test]
fn a_full_ring_drops_the_new_block_and_counts_it() {
    let (mut tx, mut rx, stats) = pair(2);
    let cap_blocks = 2 * MAX_FRAMES / B;
    for k in 0..cap_blocks + 3 {
        push(&mut tx, k * B, B);
    }
    assert_eq!(stats.counters().dropped, 3);
    // What is in the ring is the oldest data, intact.
    let mut dst = planar();
    rx.pull(B, &mut dst, 0);
    let first = dst[0];
    assert!(first >= 0.0 && first.fract() == 0.0);
}

#[test]
fn a_swap_serves_the_newest_receiver_and_returns_the_old_ones() {
    let stats = Arc::new(TapStats::default());
    let (mut inp, mut handle) = tap_in(stats.clone());
    assert_eq!(inp.poll(), 0);
    let (_t1, r1) = channel(2, B, stats.clone());
    let (mut t2, r2) = channel(4, B, stats.clone());
    handle.publish(r1);
    handle.publish(r2); // replaces the unread one
    assert_eq!(inp.poll(), 4);
    let mut dst = planar();
    // The plan wants 2 tap channels but the receiver has 4 (a swap in flight): silence, no panic.
    inp.pull(2, B, &mut dst, 0);
    assert!(dst[..B].iter().all(|&x| x == 0.0));
    for f in 0..2 * B {
        for c in 0..4 {
            t2.stage()[f * 4 + c] = 1.0 + c as f32;
        }
        if f == B - 1 || f == 2 * B - 1 {
            t2.set_block(B);
            t2.push(B);
        }
    }
    inp.pull(4, B, &mut dst, 1); // primes (2 blocks in)
    inp.pull(4, B, &mut dst, 1);
    assert_eq!(dst[MAX_FRAMES], 1.0);
    assert_eq!(dst[4 * MAX_FRAMES], 4.0);
    handle.publish(TapRx::none(stats));
    assert_eq!(inp.poll(), 0);
    handle.flush();
}
