//! Cable colours: every cable of the graph gets its own palette slot, chosen so that cables that
//! meet (a jack, a card, an output channel) look different. Slot `s` is `Tokens::channel(s)`:
//! slots 0..8 are the eight hues, 8..32 their lightness tiers, which only come into play once
//! every hue is taken among the cables that meet.
//!
//! The colour must not flicker: a cable's slot depends on its stable key (source id or monitor
//! device, channel numbers; never an index or a screen position) and only on the cables that meet
//! it. Cables are walked in key order, each takes the slot that costs least against those walked
//! before it, and ties go to the hue its own key prefers. An unrelated wire, a reordered
//! source list or a repaint changes nothing.
//!
//! shortcut: "neighbouring in y" is judged by neighbouring rows of one card, not by screen
//! position, because positions move when cards expand or reorder and would recolour cables.
//! Upgrade if two distant cables that happen to cross read as one.
use crate::model::{Link, Port};
use sr_core::config::VirtualDevice;

const HUES: usize = 8;
/// Palette slots in use: the eight hues and three tiers of them.
pub const SLOTS: usize = 32;
/// Hue pairs that read alike at cable width (indices into the palette: blue, orange, green, pink,
/// yellow, sky, vermillion, violet).
const NEAR: [(usize, usize); 5] = [(0, 5), (0, 7), (1, 4), (1, 6), (3, 7)];

