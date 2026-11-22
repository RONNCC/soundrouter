//! Design tokens, embedded fonts and the egui style built from them.
//! Light is "Mixer Mist", dark is "Studio Night"; docs/DESIGN.md records the plan and its reasons.
use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, vec2,
};
use std::sync::Arc;

pub const fn hex(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Cable colours, one per source in order: Signal Blue, Cable Magenta, Tangerine, Mint Tape,
/// Violet, Cyan, Sun, Coral. Saturated enough to read on both themes.
pub const CABLES: [Color32; 8] = [
    hex(0x3B6CFF),
    hex(0xE8458B),
    hex(0xFF8A1F),
    hex(0x19B98A),
    hex(0x8B5CF6),
    hex(0x12AEDB),
    hex(0xF2C21B),
    hex(0xFF5C5C),
];

pub fn cable_color(index: usize) -> Color32 {
    CABLES[index % CABLES.len()]
}

/// Corner radii: deliberately not one value for everything. Jacks and dots are circles.
pub mod radius {
    /// Channel strips (cards) and sheets.
    pub const STRIP: u8 = 14;
    pub const SHEET: u8 = 14;
    /// Menus and the checklist group.
    pub const MENU: u8 = 12;
    pub const ROW: u8 = 12;
    pub const INPUT: u8 = 8;
    /// Buttons and pills: egui clamps a radius to half the height, so this is a stadium.
    pub const PILL: u8 = u8::MAX;
}

#[derive(Clone, Copy)]
pub struct Tokens {
    /// Window background behind everything.
    pub canvas: Color32,
    /// Sidebar and footer.
    pub panel: Color32,
    /// Cards, sheets, buttons: one step up from the canvas.
    pub raised: Color32,
    pub ink: Color32,
    pub muted: Color32,
    /// 1 px structure lines.
    pub hairline: Color32,
    /// Selected sidebar row.
    pub selected: Color32,
    /// Primary fill under white text (4.5:1); Signal Blue nudged darker where needed.
    pub accent: Color32,
    /// Accent as text on the canvas, panel or a card.
    pub accent_ink: Color32,
    /// Keyboard focus ring: 2 px Signal Blue.
    pub focus: Color32,
    /// Status signal colours: meters, dots, glyph tiles.
    pub ok: Color32,
    pub warn: Color32,
    pub bad: Color32,
    /// Status colours as text (4.5:1 on canvas, panel and cards).
    pub ok_ink: Color32,
    pub warn_ink: Color32,
    pub bad_ink: Color32,
    /// Destructive fill under white text.
    pub danger: Color32,
    /// Meter and progress rails.
    pub track: Color32,
    /// The one soft shadow, for floating layers only.
    pub shadow: Color32,
    /// Dims the window behind a modal sheet.
    pub scrim: Color32,
    pub switch_off: Color32,
}

pub fn tokens(dark: bool) -> Tokens {
    let signal = CABLES[0];
    if dark {
        Tokens {
            canvas: hex(0x11131B),
            panel: hex(0x181B26),
            raised: hex(0x202433),
            ink: hex(0xECEEF6),
            muted: hex(0x8F95AB),
            hairline: hex(0x2C3144),
            selected: hex(0x1F2848),
            accent: hex(0x3366FF),
            accent_ink: hex(0x7C9BFF),
            focus: signal,
            ok: hex(0x19B98A),
            warn: hex(0xE89A0C),
            bad: hex(0xE5484D),
            ok_ink: hex(0x19B98A),
            warn_ink: hex(0xE89A0C),
            bad_ink: hex(0xF0696D),
            danger: hex(0xD7262C),
            track: hex(0x0C0E14),
            shadow: Color32::from_black_alpha(110),
            scrim: Color32::from_black_alpha(110),
            switch_off: hex(0x3A4056),
        }
    } else {
        Tokens {
            canvas: hex(0xEEF1F7),
            panel: hex(0xF8F9FC),
            raised: hex(0xFFFFFF),
            ink: hex(0x151826),
            muted: hex(0x5C6278),
            hairline: hex(0xD9DDE8),
            selected: hex(0xE1E8FC),
            accent: hex(0x2D5BEF),
            accent_ink: hex(0x2554E6),
            focus: signal,
            ok: hex(0x19B98A),
            warn: hex(0xE89A0C),
            bad: hex(0xE5484D),
            ok_ink: hex(0x0E7657),
            warn_ink: hex(0x8C5C06),
            bad_ink: hex(0xC8242A),
            danger: hex(0xD7262C),
            track: hex(0xE2E6EF),
            shadow: Color32::from_rgba_unmultiplied(21, 24, 38, 40),
            scrim: Color32::from_rgba_unmultiplied(21, 24, 38, 70),
            switch_off: hex(0xC5CBD9),
        }
    }
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn medium() -> FontFamily {
    FontFamily::Name("Medium".into())
}

/// Bricolage Grotesque Bold at a display optical size.
pub fn title_family() -> FontFamily {
    FontFamily::Name("Title".into())
}

/// Type scale: 11 caption, 13 body, 15 strong body, 20 section title, 26 device title.
pub fn caption() -> FontId {
    FontId::proportional(11.0)
}

pub fn body() -> FontId {
    FontId::proportional(13.0)
}

/// Body-size label for buttons, chips and menu options.
pub fn label() -> FontId {
    FontId::new(13.0, semibold())
}

pub fn strong() -> FontId {
    FontId::new(15.0, medium())
}

pub fn section() -> FontId {
    FontId::new(20.0, title_family())
}

pub fn device_title() -> FontId {
    FontId::new(26.0, title_family())
}

/// JetBrains Mono for numbers: dB, ms, percentages, channel numbers.
pub fn num(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("Num".into()))
}

static BRICOLAGE: &[u8] = include_bytes!("../assets/fonts/BricolageGrotesque.ttf");
static JETBRAINS: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono.ttf");

/// Bricolage Grotesque (UI, titles) and JetBrains Mono (numbers), both embedded and OFL.
/// Both are variable fonts: each family is one instance picked through its axes.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // egui's own fonts stay as fallbacks for symbols and emoji.
    let fallback = fonts.families[&FontFamily::Proportional].clone();
    // (face, file, optical size, width, weight). The semibold label face runs a little narrow, so
    // card titles and device names keep their room; JetBrains Mono ignores the axes it lacks.
    let faces = [
        ("bricolage", BRICOLAGE, 12.0, 100.0, 400.0),
        ("bricolage-medium", BRICOLAGE, 14.0, 100.0, 500.0),
        ("bricolage-semibold", BRICOLAGE, 12.0, 90.0, 600.0),
        ("bricolage-title", BRICOLAGE, 28.0, 100.0, 700.0),
        ("jetbrains", JETBRAINS, 12.0, 100.0, 450.0),
    ];
    for (name, bytes, opsz, wdth, wght) in faces {
        let tweak = egui::FontTweak {
            coords: egui::epaint::text::VariationCoords::new([
                ("opsz", opsz),
                ("wdth", wdth),
                ("wght", wght),
            ]),
            ..Default::default()
        };
        fonts.font_data.insert(
            name.into(),
            Arc::new(egui::FontData::from_static(bytes).tweak(tweak)),
        );
    }
    let with_fallback = |face: &str| {
        let mut v = vec![face.to_string()];
        v.extend(fallback.iter().cloned());
        v
    };
    for (family, face) in [
        (FontFamily::Proportional, "bricolage"),
        (medium(), "bricolage-medium"),
        (semibold(), "bricolage-semibold"),
        (title_family(), "bricolage-title"),
        (FontFamily::Monospace, "jetbrains"),
        (FontFamily::Name("Num".into()), "jetbrains"),
    ] {
        fonts.families.insert(family, with_fallback(face));
    }
    ctx.set_fonts(fonts);
}

