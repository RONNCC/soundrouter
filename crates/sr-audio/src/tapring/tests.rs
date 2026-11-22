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
    // The first block after priming fades in (a stream starts at whatever level the app is at).
    for f in 0..B {
        let g = (f + 1) as f32 / B as f32;
        assert_eq!(dst[2 * MAX_FRAMES + f], f as f32 * g);
        assert_eq!(dst[3 * MAX_FRAMES + f], -(f as f32) * g);
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
    // Re-primed: this block fades in again, the one after is plain.
    assert_eq!(dst[B - 1], (3 * B - 1) as f32);
    assert!(dst[..B].windows(2).all(|w| w[0] < w[1]) && dst[0] < 2.0 * B as f32 * 0.02);
    push(&mut tx, 4 * B, B);
    rx.pull(B, &mut dst, 0);
    assert_eq!(dst[0], (3 * B) as f32);
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
    // It plays the block that leaves exactly the cushion behind (fading in: the last sample is whole).
    assert_eq!(dst[B - 1], (9 * B - 1) as f32);
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

/// xorshift: deterministic pseudo-random walk, no dependency and no timer.
fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Where in the stream a read starts, checking it is contiguous. The first block after priming fades in
/// (frame `f` scaled by `(f + 1) / n`), so that one read is checked against the ramp instead; its last
/// frame is at full level either way. Frame 0 of the stream is 0 on both channels.
fn read_start(c0: &[f32], c1: &[f32]) -> f32 {
    let n = c0.len();
    let first = c0[n - 1] - (n - 1) as f32;
    let contiguous = c0.iter().enumerate().all(|(f, &a)| a == first + f as f32);
    let ramped = c0.iter().enumerate().all(|(f, &a)| {
        let want = (first + f as f32) * (f + 1) as f32 / n as f32;
        (a - want).abs() <= 1e-4 * want.abs().max(1.0)
    });
    assert!(contiguous || ramped, "a gap inside a read: {c0:?}");
    assert!(c0.iter().zip(c1).all(|(&a, &b)| b == -a));
    first
}

/// Any mix of pushes and pulls, with any block, read size and cushion, plays frames in the order they
/// were pushed: never a repeat, never an earlier frame after a later one, and each read is contiguous.
/// Underflows in the ring arithmetic would panic here (overflow checks are on in tests).
#[test]
fn random_traffic_never_replays_or_reorders_frames() {
    for (seed, cushion) in [(1u64, 0usize), (2, 1), (3, 37), (4, B), (5, 300), (6, 1000)] {
        let stats = Arc::new(TapStats::default());
        let (mut tx, mut rx) = channel(2, cushion, stats.clone());
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let (mut pushed, mut last, mut played) = (0usize, None::<f32>, 0u32);
        let mut dst = planar();
        for _ in 0..20_000 {
            let n = 1 + next(&mut s) as usize % 300;
            if next(&mut s).is_multiple_of(2) {
                push(&mut tx, pushed, n);
                pushed += n;
            } else {
                rx.pull(n, &mut dst, 0);
                let (c0, c1) = (&dst[..n], &dst[MAX_FRAMES..][..n]);
                if c0.iter().all(|&x| x == 0.0) && c1.iter().all(|&x| x == 0.0) {
                    continue;
                }
                let first = read_start(c0, c1);
                if let Some(prev) = last {
                    assert!(first > prev, "cushion {cushion}: {first} after {prev}");
                }
                last = Some(first + (n - 1) as f32);
                played += 1;
            }
        }
        assert!(played > 100, "cushion {cushion}: nothing played");
    }
}

/// Producer threads, the audio thread and the control thread, as in the engine: each generation is a new
/// ring with its own producer, published through the mailbox while the audio thread keeps pulling.
/// What the audio thread plays is contiguous within a generation and never goes back to an older one.
/// (Under ThreadSanitizer this is what shows a missing acquire/release in the ring or the mailbox.)
#[test]
fn rings_swapped_under_a_running_audio_thread_play_in_order() {
    use std::sync::atomic::AtomicBool;
    const GEN: usize = 100_000;
    let stats = Arc::new(TapStats::default());
    let (mut inp, mut handle) = tap_in(stats.clone());
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let audio = s.spawn(|| {
            let (mut dst, mut last, mut heard) = (planar(), None::<usize>, 0u32);
            while !stop.load(Relaxed) {
                let ch = inp.poll();
                inp.pull(ch.max(2), 100, &mut dst, 0);
                if ch != 2 || dst[..100].iter().all(|&x| x == 0.0) {
                    continue;
                }
                let first = read_start(&dst[..100], &dst[MAX_FRAMES..][..100]) as usize;
                if let Some(prev) = last {
                    assert!(first > prev, "{first} played after {prev}");
                }
                last = Some(first + 99);
                heard += 1;
            }
            heard
        });
        for g in 1..=30usize {
            let (mut tx, rx) = channel(2, B, stats.clone());
            let producer = s.spawn(move || {
                for k in 0..40 {
                    push(&mut tx, g * GEN + k * B, B);
                    std::thread::yield_now();
                }
            });
            handle.publish(rx);
            producer.join().unwrap();
            handle.flush();
        }
        stop.store(true, Relaxed);
        // A pull costs microseconds against a 128-frame block per push: the audio thread heard plenty.
        assert!(
            audio.join().unwrap() > 0,
            "the audio thread never played a block"
        );
    });
}
