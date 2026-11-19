//! Device-list validation and diffing. Pure Rust, no CoreFoundation.
use sr_core::driver_proto::{COMPANION_SUFFIX, DEVICE_UID_PREFIX, DriverDevice, MAX_DEVICES};

pub const MAX_CHANNELS: u16 = 64;
const MAX_UID_BYTES: usize = 128;
const MAX_NAME_CHARS: usize = 64;

/// Check a requested device list. Names are truncated; everything else must be valid as given.
pub fn validate(mut list: Vec<DriverDevice>) -> Result<Vec<DriverDevice>, String> {
    if list.len() > MAX_DEVICES {
        return Err(format!("{} devices, max {MAX_DEVICES}", list.len()));
    }
    for (i, d) in list.iter_mut().enumerate() {
        if d.uid.is_empty() || d.uid.len() > MAX_UID_BYTES || d.uid.contains('\0') {
            return Err(format!("device {i}: bad uid"));
        }
        // Any local process may set the list: keep it out of other devices' uid namespace (BuiltInSpeakerDevice, ...).
        if d.uid.len() <= DEVICE_UID_PREFIX.len() || !d.uid.starts_with(DEVICE_UID_PREFIX) {
            return Err(format!(
                "device {i}: uid must start with {DEVICE_UID_PREFIX}"
            ));
        }
        if d.uid.ends_with(COMPANION_SUFFIX) {
            return Err(format!(
                "device {i}: uid must not end with {COMPANION_SUFFIX}"
            ));
        }
        if !(1..=MAX_CHANNELS).contains(&d.channels) {
            return Err(format!(
                "device {i}: channels {} not in 1..={MAX_CHANNELS}",
                d.channels
            ));
        }
        d.name = d.name.trim().chars().take(MAX_NAME_CHARS).collect();
        if d.name.is_empty() {
            return Err(format!("device {i}: empty name"));
        }
    }
    for (i, d) in list.iter().enumerate() {
        if list[..i].iter().any(|e| e.uid == d.uid) {
            return Err(format!("duplicate uid {}", d.uid));
        }
    }
    Ok(list)
}

/// One reconcile step against the fixed slot pool. Apply in order: removals free slots for adds.
#[derive(Debug, PartialEq)]
pub enum Op {
    Remove(usize),
    Rename(usize, String),
    Channels(usize, u16),
    Add(usize, DriverDevice),
}

