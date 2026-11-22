//! Small painted controls: switch, meter, slider knob, icons.
use crate::meter::FLOOR_DB;
use crate::theme::Tokens;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Painter, Pos2, Rect, Response, Sense, Slider,
    Stroke, TextStyle, Ui, UiBuilder, WidgetInfo, accesskit::Role, pos2, vec2,
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

/// Segments in an LED ladder: one per 3 dB from -57 to 0 dBFS.
pub const LED_SEGMENTS: usize = 20;
const LED_STEP_DB: f32 = 3.0;

/// dBFS at which segment `i` (0 = quietest) lights.
pub fn led_threshold_db(i: usize) -> f32 {
    FLOOR_DB + LED_STEP_DB * (i as f32 + 1.0)
}

/// How many segments light for a 0..=1 bar position (`meter::level`, -60..0 dBFS).
pub fn led_lit(level: f32) -> usize {
    let db = FLOOR_DB - FLOOR_DB * level.clamp(0.0, 1.0);
    // The epsilon absorbs the round trip through `level`'s log and its inverse.
    (0..LED_SEGMENTS)
        .take_while(|&i| db + 1e-3 >= led_threshold_db(i))
        .count()
}

/// Green up to -12 dBFS, amber up to -3, red above.
pub fn led_color(i: usize, t: &Tokens) -> Color32 {
    match led_threshold_db(i) {
        db if db <= -12.0 => t.ok,
        db if db <= -3.0 => t.warn,
        _ => t.bad,
    }
}

/// LED ladder in `r`: the segments up to `level` light, the segment at `peak` holds its light.
pub fn meter(p: &Painter, r: Rect, level: f32, peak: f32, t: &Tokens, dim: f32) {
    let n = LED_SEGMENTS as f32;
    let gap = (r.width() / n * 0.3).clamp(1.0, 2.0);
    let w = (r.width() - gap * (n - 1.0)) / n;
    let round = CornerRadius::same((w.min(r.height()) / 3.0).round().max(1.0) as u8);
    let lit = led_lit(level);
    let held = if peak > 0.0 {
        led_lit(peak).checked_sub(1)
    } else {
        None
    };
    for i in 0..LED_SEGMENTS {
        let seg = Rect::from_min_size(
            pos2(r.left() + i as f32 * (w + gap), r.top()),
            vec2(w, r.height()),
        );
        let color = if i < lit || held == Some(i) {
            led_color(i, t).gamma_multiply(dim)
        } else {
            t.track
        };
        p.rect_filled(seg, round, color);
    }
}

/// Opaque darker shade of a cable colour, for outlines and holes.
pub fn darken(c: Color32) -> Color32 {
    c.lerp_to_gamma(Color32::BLACK, 0.45)
}

/// Radius of a jack at rest.
pub const JACK_R: f32 = 6.0;

/// How a jack looks.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Plug {
    /// No cable: an empty socket, its ring in a channel colour or neutral grey (`None`).
    Open(Option<Color32>),
    /// One cable: solid in that cable's colour.
    One(Color32),
    /// Two or more cables: the first three destination colours as equal arcs, and the cable count.
    Split([Color32; 3], usize),
}

impl Plug {
    /// The look of a jack that `n` cables leave, the first three ending in `first` (in order).
    pub fn of(first: [Color32; 3], n: usize) -> Plug {
        match n {
            0 => Plug::Open(None),
            1 => Plug::One(first[0]),
            _ => Plug::Split(first, n),
        }
    }
}

impl Plug {
    /// The same look with every colour passed through `f`.
    pub fn map(self, f: impl Fn(Color32) -> Color32) -> Plug {
        match self {
            Plug::Open(c) => Plug::Open(c.map(f)),
            Plug::One(c) => Plug::One(f(c)),
            Plug::Split(cols, n) => Plug::Split(cols.map(f), n),
        }
    }

    /// An open ring as the solid plug of its colour (the preview of a drop); others unchanged.
    pub fn solid(self) -> Plug {
        match self {
            Plug::Open(Some(c)) => Plug::One(c),
            other => other,
        }
    }
}

/// Arc `i` of `k` equal arcs of a ring, from the top, clockwise: a polyline.
fn arc(at: Pos2, r: f32, i: usize, k: usize) -> Vec<Pos2> {
    const STEPS: usize = 10;
    let turn = std::f32::consts::TAU / k as f32;
    let a0 = -std::f32::consts::FRAC_PI_2 + turn * i as f32;
    (0..=STEPS)
        .map(|s| {
            let a = a0 + turn * s as f32 / STEPS as f32;
            at + vec2(a.cos(), a.sin()) * r
        })
        .collect()
}