/// The one shadow style, for floating layers only (menus, tooltips, sheets).
pub fn soft_shadow(t: &Tokens) -> Shadow {
    Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: t.shadow,
    }
}

/// egui's own animation time: how long a switch knob slides and a sheet fades in. Kept, so only
/// "Reduce motion" changes it.
const ANIMATION_SECS: f32 = 0.2;

/// Maps the tokens onto egui's style so stock widgets (menus, checkboxes, sliders, tooltips,
/// scroll bars, text fields) match the painted ones. `reduce_motion` (the macOS setting) makes
/// egui's animations instant: the switch knob jumps and sheets appear without a fade.
pub fn apply_style(s: &mut egui::Style, t: &Tokens, reduce_motion: bool) {
    s.animation_time = if reduce_motion { 0.0 } else { ANIMATION_SECS };
    s.text_styles = [
        (TextStyle::Small, caption()),
        (TextStyle::Body, body()),
        (TextStyle::Button, body()),
        (TextStyle::Heading, section()),
        (TextStyle::Monospace, num(12.0)),
    ]
    .into();
    s.interaction.selectable_labels = false;
    s.spacing.interact_size.y = 28.0;
    s.spacing.button_padding = vec2(12.0, 5.0);
    s.spacing.menu_margin = egui::Margin::same(6);
    s.spacing.slider_rail_height = 4.0;

    let v = &mut s.visuals;
    v.override_text_color = Some(t.ink);
    v.weak_text_color = Some(t.muted);
    v.hyperlink_color = t.accent_ink;
    v.warn_fg_color = t.warn_ink;
    v.error_fg_color = t.bad_ink;
    v.panel_fill = t.canvas;
    v.faint_bg_color = t.panel;
    v.extreme_bg_color = t.canvas;
    v.text_edit_bg_color = Some(t.raised);
    v.code_bg_color = t.panel;
    v.window_fill = t.raised;
    v.window_stroke = Stroke::new(1.0, t.hairline);
    v.window_corner_radius = CornerRadius::same(radius::SHEET);
    v.menu_corner_radius = CornerRadius::same(radius::MENU);
    v.window_shadow = soft_shadow(t);
    v.popup_shadow = soft_shadow(t);
    // White text on the accent: selected text, selected buttons.
    v.selection.bg_fill = t.accent;
    v.selection.stroke = Stroke::new(2.0, Color32::WHITE);
    v.handle_shape = egui::style::HandleShape::Circle;
    v.slider_trailing_fill = true;
    v.text_cursor.stroke = Stroke::new(2.0, t.focus);

    let round = CornerRadius::same(radius::INPUT);
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = t.panel;
    w.noninteractive.weak_bg_fill = t.panel;
    w.noninteractive.bg_stroke = Stroke::new(1.0, t.hairline);
    w.noninteractive.fg_stroke = Stroke::new(1.0, t.muted);
    w.noninteractive.corner_radius = round;
    // bg_fill is a slider's rail and a checkbox's box; weak_bg_fill a button's face.
    w.inactive.bg_fill = t.switch_off;
    w.inactive.weak_bg_fill = t.raised;
    w.inactive.bg_stroke = Stroke::new(1.0, t.hairline);
    w.inactive.fg_stroke = Stroke::new(1.0, t.ink);
    w.inactive.corner_radius = round;
    w.inactive.expansion = 0.0;
    w.hovered.bg_fill = t.switch_off;
    w.hovered.weak_bg_fill = t.hairline;
    w.hovered.bg_stroke = Stroke::new(1.0, t.muted);
    w.hovered.fg_stroke = Stroke::new(1.5, t.ink);
    w.hovered.corner_radius = round;
    w.hovered.expansion = 0.0;
    // Active is also the keyboard-focus look: the 2 px focus ring.
    w.active.bg_fill = t.switch_off;
    w.active.weak_bg_fill = t.hairline;
    w.active.bg_stroke = Stroke::new(2.0, t.focus);
    w.active.fg_stroke = Stroke::new(1.5, t.ink);
    w.active.corner_radius = round;
    w.active.expansion = 0.0;
    w.open = w.hovered;
}

