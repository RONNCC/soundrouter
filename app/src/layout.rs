//! Geometry of the four-column graph: card rects, port positions, hit-testing, wire curves.
use crate::model::Port;
use eframe::egui::{Color32, Pos2, Rect, Stroke, Vec2, epaint::CubicBezierShape, pos2, vec2};

pub const HEAD_H: f32 = 34.0;
/// Minimum body height of a card with a big icon.
pub const ICON_BODY: f32 = 48.0;
pub const ROW_H: f32 = 24.0;
/// Strip under the channel rows that holds the options chevron.
pub const FOOT_H: f32 = 16.0;
/// Extra height of an expanded card: the volume row.
pub const OPTIONS_H: f32 = 32.0;
/// Extra height of an expanded app card: the "Mute in original output" row.
pub const MUTE_H: f32 = 28.0;
pub const OUT_PAD: f32 = 10.0;
pub const GAP: f32 = 16.0;
/// Space above the first card for the column titles.
pub const TOP: f32 = 40.0;
pub const SIDE: f32 = 24.0;
/// Narrowest the Processing column gets (the 1000 px window); it widens to `PROC_W_MAX`.
pub const PROC_W_MIN: f32 = 120.0;
pub const PROC_W_MAX: f32 = 150.0;
/// Height of the empty-state placeholder in the Processing column.
pub const PROC_EMPTY_H: f32 = 96.0;
/// Gutters at the 750 px pane: Sources|Processing, Processing|Output channels, Output channels|Monitors.
const GUTTERS: [f32; 3] = [40.0, 40.0, 52.0];
/// 2 SIDE + 3 cards + processing column + gutters at the minimum pane.
const MIN_PANE: f32 = 750.0;
/// Module card parts below its channel rows: the vertical meters, the limiter line, one row
/// each for the fader, every effect and Add effect, and the padding at the foot.
pub const STRIP_METER_H: f32 = 64.0;
pub const STRIP_LIMIT_H: f32 = 16.0;
pub const STRIP_ROW_H: f32 = 32.0;
const STRIP_PAD: f32 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CardId {
    Src(usize),
    /// Channel pair index (0 = channels 1 & 2).
    Out(usize),
    Mon(usize),
    /// A mixer strip (`VirtualDevice::nodes`).
    Node(usize),
}

