//! Geometry of the three-column graph: card rects, port positions, hit-testing, wire curves.
use crate::model::Port;
use eframe::egui::{Color32, Pos2, Rect, Stroke, Vec2, epaint::CubicBezierShape, pos2, vec2};

pub const HEAD_H: f32 = 34.0;
/// Minimum body height of a card with a big icon.
pub const ICON_BODY: f32 = 48.0;
pub const ROW_H: f32 = 24.0;
pub const FOOT_H: f32 = 28.0;
pub const OPTIONS_H: f32 = 58.0;
pub const OUT_PAD: f32 = 10.0;
pub const GAP: f32 = 16.0;
/// Space above the first card for the column headers.
pub const TOP: f32 = 54.0;
pub const SIDE: f32 = 24.0;
pub const PORT_R: f32 = 5.5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CardId {
    Src(usize),
    /// Channel pair index (0 = channels 1 & 2).
    Out(usize),
    Mon(usize),
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
}

#[derive(Debug)]
pub struct Layout {
    pub origin: Pos2,
    pub cards: Vec<CardLayout>,
    pub card_w: f32,
    /// Left edge of the Sources, Output Channels and Monitors columns.
    pub col_x: [f32; 3],
    pub size: Vec2,
}

pub fn body_height(rows: u16, icon: bool) -> f32 {
    let rows = ROW_H * f32::from(rows);
    if icon { rows.max(ICON_BODY) } else { rows }
}

pub fn card_height(rows: u16, options: bool, expanded: bool, icon: bool) -> f32 {
    let body = HEAD_H + body_height(rows, icon);
    if !options {
        body + OUT_PAD
    } else if expanded {
        body + FOOT_H + OPTIONS_H
    } else {
        body + FOOT_H
    }
}

impl Layout {
    /// `width` is the pane width; `origin` the top-left of the graph area.
    pub fn compute(
        origin: Pos2,
        width: f32,
        sources: &[CardSpec],
        out_pairs: usize,
        monitors: &[CardSpec],
    ) -> Layout {
        let card_w = ((width - 2.0 * SIDE - 2.0 * 64.0) / 3.0).clamp(180.0, 250.0);
        let col_x = [
            SIDE,
            ((width - card_w) / 2.0).max(SIDE + card_w + 40.0),
            (width - SIDE - card_w).max(SIDE + 2.0 * card_w + 80.0),
        ];
        let mut cards = Vec::new();
        let mut height = TOP;
        let mut column = |col: usize, specs: &mut dyn Iterator<Item = (CardId, CardSpec, bool)>| {
            let mut y = TOP;
            for (id, spec, options) in specs {
                let icon = matches!(id, CardId::Src(_));
                let h = card_height(spec.rows, options, spec.expanded, icon);
                cards.push(CardLayout {
                    id,
                    rect: Rect::from_min_size(origin + vec2(col_x[col], y), vec2(card_w, h)),
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
            1,
            &mut (0..out_pairs).map(|i| {
                (
                    CardId::Out(i),
                    CardSpec {
                        rows: 2,
                        expanded: false,
                    },
                    false,
                )
            }),
        );
        column(
            2,
            &mut monitors
                .iter()
                .enumerate()
                .map(|(i, s)| (CardId::Mon(i), *s, true)),
        );
        Layout {
            origin,
            cards,
            card_w,
            col_x,
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

/// Cubic bezier control points: control handles run horizontally out of each end.
pub fn wire_points(a: Pos2, b: Pos2) -> [Pos2; 4] {
    let dx = ((b.x - a.x) * 0.5).max(40.0);
    [a, a + vec2(dx, 0.0), b - vec2(dx, 0.0), b]
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
        };
        Layout::compute(pos2(100.0, 50.0), 900.0, &[s, s], 1, &[s])
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
        };
        let n = CardSpec {
            rows: 2,
            expanded: false,
        };
        let l = Layout::compute(Pos2::ZERO, 900.0, &[c, n], 1, &[]);
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
            }],
            1,
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
        };
        let l = Layout::compute(Pos2::ZERO, 900.0, &[s], 2, &[]);
        let p = l.port_pos(Port::OutIn { ch: 3 }).unwrap();
        assert_eq!(
            p.y,
            l.card(CardId::Out(1)).unwrap().rect.top() + HEAD_H + ROW_H * 1.5
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
    fn wire_passes_through_both_ends_and_midpoint() {
        let (a, b) = (pos2(0.0, 0.0), pos2(200.0, 100.0));
        let p = wire_points(a, b);
        for on_curve in [a, b, pos2(100.0, 50.0)] {
            assert!(wire_distance(&p, on_curve) < 1e-3, "{on_curve:?}");
        }
        // handles stay horizontal
        assert_eq!(p[1].y, a.y);
        assert_eq!(p[2].y, b.y);
    }

    #[test]
    fn backwards_wire_keeps_minimum_handle() {
        let p = wire_points(pos2(100.0, 0.0), pos2(50.0, 0.0));
        assert_eq!(p[1].x - p[0].x, 40.0);
    }

    #[test]
    fn wire_distance_is_small_on_curve() {
        let p = wire_points(pos2(0.0, 0.0), pos2(200.0, 100.0));
        assert!(wire_distance(&p, pos2(100.0, 50.0)) < 0.5);
        assert!(wire_distance(&p, pos2(100.0, 150.0)) > 50.0);
    }
}