/// Tint for an app/device tile, stable per name.
pub fn tile_tint(name: &str) -> Color32 {
    let h = name
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    cable_color(h as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lin(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let l = |c: Color32| 0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b());
        let (x, y) = (l(a), l(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn tint_is_stable() {
        assert_eq!(tile_tint("Safari"), tile_tint("Safari"));
        assert_eq!(hex(0x0A84FF), Color32::from_rgb(10, 132, 255));
        assert_eq!(cable_color(8), cable_color(0));
    }

    /// Text tokens keep 4.5:1 on every surface they sit on, in both themes.
    #[test]
    fn text_contrast() {
        for dark in [false, true] {
            let t = tokens(dark);
            for bg in [t.canvas, t.panel, t.raised, t.selected] {
                for fg in [
                    t.ink,
                    t.muted,
                    t.accent_ink,
                    t.ok_ink,
                    t.warn_ink,
                    t.bad_ink,
                ] {
                    let c = contrast(fg, bg);
                    assert!(c >= 4.5, "dark={dark} {fg:?} on {bg:?}: {c:.2}");
                }
            }
            for fill in [t.accent, t.danger] {
                let c = contrast(Color32::WHITE, fill);
                assert!(c >= 4.5, "dark={dark} white on {fill:?}: {c:.2}");
            }
            // The focus ring is a non-text indicator: 3:1.
            assert!(contrast(t.focus, t.canvas) >= 3.0);
        }
    }

    #[test]
    fn reduce_motion_turns_every_egui_animation_off_and_only_that() {
        let t = tokens(false);
        let mut s = egui::Style::default();
        apply_style(&mut s, &t, false);
        assert_eq!(s.animation_time, ANIMATION_SECS);
        assert!(s.animation_time > 0.0);
        apply_style(&mut s, &t, true);
        assert_eq!(s.animation_time, 0.0);
        apply_style(&mut s, &t, false); // switched off again in System Settings
        assert_eq!(s.animation_time, ANIMATION_SECS);
        // The rest of the style does not depend on it. `b` is cloned from `a`: two separate
        // `Style::default()`s never compare equal (their number formatters are compared by pointer).
        let mut a = egui::Style::default();
        apply_style(&mut a, &t, false);
        let mut b = a.clone();
        apply_style(&mut b, &t, true);
        b.animation_time = a.animation_time;
        assert_eq!(a, b);
    }

    #[test]
    fn the_motion_time_is_eguis_own() {
        assert_eq!(ANIMATION_SECS, egui::Style::default().animation_time);
    }
}
