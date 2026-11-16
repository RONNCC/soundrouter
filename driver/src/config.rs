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
pub fn diff(slots: &[Option<DriverDevice>], want: &[DriverDevice]) -> Vec<Op> {
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
            diff(&slots, &want),
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
        assert!(diff(&cur, &[d("a", "A", 2)]).is_empty());
        assert_eq!(
            diff(&cur, &[d("z", "Z", 2)]),
            vec![Op::Remove(0), Op::Add(1, d("z", "Z", 2))]
        );
        assert_eq!(diff(&cur, &[]), vec![Op::Remove(0)]);
    }
}