#[derive(Clone, Copy, Debug)]
pub struct CardLayout {
    pub id: CardId,
    pub rect: Rect,
    pub rows: u16,
    /// Options footer (sources and monitors); false for the output-channel cards.
    pub options: bool,
    pub expanded: bool,
    /// Sources carry a big icon left of the channel rows.
    pub icon: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct CardSpec {
    pub rows: u16,
    pub expanded: bool,
    /// App source: its options carry the mute row.
    pub mute_row: bool,
}

/// What sizes a module card: its channel rows, its effects, whether it shows a limiter line, and
/// the height of the effect opened in it (0 when none is).
#[derive(Clone, Copy, Debug)]
pub struct NodeSpec {
    pub rows: u16,
    pub fx: usize,
    pub limiter: bool,
    pub open: f32,
}

pub fn strip_height(s: NodeSpec) -> f32 {
    HEAD_H
        + ROW_H * f32::from(s.rows)
        + STRIP_METER_H
        + if s.limiter { STRIP_LIMIT_H } else { 0.0 }
        + STRIP_ROW_H * (2 + s.fx) as f32
        + s.open
        + STRIP_PAD
}

#[derive(Debug)]
pub struct Layout {
    pub origin: Pos2,
    pub cards: Vec<CardLayout>,
    /// Width of a Sources / Output channels / Monitors card.
    pub card_w: f32,
    /// Left edge of Sources, Processing, Output channels and Monitors (relative to `origin`).
    pub cols: [f32; 4],
    /// Width of the Processing column and of every module card in it.
    pub proc_w: f32,
    /// Placeholder shown where the first module goes; `None` once the device has modules.
    pub proc_empty: Option<Rect>,
    pub size: Vec2,
}

pub fn body_height(rows: u16, icon: bool) -> f32 {
    let rows = ROW_H * f32::from(rows);
    if icon { rows.max(ICON_BODY) } else { rows }
}

pub fn card_height(rows: u16, options: bool, expanded: bool, icon: bool, mute_row: bool) -> f32 {
    let body = HEAD_H + body_height(rows, icon);
    if !options {
        body + OUT_PAD
    } else if expanded {
        body + FOOT_H + OPTIONS_H + if mute_row { MUTE_H } else { 0.0 }
    } else {
        body + FOOT_H
    }
}

impl Layout {
    /// `width` is the pane width; `origin` the top-left of the graph area. Four columns, the
    /// Processing one always present: 48 + 3x150 + 120 + 40 + 40 + 52 = 750 px at the 1000 px
    /// window. Past that a fifth of every pixel goes to each card, a twentieth to the Processing
    /// column and the rest to the gutters.
    pub fn compute(
        origin: Pos2,
        width: f32,
        sources: &[CardSpec],
        out_pairs: usize,
        monitors: &[CardSpec],
        nodes: &[NodeSpec],
    ) -> Layout {
        let extra = (width - MIN_PANE).max(0.0);
        let card_w = (150.0 + 0.2 * (width - MIN_PANE)).clamp(150.0, 250.0);
        let proc_w = (PROC_W_MIN + 0.05 * extra).clamp(PROC_W_MIN, PROC_W_MAX);
        let gutter_sum: f32 = GUTTERS.iter().sum();
        let spare = width - 2.0 * SIDE - 3.0 * card_w - proc_w;
        let k = (spare / gutter_sum).max(1.0);
        let cols = [
            SIDE,
            SIDE + card_w + GUTTERS[0] * k,
            SIDE + card_w + GUTTERS[0] * k + proc_w + GUTTERS[1] * k,
            SIDE + 2.0 * card_w + proc_w + (GUTTERS[0] + GUTTERS[1] + GUTTERS[2]) * k,
        ];
        let widths = [card_w, proc_w, card_w, card_w];
        let mut cards = Vec::new();
        let mut height = TOP;
        let mut column = |col: usize, specs: &mut dyn Iterator<Item = (CardId, CardSpec, bool)>| {
            let mut y = TOP;
            for (id, spec, options) in specs {
                let icon = matches!(id, CardId::Src(_));
                let h = card_height(spec.rows, options, spec.expanded, icon, spec.mute_row);
                cards.push(CardLayout {
                    id,
                    rect: Rect::from_min_size(origin + vec2(cols[col], y), vec2(widths[col], h)),
                    rows: spec.rows,
                    options,
                    expanded: spec.expanded,
                    icon,
                });
                y += h + GAP;
            }
            height = height.max(y);
        };
        column(
            0,
            &mut sources
                .iter()
                .enumerate()
                .map(|(i, s)| (CardId::Src(i), *s, true)),
        );
        column(
            2,
            &mut (0..out_pairs).map(|i| {
                (
                    CardId::Out(i),
                    CardSpec {
                        rows: 2,
                        expanded: false,
                        mute_row: false,
                    },
                    false,
                )
            }),
        );
        column(
            3,
            &mut monitors
                .iter()
                .enumerate()
                .map(|(i, s)| (CardId::Mon(i), *s, true)),
        );
        // Module cards stack down the Processing column.
        let mut y = TOP;
        for (i, s) in nodes.iter().enumerate() {
            let h = strip_height(*s);
            cards.push(CardLayout {
                id: CardId::Node(i),
                rect: Rect::from_min_size(origin + vec2(cols[1], y), vec2(proc_w, h)),
                rows: s.rows,
                options: false,
                expanded: false,
                icon: false,
            });
            y += h + GAP;
        }
        let proc_empty = nodes
            .is_empty()
            .then(|| Rect::from_min_size(origin + vec2(cols[1], TOP), vec2(proc_w, PROC_EMPTY_H)));
        if proc_empty.is_some() {
            y = TOP + PROC_EMPTY_H + GAP;
        }
        height = height.max(y);
        Layout {
            origin,
            cards,
            card_w,
            cols,
            proc_w,
            proc_empty,
            size: vec2(width, height + 8.0),
        }
    }

