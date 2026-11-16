//! Small painted controls: switch, segmented meter, icons.
use crate::theme::Tokens;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Painter, Pos2, Rect, Response, Sense, Stroke,
    Ui, pos2, vec2,
};

pub fn switch(ui: &mut Ui, rect: Rect, id: Id, on: bool, t: &Tokens) -> bool {
    let resp = ui.interact(rect, id, Sense::click());
    let new = if resp.clicked() { !on } else { on };
    let k = ui.ctx().animate_bool_responsive(id, new);
    let p = ui.painter();
    let r = rect.height() / 2.0;
    p.rect_filled(
        rect,
        CornerRadius::same(r as u8),
        t.switch_off.lerp_to_gamma(t.accent, k),
    );
    let knob = r - 2.0;
    let x = egui::lerp((rect.left() + knob + 2.0)..=(rect.right() - knob - 2.0), k);
    p.circle_filled(
        pos2(x, rect.center().y + 0.5),
        knob + 0.5,
        Color32::from_black_alpha(40),
    );
    p.circle_filled(pos2(x, rect.center().y), knob, Color32::WHITE);
    new
}

/// Segmented bar: 0..=1 `level`, peak marker at `peak`. Segments turn yellow from 70 % and red from 90 %.
pub fn meter(p: &Painter, r: Rect, level: f32, peak: f32, t: &Tokens, dim: f32) {
    const SEG: f32 = 5.0;
    const GAP: f32 = 2.0;
    let n = ((r.width() + GAP) / (SEG + GAP)).floor().max(1.0) as usize;
    let step = r.width() / n as f32;
    let color = |pos: f32| {
        t.meter[if pos > 0.9 {
            2
        } else if pos > 0.7 {
            1
        } else {
            0
        }]
    };
    let peak_seg = ((peak * n as f32).ceil() as usize).clamp(1, n) - 1;
    for i in 0..n {
        let pos = (i as f32 + 1.0) / n as f32;
        let x = r.left() + i as f32 * step;
        let seg = Rect::from_min_size(pos2(x, r.top()), vec2(step - GAP, r.height()));
        let lit = level >= pos - 0.5 / n as f32 || (peak > 0.0 && i == peak_seg);
        let c = if lit {
            color(pos).gamma_multiply(dim)
        } else {
            t.track
        };
        p.rect_filled(seg, CornerRadius::same(1), c);
    }
}

pub fn plus_circle(ui: &mut Ui, center: Pos2, id: Id, t: &Tokens, minus: bool) -> Response {
    let rect = Rect::from_center_size(center, vec2(22.0, 22.0));
    let resp = ui.interact(rect, id, Sense::click());
    let c = if resp.hovered() { t.text } else { t.text2 };
    let p = ui.painter();
    p.circle_stroke(center, 8.0, Stroke::new(1.3, c));
    p.line_segment(
        [center - vec2(4.0, 0.0), center + vec2(4.0, 0.0)],
        Stroke::new(1.5, c),
    );
    if !minus {
        p.line_segment(
            [center - vec2(0.0, 4.0), center + vec2(0.0, 4.0)],
            Stroke::new(1.5, c),
        );
    }
    resp
}

pub fn chevron(p: &Painter, center: Pos2, open: bool, color: Color32) {
    let s = Stroke::new(1.4, color);
    let (a, b, c) = if open {
        (vec2(-3.5, -1.5), vec2(0.0, 2.0), vec2(3.5, -1.5))
    } else {
        (vec2(-1.5, -3.5), vec2(2.0, 0.0), vec2(-1.5, 3.5))
    };
    p.line_segment([center + a, center + b], s);
    p.line_segment([center + b, center + c], s);
}

pub fn speaker(ui: &mut Ui, center: Pos2, id: Id, muted: bool, t: &Tokens) -> Response {
    let rect = Rect::from_center_size(center, vec2(22.0, 22.0));
    let resp = ui.interact(rect, id, Sense::click());
    let c = if muted {
        t.danger
    } else if resp.hovered() {
        t.text
    } else {
        t.text2
    };
    let p = ui.painter();
    let s = Stroke::new(1.4, c);
    let x = center.x - 6.0;
    let cone = vec![
        pos2(x, center.y - 2.5),
        pos2(x + 3.0, center.y - 2.5),
        pos2(x + 7.0, center.y - 5.5),
        pos2(x + 7.0, center.y + 5.5),
        pos2(x + 3.0, center.y + 2.5),
        pos2(x, center.y + 2.5),
    ];
    p.add(egui::Shape::convex_polygon(cone, c, Stroke::NONE));
    if muted {
        p.line_segment(
            [
                pos2(x + 9.0, center.y - 3.0),
                pos2(x + 13.0, center.y + 3.0),
            ],
            s,
        );
        p.line_segment(
            [
                pos2(x + 13.0, center.y - 3.0),
                pos2(x + 9.0, center.y + 3.0),
            ],
            s,
        );
    } else {
        for (r, a) in [(3.5f32, 0.9f32), (6.5, 1.0)] {
            let pts: Vec<Pos2> = (0..=8)
                .map(|i| {
                    let th = (i as f32 / 8.0 - 0.5) * a * 1.6;
                    pos2(x + 8.0 + r * th.cos(), center.y + r * th.sin())
                })
                .collect();
            p.add(egui::Shape::line(pts, s));
        }
    }
    resp
}

pub fn pencil(p: &Painter, center: Pos2, color: Color32) {
    let s = Stroke::new(1.3, color);
    let (a, b) = (center + vec2(-4.5, 4.5), center + vec2(3.5, -3.5));
    p.line_segment([a, b], Stroke::new(3.0, color));
    p.line_segment([a + vec2(-0.5, 1.0), a + vec2(-1.0, 1.0)], s);
}

/// Rounded-square tile with a centred glyph.
pub fn tile(p: &Painter, rect: Rect, tint: Color32, glyph: &str, size: f32, alpha: f32) {
    p.rect_filled(
        rect,
        CornerRadius::same((rect.width() * 0.24) as u8),
        tint.gamma_multiply(alpha),
    );
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        FontId::proportional(size),
        Color32::WHITE.gamma_multiply(alpha),
    );
}

/// Two interlocking loops, the Pass-Thru / virtual device mark.
pub fn loops(p: &Painter, rect: Rect, color: Color32) {
    let r = rect.height() * 0.26;
    let w = Stroke::new(rect.height() * 0.09, color);
    for dx in [-0.17, 0.17] {
        p.circle_stroke(rect.center() + vec2(rect.width() * dx, 0.0), r, w);
    }
}
