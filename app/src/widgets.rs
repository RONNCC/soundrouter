//! Small painted controls: switch, meter, slider knob, icons.
use crate::theme::Tokens;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Painter, Pos2, Rect, Response, Sense, Stroke,
    Ui, WidgetInfo, accesskit::Role, pos2, vec2,
};

/// Names a painted button so screen readers (and tests) can find it.
pub fn name_button(resp: &Response, label: &str) {
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
}

/// Names a widget that reports its own info (slider, text field) without a label.
pub fn name_widget(ui: &Ui, resp: &Response, label: &str) {
    ui.ctx()
        .accesskit_node_builder(resp.id, |n| n.set_label(label));
}

pub fn switch(ui: &mut Ui, rect: Rect, id: Id, on: bool, t: &Tokens, label: &str) -> bool {
    let resp = ui.interact(rect, id, Sense::click());
    resp.widget_info(|| WidgetInfo::selected(Role::CheckBox, true, on, label));
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
    p.circle_filled(pos2(x, rect.center().y), knob, Color32::WHITE);
    new
}

/// Continuous bar: 0..=1 `level`, 2 px peak tick at `peak`. Colour turns amber from 70 % and red from 90 %.
pub fn meter(p: &Painter, r: Rect, level: f32, peak: f32, t: &Tokens, dim: f32) {
    let color = |pos: f32| {
        if pos > 0.9 {
            t.bad
        } else if pos > 0.7 {
            t.warn
        } else {
            t.ok
        }
        .gamma_multiply(dim)
    };
    let round = CornerRadius::same((r.height() / 2.0) as u8);
    p.rect_filled(r, round, t.track);
    let w = r.width() * level.clamp(0.0, 1.0);
    if w > 0.5 {
        let fill = Rect::from_min_size(r.min, vec2(w.max(r.height()), r.height()));
        p.rect_filled(fill, round, color(level));
    }
    if peak > 0.0 {
        let x =
            (r.left() + r.width() * peak.clamp(0.0, 1.0)).clamp(r.left() + 1.0, r.right() - 1.0);
        p.rect_filled(
            Rect::from_x_y_ranges((x - 1.0)..=(x + 1.0), r.y_range()),
            CornerRadius::ZERO,
            color(peak),
        );
    }
}

/// White knob with a hairline edge, painted over egui's grey slider handle.
/// `r` is the slider's response rect, `frac` the value in 0..=1.
pub fn slider_knob(p: &Painter, r: Rect, frac: f32, t: &Tokens) {
    let rad = r.height() / 2.5; // egui's handle radius
    let c = pos2(
        egui::lerp((r.left() + rad)..=(r.right() - rad), frac.clamp(0.0, 1.0)),
        r.center().y,
    );
    p.circle(c, rad + 1.0, Color32::WHITE, Stroke::new(1.0, t.hairline));
}

/// The ⊕ / ⊖ mark, without any interaction.
pub fn plus_glyph(p: &Painter, center: Pos2, c: Color32, minus: bool) {
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
}

/// Small down-triangle beside a ⊕ that opens a menu.
pub fn menu_hint(p: &Painter, center: Pos2, c: Color32) {
    p.add(egui::Shape::convex_polygon(
        vec![
            center + vec2(-3.0, -1.5),
            center + vec2(3.0, -1.5),
            center + vec2(0.0, 2.0),
        ],
        c,
        Stroke::NONE,
    ));
}

pub fn plus_circle(
    ui: &mut Ui,
    center: Pos2,
    id: Id,
    t: &Tokens,
    minus: bool,
    label: &str,
) -> Response {
    let rect = Rect::from_center_size(center, vec2(22.0, 22.0));
    let resp = ui.interact(rect, id, Sense::click());
    name_button(&resp, label);
    let c = if resp.hovered() { t.ink } else { t.muted };
    plus_glyph(ui.painter(), center, c, minus);
    resp
}

/// Small × (remove) glyph, ~8 px across.
pub fn xmark(p: &Painter, center: Pos2, c: Color32) {
    let s = Stroke::new(1.4, c);
    p.line_segment([center + vec2(-4.0, -4.0), center + vec2(4.0, 4.0)], s);
    p.line_segment([center + vec2(-4.0, 4.0), center + vec2(4.0, -4.0)], s);
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

pub fn speaker(
    ui: &mut Ui,
    center: Pos2,
    id: Id,
    muted: bool,
    t: &Tokens,
    label: &str,
) -> Response {
    let rect = Rect::from_center_size(center, vec2(22.0, 22.0));
    let resp = ui.interact(rect, id, Sense::click());
    resp.widget_info(|| WidgetInfo::selected(Role::CheckBox, true, muted, label));
    let c = if muted {
        t.bad_ink
    } else if resp.hovered() {
        t.ink
    } else {
        t.muted
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