    pub fn card(&self, id: CardId) -> Option<&CardLayout> {
        self.cards.iter().find(|c| c.id == id)
    }

    pub fn row_y(c: &CardLayout, row: u16) -> f32 {
        let pad = (body_height(c.rows, c.icon) - ROW_H * f32::from(c.rows)) / 2.0;
        c.rect.top() + HEAD_H + pad + ROW_H * (f32::from(row) + 0.5)
    }

    pub fn port_pos(&self, p: Port) -> Option<Pos2> {
        match p {
            Port::Src { src, ch } => {
                let c = self.card(CardId::Src(src))?;
                (ch < c.rows).then(|| pos2(c.rect.right(), Self::row_y(c, ch)))
            }
            Port::OutIn { ch } | Port::OutOut { ch } => {
                let c = self.card(CardId::Out(usize::from(ch / 2)))?;
                let x = if p.is_output() {
                    c.rect.right()
                } else {
                    c.rect.left()
                };
                Some(pos2(x, Self::row_y(c, ch % 2)))
            }
            Port::Mon { mon, ch } => {
                let c = self.card(CardId::Mon(mon))?;
                (ch < c.rows).then(|| pos2(c.rect.left(), Self::row_y(c, ch)))
            }
            Port::NodeIn { node, ch } | Port::NodeOut { node, ch } => {
                let c = self.card(CardId::Node(node))?;
                let x = if p.is_output() {
                    c.rect.right()
                } else {
                    c.rect.left()
                };
                (ch < c.rows).then(|| pos2(x, Self::row_y(c, ch)))
            }
        }
    }

    /// Every port that has a position, in a stable order.
    pub fn ports(&self) -> Vec<Port> {
        let mut v = Vec::new();
        for c in &self.cards {
            for r in 0..c.rows {
                match c.id {
                    CardId::Src(src) => v.push(Port::Src { src, ch: r }),
                    CardId::Out(p) => {
                        let ch = p as u16 * 2 + r;
                        v.push(Port::OutIn { ch });
                        v.push(Port::OutOut { ch });
                    }
                    CardId::Mon(mon) => v.push(Port::Mon { mon, ch: r }),
                    CardId::Node(node) => {
                        v.push(Port::NodeIn { node, ch: r });
                        v.push(Port::NodeOut { node, ch: r });
                    }
                }
            }
        }
        v
    }

