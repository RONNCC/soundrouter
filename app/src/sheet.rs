//! The one modal sheet every pop-up uses: dimmed window, centred rounded card, icon + title + intro,
//! scrolling body, and a button row with the default (primary) action on the right.
use crate::theme::{self, Tokens, radius};
use crate::widgets::{loops, name_button};
use eframe::egui::{
    self, Align, Align2, Color32, Context, CornerRadius, Frame, Id, Key, Label, Layout, Margin,
    Modal, Modifiers, Rect, RichText, ScrollArea, Sense, Spinner, Stroke, StrokeKind, Ui,
    WidgetInfo, accesskit::Role, pos2, vec2,
};

/// Card content width; the card is this plus its padding.
const INNER_W: f32 = 420.0;
const PAD: f32 = 24.0;
/// Space kept free around the card, so it never touches the window edge.
const MARGIN: f32 = 16.0;
const ICON: f32 = 48.0;
const BUTTON_H: f32 = 28.0;
const GAP: f32 = 18.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    /// The app: SoundRouter's loops on the accent colour.
    App,
    Ok,
    Warn,
    Fail,
}

pub struct Sheet<'a> {
    /// Stable per sheet, so egui remembers its size between frames.
    pub id: &'static str,
    pub glyph: Glyph,
    pub title: &'a str,
    /// Secondary text under the title; may be empty.
    pub intro: &'a str,
    /// Whether the sheet has a button row (false while it only waits).
    pub buttons: bool,
}

/// Shows `sheet` over the window. The backdrop swallows clicks and never dismisses the sheet.
/// `content` scrolls when the window is too small; `footer` stays pinned at the bottom.
pub fn modal<S>(
    ctx: &Context,
    t: &Tokens,
    sheet: &Sheet,
    state: &mut S,
    content: impl FnOnce(&mut Ui, &mut S),
    footer: impl FnOnce(&mut Ui, &mut S),
) {
    let frame = Frame::new()
        .fill(t.raised)
        .stroke(Stroke::new(1.0, t.hairline))
        .corner_radius(CornerRadius::same(radius::SHEET))
        .inner_margin(Margin::same(PAD as i8))
        .shadow(theme::soft_shadow(t));
    let screen = ctx.content_rect();
    let inner_w = INNER_W
        .min(screen.width() - 2.0 * (MARGIN + PAD))
        .max(200.0);
    let max_inner_h = screen.height() - 2.0 * (MARGIN + PAD);
    Modal::new(Id::unique(sheet.id))
        .backdrop_color(t.scrim)
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_width(inner_w);
            ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
            header(ui, t, sheet, inner_w);
            let used = ui.min_rect().height();
            let footer_h = if sheet.buttons { GAP + BUTTON_H } else { 0.0 };
            let budget = (max_inner_h - used - GAP - footer_h).max(60.0);
            ui.add_space(GAP - 8.0);
            ScrollArea::vertical()
                .max_height(budget)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.set_width(inner_w);
                    content(ui, state);
                });
            if sheet.buttons {
                ui.add_space(GAP - 8.0);
                ui.allocate_ui_with_layout(
                    vec2(inner_w, BUTTON_H),
                    Layout::left_to_right(Align::Center),
                    |ui| footer(ui, state),
                );
            }
        });
}

fn header(ui: &mut Ui, t: &Tokens, sheet: &Sheet, inner_w: f32) {
    let text_w = inner_w - ICON - 16.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 16.0;
        let (r, _) = ui.allocate_exact_size(vec2(ICON, ICON), Sense::hover());
        icon_tile(ui, r, sheet.glyph, t);
        ui.vertical(|ui| {
            ui.set_width(text_w);
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add_space(2.0);
            ui.add(
                Label::new(
                    RichText::new(sheet.title)
                        .font(theme::section())
                        .color(t.ink),
                )
                .wrap(),
            );
            if !sheet.intro.is_empty() {
                body(ui, t, sheet.intro);
            }
        });
    });
}

/// Secondary-colour wrapped paragraph.
pub fn body(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add(
        Label::new(
            RichText::new(text)
                .size(13.0)
                .line_height(Some(18.0))
                .color(t.muted),
        )
        .wrap(),
    );
}

/// Same, in the error colour.
pub fn error(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add(
        Label::new(
            RichText::new(text)
                .size(13.0)
                .line_height(Some(18.0))
                .color(t.bad_ink),
        )
        .wrap(),
    );
}