/// Steps that turn the live slots into `want` (already validated, so `want.len() <= slots.len()`).
/// `freed[s]` is when slot `s` was last removed (0 = never used), on the same clock as `now`. A slot is
/// quarantined until `settle` after its removal: the HAL may still run I/O on its ids with the old
/// buffers, so handing them to a wider device would overflow them. That holds however many passes run.
/// Adds take never-used slots first, then the longest-free ones.
pub fn diff(
    slots: &[Option<DriverDevice>],
    freed: &[u64],
    now: u64,
    settle: u64,
    want: &[DriverDevice],
) -> Vec<Op> {
    let mut ops = vec![];
    // Slots freed in this pass are live in `slots`, so never handed out in it either; adds that do not
    // fit wait for a later pass.
    let mut empty = vec![];
    let mut kept = vec![false; want.len()];
    for (s, cur) in slots.iter().enumerate() {
        let Some(cur) = cur else {
            if freed[s] == 0 || now.saturating_sub(freed[s]) >= settle {
                empty.push(s);
            }
            continue;
        };
        match want.iter().position(|w| w.uid == cur.uid) {
            None => {
                ops.push(Op::Remove(s));
            }
            Some(i) => {
                kept[i] = true;
                if want[i].name != cur.name {
                    ops.push(Op::Rename(s, want[i].name.clone()));
                }
                if want[i].channels != cur.channels {
                    ops.push(Op::Channels(s, want[i].channels));
                }
            }
        }
    }
    empty.sort_by_key(|&s| freed[s]); // stable: ties keep slot order
    let mut free = empty.into_iter();
    for w in want
        .iter()
        .zip(&kept)
        .filter_map(|(w, k)| (!k).then_some(w))
    {
        if let Some(s) = free.next() {
            ops.push(Op::Add(s, w.clone()));
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `uid` is the part after `DEVICE_UID_PREFIX`.
    fn d(uid: &str, name: &str, channels: u16) -> DriverDevice {
        DriverDevice {
            uid: format!("{DEVICE_UID_PREFIX}{uid}"),
            name: name.into(),
            channels,
        }
    }

    #[test]
    fn validate_accepts_and_trims() {
        let ok = validate(vec![d("a", "  Name  ", 2), d("b", &"x".repeat(200), 64)]).unwrap();
        assert_eq!(ok[0].name, "Name");
        assert_eq!(ok[1].name.chars().count(), MAX_NAME_CHARS);
        assert_eq!(validate(vec![]).unwrap(), vec![]);
    }

    #[test]
    fn validate_rejects_bad_input() {
        assert!(
            validate(vec![d("a", "n", 2), d("a", "m", 2)])
                .unwrap_err()
                .contains("duplicate")
        );
        assert!(validate(vec![d("", "n", 2)]).is_err());
        for uid in [
            "BuiltInSpeakerDevice",
            "soundrouter",
            "v1",
            "SoundRouter:v1",
        ] {
            let raw = DriverDevice {
                uid: uid.into(),
                ..d("a", "n", 2)
            };
            assert!(
                validate(vec![raw]).unwrap_err().contains("must start"),
                "{uid}"
            );
        }
        assert!(validate(vec![d("a\0b", "n", 2)]).is_err());
        assert!(validate(vec![d("a.engine", "n", 2)]).is_err());
        assert!(validate(vec![d(&"u".repeat(129 - DEVICE_UID_PREFIX.len()), "n", 2)]).is_err());
        assert!(validate(vec![d(&"u".repeat(128 - DEVICE_UID_PREFIX.len()), "n", 2)]).is_ok());
        assert!(validate(vec![d("a", "n", 0)]).is_err());
        assert!(validate(vec![d("a", "n", 65)]).is_err());
        assert!(validate(vec![d("a", "   ", 2)]).is_err());
        let many = (0..=MAX_DEVICES)
            .map(|i| d(&format!("u{i}"), "n", 2))
            .collect();
        assert!(validate(many).is_err());
        let full = (0..MAX_DEVICES)
            .map(|i| d(&format!("u{i}"), "n", 2))
            .collect();
        assert_eq!(validate(full).unwrap().len(), MAX_DEVICES);
    }

    #[test]
    fn diff_quarantines_recently_freed_slots() {
        // Slot 0 freed at t=5, slot 1 never used, slot 2 freed at t=2; quarantine 3.
        let slots = vec![None, None, None];
        let want = [d("a", "A", 2), d("b", "B", 2), d("c", "C", 2)];
        assert_eq!(
            diff(&slots, &[5, 0, 2], 6, 3, &want),
            vec![Op::Add(1, d("a", "A", 2)), Op::Add(2, d("b", "B", 2))]
        );
        // Once slot 0 settled it is handed out, after the never-used and the longer-free slots.
        assert_eq!(
            diff(&slots, &[5, 0, 2], 8, 3, &want),
            vec![
                Op::Add(1, d("a", "A", 2)),
                Op::Add(2, d("b", "B", 2)),
                Op::Add(0, d("c", "C", 2)),
            ]
        );
    }

    /// Quarantine in the simulations below (think ms).
    const Q: u64 = 1000;

    /// Applies `diff` at time `t` like `pass` does and checks that no add takes a slot inside its quarantine.
    fn apply(slots: &mut [Option<DriverDevice>], freed: &mut [u64], t: u64, want: &[DriverDevice]) {
        for op in diff(slots, freed, t, Q, want) {
            match op {
                Op::Remove(s) => {
                    assert!(slots[s].take().is_some());
                    freed[s] = t;
                }
                Op::Rename(s, n) => slots[s].as_mut().unwrap().name = n,
                Op::Channels(s, c) => slots[s].as_mut().unwrap().channels = c,
                Op::Add(s, dev) => {
                    assert!(
                        freed[s] == 0 || t - freed[s] >= Q,
                        "slot {s} freed at {} reused at {t}",
                        freed[s]
                    );
                    assert!(slots[s].replace(dev).is_none());
                }
            }
        }
    }

    #[test]
    fn rapid_lists_on_a_full_pool_never_reuse_a_slot_inside_the_quarantine() {
        // Full pool; A, B, C swap the last device for wider ones in quick succession, then D replaces C
        // after it was placed. A pass runs every tick, as concurrent reconciles could: the quarantine must
        // hold however many passes run, not only within one.
        let base: Vec<_> = (0..MAX_DEVICES - 1)
            .map(|i| d(&format!("u{i}"), "n", 2))
            .collect();
        let with = |last: DriverDevice| [base.clone(), vec![last]].concat();
        let lists = [
            (0, with(d("a", "A", 4))),
            (5, with(d("b", "B", 6))),
            (10, with(d("c", "C", 8))),
            (1500, with(d("d", "D", 16))),
        ];
        let mut slots: Vec<_> = with(d("old", "O", 2)).into_iter().map(Some).collect();
        let mut freed = vec![0; MAX_DEVICES];
        // (time, uid) each time the last slot's occupant changes.
        let mut history = vec![(0, Some(format!("{DEVICE_UID_PREFIX}old")))];
        for t in 1..4000 {
            let want = &lists.iter().rfind(|(at, _)| *at <= t).unwrap().1;
            apply(&mut slots, &mut freed, t, want);
            let now = slots[MAX_DEVICES - 1].as_ref().map(|x| x.uid.clone());
            if history.last().unwrap().1 != now {
                history.push((t, now));
            }
        }
        assert_eq!(
            slots,
            with(d("d", "D", 16))
                .into_iter()
                .map(Some)
                .collect::<Vec<_>>()
        );
        // The slot sits empty for exactly Q after each removal; A and B never get it, C waits for it.
        let s = |u: &str| Some(format!("{DEVICE_UID_PREFIX}{u}"));
        assert_eq!(
            history,
            [
                (0, s("old")),
                (1, None),
                (1 + Q, s("c")),
                (1500, None),
                (1500 + Q, s("d"))
            ]
        );
    }

    #[test]
    fn diff_add_remove_change() {
        let slots = vec![
            Some(d("a", "A", 2)),
            Some(d("b", "B", 2)),
            None,
            Some(d("c", "C", 8)),
        ];
        let want = [
            d("c", "C2", 16),
            d("a", "A", 2),
            d("n", "N", 4),
            d("m", "M", 1),
        ];
        assert_eq!(
            diff(&slots, &[0; 4], 1, Q, &want),
            vec![
                Op::Remove(1),
                Op::Rename(3, "C2".into()),
                Op::Channels(3, 16),
                Op::Add(2, d("n", "N", 4)),
            ]
        );
        // "m" did not fit: slot 1 was freed in that pass, so it is only handed out once it settled.
        let next = vec![
            Some(d("a", "A", 2)),
            None,
            Some(d("n", "N", 4)),
            Some(d("c", "C2", 16)),
        ];
        assert!(diff(&next, &[0, 1, 0, 0], Q, Q, &want).is_empty());
        assert_eq!(
            diff(&next, &[0, 1, 0, 0], 1 + Q, Q, &want),
            vec![Op::Add(1, d("m", "M", 1))]
        );
    }

    #[test]
    fn full_pool_swap_never_changes_a_live_slot_in_place() {
        // 16 devices, one swapped for a wider one: pass 1 only removes; the add comes in pass 2.
        let mut slots: Vec<_> = (0..MAX_DEVICES)
            .map(|i| Some(d(&format!("u{i}"), "n", 2)))
            .collect();
        let mut want: Vec<_> = slots.iter().flatten().cloned().collect();
        want[15] = d("wide", "w", 8);
        let ops = diff(&slots, &[0; MAX_DEVICES], 1, Q, &want);
        assert_eq!(ops, vec![Op::Remove(15)]);
        slots[15] = None;
        let mut freed = [0; MAX_DEVICES];
        freed[15] = 1;
        assert_eq!(
            diff(&slots, &freed, 1 + Q, Q, &want),
            vec![Op::Add(15, d("wide", "w", 8))]
        );
    }

    #[test]
    fn diff_noop_and_full_replace() {
        let cur = vec![Some(d("a", "A", 2)), None];
        assert!(diff(&cur, &[0; 2], 1, Q, &[d("a", "A", 2)]).is_empty());
        assert_eq!(
            diff(&cur, &[0; 2], 1, Q, &[d("z", "Z", 2)]),
            vec![Op::Remove(0), Op::Add(1, d("z", "Z", 2))]
        );
        assert_eq!(diff(&cur, &[0; 2], 1, Q, &[]), vec![Op::Remove(0)]);
    }

    use proptest::prelude::*;
    use std::collections::HashSet;

    /// A requested list as `validate` would pass it: unique uids, at most MAX_DEVICES, small alphabets so
    /// consecutive lists overlap (keeps, renames, channel changes, removals and adds all occur).
    fn want_list() -> impl Strategy<Value = Vec<DriverDevice>> {
        prop::collection::vec((0..24u8, 0..3u8, 1..=3u16), 0..=MAX_DEVICES + 4).prop_map(|v| {
            let mut seen = HashSet::new();
            let list = v
                .into_iter()
                .filter(|&(u, ..)| seen.insert(u))
                .take(MAX_DEVICES)
                .map(|(u, n, c)| d(&format!("u{u}"), &format!("name{n}"), c))
                .collect();
            validate(list).unwrap()
        })
    }

    proptest! {
        /// Replays `diff` the way `reconcile` does (ops in order, removals stamp the pass time, a second pass
        /// one quarantine later) over any sequence of lists arriving at any gaps, and checks the pool after every step.
        #[test]
        fn diff_keeps_the_slot_pool_consistent(steps in prop::collection::vec((want_list(), 0..2 * Q), 1..30)) {
            let mut slots: Vec<Option<DriverDevice>> = vec![None; MAX_DEVICES];
            let mut freed = vec![0u64; MAX_DEVICES];
            let mut t = 1;
            for (want, gap) in steps {
                t += gap;
                let before = slots.clone();
                let freed_before = freed.clone();
                let (mut removed, mut added) = (HashSet::new(), vec![]);
                // Pass 1, then (like `reconcile`) a second pass after the settle time when some wanted devices did not fit.
                for pass in 0..2 {
                    if pass == 1 {
                        t += Q;
                    }
                    let ops = diff(&slots, &freed, t, Q, &want);
                    if pass == 1 {
                        prop_assert!(ops.iter().all(|o| matches!(o, Op::Add(..))), "second pass must only add");
                    }
                    for op in ops {
                        match op {
                            Op::Remove(s) => {
                                prop_assert!(slots[s].take().is_some(), "remove of empty slot {s}");
                                freed[s] = t;
                                removed.insert(s);
                            }
                            Op::Rename(s, n) => slots[s].as_mut().unwrap().name = n,
                            Op::Channels(s, c) => slots[s].as_mut().unwrap().channels = c,
                            Op::Add(s, dev) => {
                                prop_assert!(slots[s].is_none(), "add into occupied slot {s}");
                                prop_assert!(freed[s] == 0 || t - freed[s] >= Q, "slot {s} reused inside its quarantine");
                                slots[s] = Some(dev);
                                if pass == 0 {
                                    added.push(s);
                                }
                            }
                        }
                    }
                    if slots.iter().flatten().count() == want.len() {
                        break;
                    }
                }
                // The pool now holds exactly the wanted devices, uids unique, never more than MAX_DEVICES.
                let mut have: Vec<_> = slots.iter().flatten().cloned().collect();
                prop_assert!(have.len() <= MAX_DEVICES);
                let uids: HashSet<_> = have.iter().map(|x| x.uid.clone()).collect();
                prop_assert_eq!(uids.len(), have.len(), "duplicate uid in pool");
                let mut want_sorted = want.clone();
                have.sort_by(|a, b| a.uid.cmp(&b.uid));
                want_sorted.sort_by(|a, b| a.uid.cmp(&b.uid));
                prop_assert_eq!(have, want_sorted);
                // Devices that stay keep their slot (their object ids must not change).
                for (s, b) in before.iter().enumerate() {
                    if let Some(b) = b.as_ref().filter(|b| want.iter().any(|w| w.uid == b.uid)) {
                        prop_assert_eq!(slots[s].as_ref().map(|x| &x.uid), Some(&b.uid));
                    }
                }
                // Adds take the longest-free empty slots first and never a slot removed in this pass.
                let empty: Vec<_> = (0..MAX_DEVICES).filter(|&s| before[s].is_none()).collect();
                for &a in &added {
                    prop_assert!(!removed.contains(&a), "slot {a} removed and re-added in one pass");
                    for &e in empty.iter().filter(|&&e| freed_before[e] < freed_before[a]) {
                        prop_assert!(added.contains(&e), "slot {a} taken before older-free slot {e}");
                    }
                }
            }
        }
    }
}