    /// Nearest port within `radius` of `pos`, optionally limited to a predicate.
    pub fn hit_port(&self, pos: Pos2, radius: f32, accept: impl Fn(Port) -> bool) -> Option<Port> {
        self.ports()
            .into_iter()
            .filter(|p| accept(*p))
            .filter_map(|p| Some((p, self.port_pos(p)?.distance(pos))))
            .filter(|(_, d)| *d <= radius)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
    }
}

/// Longest droop the control points get, in px.
pub const SAG_MAX: f32 = 36.0;

/// Cubic bezier control points of a cable: the handles run out of each end and gravity pushes
/// them down in proportion to the horizontal run (capped), so a long cable hangs lower.
pub fn wire_points(a: Pos2, b: Pos2) -> [Pos2; 4] {
    let run = (b.x - a.x).abs();
    // A short forward run (a source into a mixer beside it) bends within its own gap instead of
    // overshooting into the cards; longer and backward runs keep the 40 px handle.
    let dx = (run * 0.5).max(if b.x > a.x { run.min(40.0) } else { 40.0 });
    let sag = vec2(0.0, (run * 0.16).min(SAG_MAX));
    [a, a + vec2(dx, 0.0) + sag, b - vec2(dx, 0.0) + sag, b]
}

/// Midpoint of the curve (t = 0.5).
pub fn wire_mid(p: &[Pos2; 4]) -> Pos2 {
    pos2(
        (p[0].x + 3.0 * p[1].x + 3.0 * p[2].x + p[3].x) / 8.0,
        (p[0].y + 3.0 * p[1].y + 3.0 * p[2].y + p[3].y) / 8.0,
    )
}

/// Approximate distance from `pos` to the curve (polyline of 32 samples).
pub fn wire_distance(p: &[Pos2; 4], pos: Pos2) -> f32 {
    let curve = CubicBezierShape::from_points_stroke(*p, false, Color32::TRANSPARENT, Stroke::NONE);
    let pts: Vec<Pos2> = (0..=32).map(|i| curve.sample(i as f32 / 32.0)).collect();
    pts.windows(2)
        .map(|s| {
            let (a, b) = (s[0], s[1]);
            let ab = b - a;
            let t = if ab.length_sq() > 0.0 {
                ((pos - a).dot(ab) / ab.length_sq()).clamp(0.0, 1.0)
            } else {
                0.0
            };
            (a + ab * t).distance(pos)
        })
        .fold(f32::MAX, f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Layout {
        let s = CardSpec {
            rows: 2,
            expanded: false,
            mute_row: false,
        };
        Layout::compute(pos2(100.0, 50.0), 900.0, &[s, s], 1, &[s], &[])
    }

    #[test]
    fn columns_do_not_overlap_and_stack() {
        let l = sample();
        let a = l.card(CardId::Src(0)).unwrap().rect;
        let b = l.card(CardId::Src(1)).unwrap().rect;
        let o = l.card(CardId::Out(0)).unwrap().rect;
        let m = l.card(CardId::Mon(0)).unwrap().rect;
        assert!(a.right() < o.left() && o.right() < m.left());
        assert!((b.top() - a.bottom() - GAP).abs() < 1e-3);
        assert_eq!(a.left(), 100.0 + SIDE);
        assert!(l.size.y > b.bottom() - 50.0);
    }

    #[test]
    fn expanded_card_is_taller() {
        let c = CardSpec {
            rows: 2,
            expanded: true,
            mute_row: false,
        };
        let n = CardSpec {
            rows: 2,
            expanded: false,
            mute_row: false,
        };
        let l = Layout::compute(Pos2::ZERO, 900.0, &[c, n], 1, &[], &[]);
        let a = l.card(CardId::Src(0)).unwrap().rect;
        let b = l.card(CardId::Src(1)).unwrap().rect;
        assert_eq!(a.height() - b.height(), OPTIONS_H);
        assert!(b.top() >= a.bottom() + GAP - 1e-3);
    }

    #[test]
    fn port_positions_sit_on_card_edges() {
        let l = sample();
        let a = l.card(CardId::Src(0)).unwrap().rect;
        let p = l.port_pos(Port::Src { src: 0, ch: 1 }).unwrap();
        assert_eq!(p.x, a.right());
        assert_eq!(p.y, a.top() + HEAD_H + ROW_H * 1.5);
        let o = l.card(CardId::Out(0)).unwrap().rect;
        assert_eq!(l.port_pos(Port::OutIn { ch: 0 }).unwrap().x, o.left());
        assert_eq!(l.port_pos(Port::OutOut { ch: 1 }).unwrap().x, o.right());
        let m = l.card(CardId::Mon(0)).unwrap().rect;
        assert_eq!(l.port_pos(Port::Mon { mon: 0, ch: 0 }).unwrap().x, m.left());
        assert!(l.port_pos(Port::Src { src: 0, ch: 2 }).is_none());
        assert!(l.port_pos(Port::OutIn { ch: 2 }).is_none());
    }

    #[test]
    fn mono_source_rows_are_centered_beside_the_icon() {
        let l = Layout::compute(
            Pos2::ZERO,
            900.0,
            &[CardSpec {
                rows: 1,
                expanded: false,
                mute_row: false,
            }],
            1,
            &[],
            &[],
        );
        let c = l.card(CardId::Src(0)).unwrap();
        assert_eq!(c.rect.height(), HEAD_H + ICON_BODY + FOOT_H);
        assert_eq!(
            l.port_pos(Port::Src { src: 0, ch: 0 }).unwrap().y,
            c.rect.top() + HEAD_H + ICON_BODY / 2.0
        );
    }

    #[test]
    fn second_channel_pair_gets_its_own_card() {
        let s = CardSpec {
            rows: 2,
            expanded: false,
            mute_row: false,
        };
        let l = Layout::compute(Pos2::ZERO, 900.0, &[s], 2, &[], &[]);
        let p = l.port_pos(Port::OutIn { ch: 3 }).unwrap();
        assert_eq!(
            p.y,
            l.card(CardId::Out(1)).unwrap().rect.top() + HEAD_H + ROW_H * 1.5
        );
    }

    fn spec() -> CardSpec {
        CardSpec {
            rows: 2,
            expanded: false,
            mute_row: false,
        }
    }

    fn node() -> NodeSpec {
        NodeSpec {
            rows: 2,
            fx: 1,
            limiter: true,
            open: 0.0,
        }
    }

    /// Four columns left to right, each card inside its column and the pane.
    fn assert_columns(l: &Layout, width: f32) {
        let src = l.card(CardId::Src(0)).unwrap().rect;
        let out = l.card(CardId::Out(0)).unwrap().rect;
        let mon = l.card(CardId::Mon(0)).unwrap().rect;
        let proc = l.origin.x + l.cols[1]..l.origin.x + l.cols[1] + l.proc_w;
        assert!(src.right() < proc.start && proc.end < out.left() && out.right() < mon.left());
        assert!(mon.right() <= l.origin.x + width - SIDE + 0.5, "{mon:?}");
    }

    #[test]
    fn processing_column_is_always_there_at_the_minimum_window() {
        // 750 px: the graph pane of the 1000 px window; 48 + 3x150 + 120 + 40 + 40 + 52.
        let l = Layout::compute(Pos2::ZERO, 750.0, &[spec()], 1, &[spec()], &[]);
        assert_eq!(l.card_w, 150.0);
        assert_eq!(l.proc_w, PROC_W_MIN);
        assert_eq!(l.cols, [24.0, 214.0, 374.0, 576.0]);
        assert_columns(&l, 750.0);
        // Empty device: a placeholder fills the head of the column, no module cards.
        let e = l.proc_empty.unwrap();
        assert_eq!((e.left(), e.top(), e.width()), (214.0, TOP, PROC_W_MIN));
        assert!(l.cards.iter().all(|c| !matches!(c.id, CardId::Node(_))));
        assert!(l.size.y >= e.bottom());
    }

    #[test]
    fn module_cards_stack_in_the_processing_column() {
        let n = node();
        let l = Layout::compute(Pos2::ZERO, 750.0, &[spec()], 1, &[spec()], &[n, n]);
        assert_columns(&l, 750.0);
        assert!(l.proc_empty.is_none());
        let (a, b) = (
            l.card(CardId::Node(0)).unwrap().rect,
            l.card(CardId::Node(1)).unwrap().rect,
        );
        assert_eq!((a.left(), a.width()), (l.cols[1], l.proc_w));
        assert_eq!(a.top(), TOP);
        assert_eq!(a.height(), strip_height(n));
        assert!((b.top() - a.bottom() - GAP).abs() < 1e-3);
        assert_eq!(
            l.port_pos(Port::NodeIn { node: 1, ch: 1 }).unwrap().x,
            b.left()
        );
        assert_eq!(
            l.port_pos(Port::NodeOut { node: 0, ch: 0 }).unwrap().x,
            a.right()
        );
        assert!(l.port_pos(Port::NodeOut { node: 0, ch: 2 }).is_none());
        assert!(l.size.y >= b.bottom());
    }

    #[test]
    fn wide_window_spreads_cards_processing_and_gutters() {
        let l = Layout::compute(Pos2::ZERO, 1400.0, &[spec()], 1, &[spec()], &[node()]);
        assert_columns(&l, 1400.0);
        assert_eq!(l.card_w, 250.0);
        assert_eq!(l.proc_w, PROC_W_MAX);
        // Gutters keep the 40 : 40 : 52 proportion and all exceed their minimum.
        let g1 = l.cols[1] - (l.cols[0] + l.card_w);
        let g2 = l.cols[2] - (l.cols[1] + l.proc_w);
        let g3 = l.cols[3] - (l.cols[2] + l.card_w);
        assert!(g1 > 40.0 && (g1 - g2).abs() < 1e-3);
        assert!((g3 / g1 - 52.0 / 40.0).abs() < 1e-3);
        assert!((l.cols[3] + l.card_w - (1400.0 - SIDE)).abs() < 1e-3);
    }

    #[test]
    fn eight_channel_device_and_many_nodes_grow_the_height_only() {
        let n = node();
        let nodes = [n; 6];
        let l = Layout::compute(Pos2::ZERO, 750.0, &[spec()], 4, &[spec()], &nodes);
        assert_columns(&l, 750.0);
        let last_out = l.card(CardId::Out(3)).unwrap().rect;
        let last_node = l.card(CardId::Node(5)).unwrap().rect;
        assert!(last_out.left() > l.cols[1] + l.proc_w);
        assert_eq!(
            last_node.left(),
            l.card(CardId::Node(0)).unwrap().rect.left()
        );
        assert!(last_node.bottom() > last_out.bottom());
        assert!(l.size.y >= last_node.bottom());
        assert_eq!(
            l.port_pos(Port::OutIn { ch: 7 }).unwrap().x,
            last_out.left()
        );
    }

    #[test]
    fn hit_testing_picks_nearest_accepted_port() {
        let l = sample();
        let p = l.port_pos(Port::Src { src: 0, ch: 0 }).unwrap();
        assert_eq!(
            l.hit_port(p + vec2(3.0, 2.0), 9.0, |_| true),
            Some(Port::Src { src: 0, ch: 0 })
        );
        assert_eq!(l.hit_port(p + vec2(30.0, 0.0), 9.0, |_| true), None);
        assert_eq!(l.hit_port(p, 9.0, |p| !p.is_output()), None);
        let i = l.port_pos(Port::OutIn { ch: 1 }).unwrap();
        assert_eq!(
            l.hit_port(i, 9.0, |p| !p.is_output()),
            Some(Port::OutIn { ch: 1 })
        );
    }

    #[test]
    fn wire_passes_through_both_ends_and_sagged_midpoint() {
        let (a, b) = (pos2(0.0, 0.0), pos2(200.0, 100.0));
        let p = wire_points(a, b);
        for on_curve in [a, b, wire_mid(&p)] {
            assert!(wire_distance(&p, on_curve) < 1e-3, "{on_curve:?}");
        }
        // gravity: the middle hangs below the straight line between the ends
        assert!(wire_mid(&p).y > 50.0);
    }

    #[test]
    fn sag_grows_with_the_run_and_is_capped() {
        let droop = |run: f32| wire_points(pos2(0.0, 0.0), pos2(run, 0.0))[1].y;
        assert!(droop(50.0) < droop(150.0));
        assert_eq!(droop(1000.0), SAG_MAX);
        assert_eq!(droop(5000.0), SAG_MAX);
        assert_eq!(droop(0.0), 0.0);
    }

    #[test]
    fn sag_is_finite_deterministic_and_mirror_symmetric() {
        let (a, b) = (pos2(12.5, 80.0), pos2(310.0, -40.0));
        let p = wire_points(a, b);
        assert_eq!(p, wire_points(a, b));
        assert!(p.iter().all(|q| q.x.is_finite() && q.y.is_finite()));
        // mirrored left to right, the cable is the same curve walked backwards
        let mirror = |q: Pos2| pos2(-q.x, q.y);
        let m = wire_points(mirror(b), mirror(a));
        for i in 0..4 {
            assert_eq!(m[i], mirror(p[3 - i]));
        }
    }

    #[test]
    fn backwards_wire_keeps_minimum_handle() {
        let p = wire_points(pos2(100.0, 0.0), pos2(50.0, 0.0));
        assert_eq!(p[1].x - p[0].x, 40.0);
    }

    #[test]
    fn wire_distance_is_small_on_curve() {
        let p = wire_points(pos2(0.0, 0.0), pos2(200.0, 100.0));
        assert!(wire_distance(&p, wire_mid(&p)) < 0.5);
        assert!(wire_distance(&p, pos2(100.0, 250.0)) > 50.0);
    }
}