/// A round jack. `scale` grows it for the plug-in pop; `hot` lights it (hovered cable or jack).
/// Wired jacks are solid with a heavier ring and a soft halo, open ones a thin ring on the card.
pub fn jack(p: &Painter, at: Pos2, plug: Plug, scale: f32, hot: bool, t: &Tokens) {
    let r = JACK_R * scale;
    let hole = |k: f32| p.circle_filled(at, r * k, Color32::from_black_alpha(170));
    match plug {
        Plug::One(c) => {
            p.circle_filled(at, r * 1.6, c.gamma_multiply(if hot { 0.45 } else { 0.18 }));
            p.circle(
                at,
                r,
                c,
                Stroke::new(if hot { 2.5 } else { 2.0 }, darken(c)),
            );
            p.circle_stroke(
                at,
                r * 0.6,
                Stroke::new(1.0, c.lerp_to_gamma(Color32::WHITE, 0.55)),
            );
            hole(0.28);
        }
        Plug::Open(ring) => {
            let c = ring.unwrap_or(t.muted);
            if hot {
                p.circle_filled(at, r * 1.6, c.gamma_multiply(0.3));
            }
            p.circle(at, r, t.raised, Stroke::new(if hot { 2.5 } else { 1.5 }, c));
            p.circle_filled(at, r * 0.34, Color32::from_black_alpha(110));
        }
        Plug::Split(cols, n) => {
            // Larger than a plain jack: a ring in several colours needs the room to read.
            let (k, r) = (n.min(3), r * 1.3);
            p.circle_filled(at, r, t.raised);
            for (i, c) in cols.iter().take(k).enumerate() {
                p.add(egui::Shape::line(
                    arc(at, r - 1.75, i, k),
                    Stroke::new(if hot { 4.5 } else { 3.5 }, *c),
                ));
            }
            hole(0.39);
            if n > 3 {
                p.text(
                    at + vec2(r + 3.0, -r - 1.0),
                    Align2::LEFT_BOTTOM,
                    format!("+{}", n - 3),
                    crate::theme::caption(),
                    t.ink,
                );
            }
        }
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

/// Volume slider (percent, 0..=100) filling `rect`, with the 14 px knob; the caller names it.
pub fn fader(ui: &mut Ui, rect: Rect, pct: &mut f32, t: &Tokens) -> Response {
    let resp = ui
        .scope_builder(UiBuilder::new().max_rect(rect), |ui| {
            // egui sizes a slider from the body text height and `interact_size`; shrink both so the
            // knob painted over its handle can be the 14 px one.
            let style = ui.style_mut();
            style.spacing.interact_size.y = rect.height();
            if let Some(body) = style.text_styles.get_mut(&TextStyle::Body) {
                body.size = 8.0;
            }
            ui.spacing_mut().slider_width = rect.width() - 4.0;
            ui.add(Slider::new(pct, 0.0..=100.0).show_value(false))
        })
        .inner;
    slider_knob(ui.painter(), resp.rect, *pct / 100.0, t);
    resp
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meter::level;
    use crate::theme::tokens;

    fn lin(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    #[test]
    fn ladder_lights_one_segment_per_three_db() {
        assert_eq!(led_lit(0.0), 0);
        assert_eq!(led_lit(level(lin(-58.0))), 0);
        assert_eq!(led_lit(level(lin(-57.0))), 1);
        assert_eq!(led_lit(level(lin(-30.0))), 10);
        assert_eq!(led_lit(level(lin(0.0))), LED_SEGMENTS);
        assert_eq!(led_lit(1.5), LED_SEGMENTS);
        assert_eq!(led_lit(f32::NAN), 0);
    }

    #[test]
    fn a_jack_is_open_solid_or_split_by_its_cable_count() {
        let (a, b, c) = (Color32::RED, Color32::GREEN, Color32::BLUE);
        assert_eq!(Plug::of([a, b, c], 0), Plug::Open(None));
        assert_eq!(Plug::of([a, b, c], 1), Plug::One(a));
        assert_eq!(Plug::of([a, b, c], 2), Plug::Split([a, b, c], 2));
        assert_eq!(Plug::of([a, b, c], 9), Plug::Split([a, b, c], 9));
    }

    #[test]
    fn a_ring_splits_into_equal_arcs_that_meet() {
        let at = pos2(10.0, 10.0);
        for k in [2, 3] {
            let arcs: Vec<_> = (0..k).map(|i| arc(at, 5.0, i, k)).collect();
            for (i, a) in arcs.iter().enumerate() {
                let next = &arcs[(i + 1) % k];
                assert!(a.last().unwrap().distance(next[0]) < 1e-3);
                assert!(a.iter().all(|q| (q.distance(at) - 5.0).abs() < 1e-3));
            }
        }
    }

    #[test]
    fn ladder_is_monotonic() {
        let mut last = 0;
        for i in 0..=600 {
            let n = led_lit(i as f32 / 600.0);
            assert!(n >= last);
            last = n;
        }
    }

    #[test]
    fn ladder_colours_switch_at_minus_12_and_minus_3_dbfs() {
        let t = tokens(true);
        let colours: Vec<Color32> = (0..LED_SEGMENTS).map(|i| led_color(i, &t)).collect();
        // the -12 dBFS segment is the last green, the -3 dBFS one the last amber
        let at = |db: f32| {
            (0..LED_SEGMENTS)
                .find(|&i| led_threshold_db(i) == db)
                .unwrap()
        };
        assert_eq!(colours[at(-12.0)], t.ok);
        assert_eq!(colours[at(-9.0)], t.warn);
        assert_eq!(colours[at(-3.0)], t.warn);
        assert_eq!(colours[at(0.0)], t.bad);
        assert_eq!(led_threshold_db(LED_SEGMENTS - 1), 0.0);
    }
}