const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv(h: u64, bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(h, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Spreads the bits of an FNV hash, so its high half is as good as its low half.
fn mix(mut h: u64) -> u64 {
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

fn ch(p: Port) -> u16 {
    match p {
        Port::Src { ch, .. }
        | Port::OutIn { ch }
        | Port::OutOut { ch }
        | Port::Mon { ch, .. }
        | Port::NodeIn { ch, .. }
        | Port::NodeOut { ch, .. } => ch,
    }
}

/// The card a port sits on: sources, output-channel pairs, monitors, mixer strips.
fn card(p: Port) -> (u8, usize) {
    match p {
        Port::Src { src, .. } => (0, src),
        Port::OutIn { ch } | Port::OutOut { ch } => (1, usize::from(ch / 2)),
        Port::Mon { mon, .. } => (2, mon),
        Port::NodeIn { node, .. } | Port::NodeOut { node, .. } => (3, node),
    }
}

/// What names a cable across sessions and edits: its stage, the source id, node id or monitor
/// device, and the channel at each end (plus the node it feeds, for a cable into a mixer).
pub fn key(dev: &VirtualDevice, l: Link) -> u64 {
    let node_id = |node: usize| dev.nodes.get(node).map_or("", |n| n.id.as_str());
    let (tag, id) = match (l.from, l.to) {
        (Port::Src { src, .. }, _) => (b's', dev.sources.get(src).map_or("", |s| &s.id)),
        (Port::NodeOut { node, .. }, _) => (b'n', node_id(node)),
        (_, Port::Mon { mon, .. }) => (b'm', dev.monitors.get(mon).map_or("", |m| &m.device_uid)),
        _ => (b'?', ""),
    };
    let h = fnv(fnv(FNV0, &[tag]), id.as_bytes());
    let h = fnv(h, &[0xff]);
    let h = fnv(fnv(h, &ch(l.from).to_le_bytes()), &ch(l.to).to_le_bytes());
    // Cables into output channels keep the keys they had before mixers existed.
    mix(match l.to {
        Port::NodeIn { node, .. } => fnv(fnv(h, b">"), node_id(node).as_bytes()),
        _ => h,
    })
}

/// How much two cables meet: 0 when they share no card, else 2 for the card, plus 2 for a shared
/// jack, 2 when one ends at the output channel or mixer row the other starts from, 1 for
/// neighbouring rows.
fn relation(a: Link, b: Link) -> u32 {
    let (ca, cb) = ([card(a.from), card(a.to)], [card(b.from), card(b.to)]);
    if !ca.iter().any(|c| cb.contains(c)) {
        return 0;
    }
    let through = |x: Link, y: Link| match (x.to, y.from) {
        (Port::OutIn { ch: i }, Port::OutOut { ch: o }) => i == o,
        (Port::NodeIn { node: m, ch: i }, Port::NodeOut { node: n, ch: o }) => m == n && i == o,
        _ => false,
    };
    let next = |p: Port, q: Port| card(p) == card(q) && ch(p).abs_diff(ch(q)) == 1;
    2 + 2 * u32::from(a.from == b.from || a.to == b.to)
        + 2 * u32::from(through(a, b) || through(b, a))
        + u32::from(next(a.from, b.from) || next(a.to, b.to))
}

fn near(a: usize, b: usize) -> bool {
    NEAR.iter().any(|&p| p == (a, b) || p == (b, a))
}

/// A palette slot for every link of `all`, in `all`'s order.
pub fn assign(dev: &VirtualDevice, all: &[Link]) -> Vec<u8> {
    let mut order: Vec<(u64, usize)> = all
        .iter()
        .enumerate()
        .map(|(i, &l)| (key(dev, l), i))
        .collect();
    order.sort_unstable();
    let mut slots = vec![0u8; all.len()];
    for (pos, &(k, i)) in order.iter().enumerate() {
        // What the cables walked before, that meet this one, already wear.
        let (mut users, mut weight, mut exact) = ([0u32; HUES], [0u32; HUES], [0u32; SLOTS]);
        for &(_, j) in &order[..pos] {
            let w = relation(all[i], all[j]);
            if w > 0 {
                let s = usize::from(slots[j]);
                users[s % HUES] += 1;
                weight[s % HUES] += w;
                exact[s] += w;
            }
        }
        let prefer = (k >> 32) as usize % HUES;
        let cost = |s: usize| {
            let h = s % HUES;
            let alike: u32 = (0..HUES).filter(|&o| near(h, o)).map(|o| weight[o]).sum();
            // a hue no meeting cable wears, then the lightest use of one, then a slot of their
            // own, then a hue unlike theirs, then the plainest tier, then the hue the key likes
            (
                users[h],
                weight[h],
                exact[s],
                alike,
                s / HUES,
                (h + HUES - prefer) % HUES,
            )
        };
        slots[i] = (0..SLOTS).min_by_key(|&s| cost(s)).map_or(0, |s| s as u8);
    }
    slots
}

/// The slots of the shown device's cables, kept until its wires change.
#[derive(Default)]
pub struct Cables {
    fingerprint: u64,
    slots: Vec<u8>,
}

impl Cables {
    /// Slot of every link of `all`; recomputed only when the cables (or their order) changed.
    pub fn slots(&mut self, dev: &VirtualDevice, all: &[Link]) -> &[u8] {
        let fp = all
            .iter()
            .fold(fnv(FNV0, &all.len().to_le_bytes()), |h, &l| {
                fnv(h, &key(dev, l).to_le_bytes())
            });
        if fp != self.fingerprint || self.slots.len() != all.len() {
            self.slots = assign(dev, all);
            self.fingerprint = fp;
        }
        &self.slots
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{bare_config, links};
    use sr_core::config::{MonitorWire, Source, SourceKind, Wire};
    use std::collections::{HashMap, HashSet};

    fn dev(sources: usize, monitors: usize, channels: u16) -> VirtualDevice {
        let mut d = bare_config().devices.remove(0);
        d.channels = channels;
        d.sources = (1..=sources)
            .map(|n| Source {
                id: format!("s{n}"),
                kind: SourceKind::PassThru,
                channels: 2,
                volume_db: 0.0,
                muted: false,
                mute_original: false,
                name: String::new(),
            })
            .collect();
        d.monitors = (0..monitors)
            .map(|n| sr_core::config::Monitor {
                device_uid: format!("hw{n}"),
                enabled: true,
                volume_db: 0.0,
                wires: Vec::new(),
                name: String::new(),
            })
            .collect();
        d.wires.clear();
        d
    }

    fn wire(d: &mut VirtualDevice, s: usize, src_ch: u16, out_ch: u16) {
        let source = d.sources[s].id.clone();
        d.wires.push(Wire::new(source, src_ch, out_ch));
    }

    /// Every cable's slot by its key: what a person sees, whatever the indices are.
    fn by_key(d: &VirtualDevice) -> HashMap<u64, u8> {
        let all = links(d);
        let slots = assign(d, &all);
        all.iter()
            .zip(slots)
            .map(|(&l, s)| (key(d, l), s))
            .collect()
    }

    /// 8 sources x 2 channels onto 2 output channels, which feed 3 monitors.
    fn fan() -> VirtualDevice {
        let mut d = dev(8, 3, 2);
        for s in 0..8 {
            for c in 0..2 {
                wire(&mut d, s, c, c);
            }
        }
        for m in &mut d.monitors {
            m.wires = (0..2)
                .map(|c| MonitorWire {
                    out_ch: c,
                    dst_ch: c,
                })
                .collect();
        }
        d
    }

    fn hues(slots: impl Iterator<Item = u8>) -> usize {
        slots
            .map(|s| usize::from(s) % HUES)
            .collect::<HashSet<_>>()
            .len()
    }

    #[test]
    fn a_fan_uses_every_hue_among_the_cables_of_a_card() {
        let d = fan();
        let all = links(&d);
        let slots = assign(&d, &all);
        let on = |card_of: (u8, usize)| {
            all.iter()
                .zip(&slots)
                .filter(|(l, _)| card(l.from) == card_of || card(l.to) == card_of)
                .map(|(_, &s)| s)
                .collect::<Vec<_>>()
        };
        // the output card meets all 22 cables, each source card its 2, each monitor card its 2
        let out = on((1, 0));
        assert_eq!(out.len(), 22);
        assert_eq!(hues(out.into_iter()), 8);
        for s in 0..8 {
            assert_eq!(hues(on((0, s)).into_iter()), 2, "source {s}");
        }
        for m in 0..3 {
            assert_eq!(hues(on((2, m)).into_iter()), 2, "monitor {m}");
        }
        // 16 cables on the card cannot all differ in hue, but no slot repeats among 8
        let src_cables: Vec<u8> = all
            .iter()
            .zip(&slots)
            .filter(|(l, _)| matches!(l.from, Port::Src { .. }))
            .map(|(_, &s)| s)
            .collect();
        assert_eq!(src_cables.len(), 16);
        assert_eq!(src_cables.iter().collect::<HashSet<_>>().len(), 16);
    }

    #[test]
    fn few_cables_never_share_a_hue_when_they_meet() {
        // three sources into channel 1, one of them also into channel 2
        let mut d = dev(3, 1, 2);
        for s in 0..3 {
            wire(&mut d, s, 0, 0);
        }
        wire(&mut d, 0, 1, 1);
        d.monitors[0].wires = vec![
            MonitorWire {
                out_ch: 0,
                dst_ch: 0,
            },
            MonitorWire {
                out_ch: 1,
                dst_ch: 1,
            },
        ];
        let all = links(&d);
        let slots = assign(&d, &all);
        assert_eq!(all.len(), 6);
        assert_eq!(hues(slots.iter().copied()), 6);
        // the cable out of channel 1 differs from every cable that goes into it
        let out = all
            .iter()
            .position(|l| l.from == Port::OutOut { ch: 0 })
            .unwrap();
        for (l, &s) in all.iter().zip(&slots) {
            if l.to == (Port::OutIn { ch: 0 }) {
                assert_ne!(s, slots[out]);
            }
        }
    }

    #[test]
    fn a_cable_keeps_its_colour_when_an_unrelated_wire_comes_or_goes() {
        let mut d = dev(4, 1, 4);
        for (s, c, o) in [(0, 0, 0), (0, 1, 1), (1, 0, 0), (2, 0, 1)] {
            wire(&mut d, s, c, o);
        }
        d.monitors[0].wires = vec![MonitorWire {
            out_ch: 0,
            dst_ch: 0,
        }];
        let before = by_key(&d);
        // another source into the second output pair meets none of them
        wire(&mut d, 3, 0, 2);
        let after = by_key(&d);
        for (k, s) in &before {
            assert_eq!(after[k], *s);
        }
        assert_eq!(after.len(), before.len() + 1);
        // and out again
        d.wires.pop();
        assert_eq!(by_key(&d), before);
    }

    #[test]
    fn only_cables_that_meet_the_new_one_may_change() {
        let mut d = dev(4, 1, 4);
        for (s, c, o) in [(0, 0, 0), (0, 1, 1), (1, 0, 0), (1, 1, 1), (2, 0, 2)] {
            wire(&mut d, s, c, o);
        }
        let before = by_key(&d);
        let old = links(&d);
        wire(&mut d, 3, 0, 0);
        let all = links(&d);
        let new = *all.last().unwrap();
        let after = by_key(&d);
        for l in &old {
            let k = key(&d, *l);
            if after[&k] != before[&k] {
                assert!(
                    relation(*l, new) > 0,
                    "{l:?} changed without meeting {new:?}"
                );
            }
        }
    }

    #[test]
    fn reordering_the_sources_changes_nothing() {
        let d = fan();
        let want = by_key(&d);
        let mut r = d.clone();
        r.sources.reverse();
        assert_eq!(by_key(&r), want);
        // and so does a different wire order
        let mut w = d;
        w.wires.reverse();
        assert_eq!(by_key(&w), want);
    }

    #[test]
    fn the_same_wires_give_the_same_slots_every_time() {
        let d = fan();
        let all = links(&d);
        let first = assign(&d, &all);
        for _ in 0..5 {
            assert_eq!(assign(&d, &all), first);
        }
    }

    #[test]
    fn near_hues_are_avoided_when_the_choice_is_free() {
        // two cables through one jack take hues that do not read alike
        let mut d = dev(1, 0, 2);
        wire(&mut d, 0, 0, 0);
        wire(&mut d, 0, 0, 1);
        let s = assign(&d, &links(&d));
        assert_ne!(s[0], s[1]);
        assert!(!near(usize::from(s[0]) % HUES, usize::from(s[1]) % HUES));
    }

    #[test]
    fn the_memo_follows_the_wire_set() {
        let mut d = fan();
        let mut c = Cables::default();
        let all = links(&d);
        let first = c.slots(&d, &all).to_vec();
        assert_eq!(first, assign(&d, &all));
        assert_eq!(c.slots(&d, &all), first);
        d.wires.pop();
        let less = links(&d);
        assert_eq!(c.slots(&d, &less), assign(&d, &less));
        assert_eq!(c.slots(&d, &[]).len(), 0);
    }

    #[test]
    fn slots_stay_in_the_palette() {
        let mut d = dev(8, 0, 8);
        for s in 0..8 {
            for c in 0..2 {
                for o in 0..8 {
                    wire(&mut d, s, c, o);
                }
            }
        }
        assert!(
            assign(&d, &links(&d))
                .iter()
                .all(|&s| usize::from(s) < SLOTS)
        );
    }
}