fn icon_tile(ui: &Ui, r: Rect, glyph: Glyph, t: &Tokens) {
    let p = ui.painter();
    let tint = match glyph {
        Glyph::App => t.accent,
        Glyph::Ok => t.ok,
        Glyph::Warn => t.warn,
        Glyph::Fail => t.bad,
    };
    p.rect_filled(r, CornerRadius::same(12), tint);
    let (c, s) = (r.center(), Stroke::new(3.2, Color32::WHITE));
    match glyph {
        Glyph::App => loops(p, r, Color32::WHITE),
        Glyph::Ok => {
            p.line_segment([c + vec2(-9.0, 0.5), c + vec2(-3.0, 7.0)], s);
            p.line_segment([c + vec2(-3.0, 7.0), c + vec2(10.0, -7.0)], s);
        }
        Glyph::Warn => {
            p.line_segment([c + vec2(0.0, -10.0), c + vec2(0.0, 3.0)], s);
            p.circle_filled(c + vec2(0.0, 9.0), 2.2, Color32::WHITE);
        }
        Glyph::Fail => {
            p.line_segment([c + vec2(-8.0, -8.0), c + vec2(8.0, 8.0)], s);
            p.line_segment([c + vec2(-8.0, 8.0), c + vec2(8.0, -8.0)], s);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    /// Not decided yet (not asked, not run).
    Pending,
    Running,
    Fail,
}

#[derive(Clone)]
pub struct Row<'a> {
    pub state: Status,
    pub title: &'a str,
    /// Right-aligned, in the state's colour; read by screen readers as "title: status".
    pub status: &'a str,
    /// Wrapped under the title; may be empty.
    pub detail: &'a str,
    /// A small button under the detail.
    pub action: Option<&'a str>,
}

/// Checklist in an inset group. Returns the index of the row whose action was clicked.
pub fn checklist(ui: &mut Ui, t: &Tokens, rows: &[Row]) -> Option<usize> {
    let mut clicked = None;
    let w = ui.available_width();
    // An inset group by tone alone: no outline, so the rows read as lines, not as boxes.
    Frame::new()
        .fill(t.canvas)
        .corner_radius(CornerRadius::same(radius::ROW))
        .inner_margin(Margin::symmetric(14, 0))
        .show(ui, |ui| {
            ui.set_width(w - 28.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, r) in rows.iter().enumerate() {
                if i > 0 {
                    let (line, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
                    ui.painter().rect_filled(line, 0.0, t.hairline);
                }
                if row(ui, t, r) {
                    clicked = Some(i);
                }
            }
        });
    clicked
}

fn row(ui: &mut Ui, t: &Tokens, r: &Row) -> bool {
    let mut clicked = false;
    let w = ui.available_width();
    // Dot and chip tint use the signal colour; the chip's text uses its 4.5:1 ink.
    let (color, ink, spin) = match r.state {
        Status::Ok => (t.ok, t.ok_ink, false),
        Status::Pending => (t.warn, t.warn_ink, false),
        Status::Running => (t.accent, t.accent_ink, true),
        Status::Fail => (t.bad, t.bad_ink, false),
    };
    ui.add_space(12.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        let (g, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
        if spin {
            ui.put(g, Spinner::new().size(16.0).color(color));
        } else {
            status_dot(ui, g.center(), r.state, color);
        }
        ui.vertical(|ui| {
            ui.set_width(w - 30.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            let (line, resp) = ui.allocate_exact_size(vec2(w - 30.0, 22.0), Sense::hover());
            let label = format!("{}: {}", r.title, r.status);
            resp.widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
            let p = ui.painter();
            p.text(
                line.left_center(),
                Align2::LEFT_CENTER,
                r.title,
                theme::strong(),
                t.ink,
            );
            let g = p.layout_no_wrap(r.status.to_string(), theme::label(), ink);
            let chip = Rect::from_center_size(
                pos2(line.right() - (g.size().x + 16.0) / 2.0, line.center().y),
                g.size() + vec2(16.0, 6.0),
            );
            p.rect_filled(
                chip,
                CornerRadius::same(radius::PILL),
                color.gamma_multiply(0.16),
            );
            p.galley(chip.center() - g.size() / 2.0, g, ink);
            if !r.detail.is_empty() {
                ui.add(
                    Label::new(
                        RichText::new(r.detail)
                            .size(13.0)
                            .line_height(Some(18.0))
                            .color(t.muted),
                    )
                    .wrap(),
                );
            }
            if let Some(a) = r.action {
                ui.add_space(4.0);
                // Secondary: only the footer's default button is accented and answers Return.
                clicked = button(ui, t, a, Kind::Secondary);
            }
        });
    });
    ui.add_space(12.0);
    clicked
}

fn status_dot(ui: &Ui, c: egui::Pos2, state: Status, color: Color32) {
    let p = ui.painter();
    p.circle_filled(c, 8.0, color);
    let s = Stroke::new(1.8, Color32::WHITE);
    match state {
        Status::Ok => {
            p.line_segment([c + vec2(-3.6, 0.2), c + vec2(-1.2, 2.8)], s);
            p.line_segment([c + vec2(-1.2, 2.8), c + vec2(3.8, -2.8)], s);
        }
        Status::Fail => {
            p.line_segment([c + vec2(-3.0, -3.0), c + vec2(3.0, 3.0)], s);
            p.line_segment([c + vec2(-3.0, 3.0), c + vec2(3.0, -3.0)], s);
        }
        _ => {
            p.circle_filled(c, 2.6, Color32::WHITE);
        }
    }
}

/// Indeterminate thin bar with a caption, for work of unknown length.
pub fn progress(ui: &mut Ui, t: &Tokens, label: &str) {
    ui.add(Label::new(RichText::new(label).size(13.0).color(t.ink)).wrap());
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 4.0), Sense::hover());
    let p = ui.painter();
    let round = CornerRadius::same(2);
    p.rect_filled(r, round, t.track);
    let phase = (ui.input(|i| i.time) * 0.7).fract() as f32;
    let seg = r.width() * 0.3;
    let x = egui::lerp((r.left() - seg)..=r.right(), phase);
    let bar = Rect::from_x_y_ranges(x.max(r.left())..=(x + seg).min(r.right()), r.y_range());
    p.rect_filled(bar, round, t.accent);
    ui.request_repaint();
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The default action: accent fill, answers Return.
    Primary,
    /// A destructive default action: red fill, never answers Return.
    Danger,
    Secondary,
}

pub fn button(ui: &mut Ui, t: &Tokens, label: &str, kind: Kind) -> bool {
    let font = theme::label();
    let text = if kind == Kind::Secondary {
        t.ink
    } else {
        Color32::WHITE
    };
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, text);
    let size = vec2((galley.size().x + 28.0).max(72.0), BUTTON_H);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    name_button(&resp, label);
    let base = match kind {
        Kind::Primary => t.accent,
        Kind::Danger => t.danger,
        Kind::Secondary => t.raised.lerp_to_gamma(t.hairline, 0.5),
    };
    // Hover and press darken, so white text keeps its 4.5:1 on the accent and danger fills.
    let fill = if resp.is_pointer_button_down_on() {
        base.lerp_to_gamma(Color32::BLACK, 0.18)
    } else if resp.hovered() {
        base.lerp_to_gamma(Color32::BLACK, 0.08)
    } else {
        base
    };
    let stroke = if kind == Kind::Secondary {
        Stroke::new(1.0, t.hairline)
    } else {
        Stroke::NONE
    };
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(radius::PILL),
        fill,
        stroke,
        StrokeKind::Inside,
    );
    if resp.has_focus() {
        p.rect_stroke(
            rect.expand(2.0),
            CornerRadius::same(radius::PILL),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    p.galley(
        pos2(
            rect.center().x - galley.size().x / 2.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        text,
    );
    // Return answers the default button unless a control has the keyboard.
    let enter = kind == Kind::Primary
        && ui.memory(|m| m.focused().is_none())
        && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
    resp.clicked() || enter
}

/// Button row: `left` is left-aligned; `right` is laid out right to left, so add the primary button
/// first and the Cancel/Close button after it.
pub fn buttons<S>(
    ui: &mut Ui,
    state: &mut S,
    left: impl FnOnce(&mut Ui, &mut S),
    right: impl FnOnce(&mut Ui, &mut S),
) {
    ui.spacing_mut().item_spacing.x = 8.0;
    left(ui, state);
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| right(ui, state));
}
