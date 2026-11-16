//! Device-list validation and diffing. Pure Rust, no CoreFoundation.
use sr_core::driver_proto::{COMPANION_SUFFIX, DriverDevice, MAX_DEVICES};

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
/// `freed[s]` is the removal epoch of slot `s` (0 = never used). Adds take never-used slots first,
/// then the longest-free ones, so the HAL can finish tearing a removed device down before its ids return.
pub fn diff(slots: &[Option<DriverDevice>], freed: &[u64], want: &[DriverDevice]) -> Vec<Op> {
    let mut ops = vec![];
    // Slots freed in this pass go last, so an id is not handed to a different device in the same pass.
    let (mut empty, mut removed) = (vec![], vec![]);
    let mut kept = vec![false; want.len()];
    for (s, cur) in slots.iter().enumerate() {
        let Some(cur) = cur else {
            empty.push(s);
            continue;
        };
        match want.iter().position(|w| w.uid == cur.uid) {
            None => {
                ops.push(Op::Remove(s));
                removed.push(s);
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
    let mut free = empty.into_iter().chain(removed);
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

    fn d(uid: &str, name: &str, channels: u16) -> DriverDevice {
        DriverDevice {
            uid: uid.into(),
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
        assert!(validate(vec![d("a\0b", "n", 2)]).is_err());
        assert!(validate(vec![d("a.engine", "n", 2)]).is_err());
        assert!(validate(vec![d(&"u".repeat(129), "n", 2)]).is_err());
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
    fn diff_does_not_reuse_just_freed_slot() {
        // Slot 0 was freed in the previous pass (epoch 5), slot 1 never used, slot 2 freed long ago (epoch 2).
        let slots = vec![None, None, None];
        assert_eq!(
            diff(
                &slots,
                &[5, 0, 2],
                &[d("a", "A", 2), d("b", "B", 2), d("c", "C", 2)]
            ),
            vec![
                Op::Add(1, d("a", "A", 2)),
                Op::Add(2, d("b", "B", 2)),
                Op::Add(0, d("c", "C", 2)),
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
            diff(&slots, &[0; 4], &want),
            vec![
                Op::Remove(1),
                Op::Rename(3, "C2".into()),
                Op::Channels(3, 16),
                Op::Add(2, d("n", "N", 4)),
                Op::Add(1, d("m", "M", 1)),
            ]
        );
    }

    #[test]
    fn diff_noop_and_full_replace() {
        let cur = vec![Some(d("a", "A", 2)), None];
        assert!(diff(&cur, &[0; 2], &[d("a", "A", 2)]).is_empty());
        assert_eq!(
            diff(&cur, &[0; 2], &[d("z", "Z", 2)]),
            vec![Op::Remove(0), Op::Add(1, d("z", "Z", 2))]
        );
        assert_eq!(diff(&cur, &[0; 2], &[]), vec![Op::Remove(0)]);
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
        /// Replays `diff` the way `reconcile` does (ops in order, removals stamp a rising epoch) over any
        /// sequence of lists and checks the pool after every step.
        #[test]
        fn diff_keeps_the_slot_pool_consistent(steps in prop::collection::vec(want_list(), 1..30)) {
            let mut slots: Vec<Option<DriverDevice>> = vec![None; MAX_DEVICES];
            let mut freed = vec![0u64; MAX_DEVICES];
            let mut epoch = 0;
            for want in steps {
                let before = slots.clone();
                let freed_before = freed.clone();
                let (mut removed, mut added) = (HashSet::new(), vec![]);
                for op in diff(&slots, &freed, &want) {
                    match op {
                        Op::Remove(s) => {
                            prop_assert!(slots[s].take().is_some(), "remove of empty slot {s}");
                            epoch += 1;
                            freed[s] = epoch;
                            removed.insert(s);
                        }
                        Op::Rename(s, n) => slots[s].as_mut().unwrap().name = n,
                        Op::Channels(s, c) => slots[s].as_mut().unwrap().channels = c,
                        Op::Add(s, dev) => {
                            prop_assert!(slots[s].is_none(), "add into occupied slot {s}");
                            slots[s] = Some(dev);
                            added.push(s);
                        }
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
                // Adds take the longest-free empty slots first; a slot removed in this very pass only
                // comes back when every slot that was already empty is used up.
                let empty: Vec<_> = (0..MAX_DEVICES).filter(|&s| before[s].is_none()).collect();
                for &a in &added {
                    if removed.contains(&a) {
                        prop_assert!(empty.iter().all(|e| added.contains(e)), "slot {a} reused with free slots left");
                    } else {
                        for &e in empty.iter().filter(|&&e| freed_before[e] < freed_before[a]) {
                            prop_assert!(added.contains(&e), "slot {a} taken before older-free slot {e}");
                        }
                    }
                }
            }
        }
    }
}
