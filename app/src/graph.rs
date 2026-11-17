//! The three-column graph (Sources | Output Channels | Monitors): painting and interaction.
//! Reads the config, emits `Edit`s; never mutates it.
use crate::icons::{self, IconTextures};
use crate::layout::{
    CardId, CardLayout, CardSpec, FOOT_H, HEAD_H, Layout, MUTE_H, PORT_R, TOP, body_height,
    wire_distance, wire_points,
};
use crate::meter::{PeakHold, level};
use crate::model::{Edit, Link, Port, can_link, ch_label, links};
use crate::theme::{Tokens, semibold, tile_tint};
use crate::widgets::{
    chevron, loops, menu_hint, meter, name_button, name_widget, plus_circle, slider_knob, switch,
    tile,
};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Popup, PopupCloseBehavior, Rect, Response,
    Sense, Shadow, Slider, Stroke, StrokeKind, TextStyle, Ui, UiBuilder, WidgetInfo,
    epaint::CubicBezierShape, pos2, text::LayoutJob, vec2,
};
use sr_core::backend::{AppInfo, HwDevice, MeterReading, TccStatus};
use sr_core::config::{Monitor, Source, SourceKind, VirtualDevice};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

const MAX_ROWS: u16 = 32;

pub struct GraphCtx<'a> {
    pub dev_idx: usize,
    pub dev: &'a VirtualDevice,
    pub hw: &'a [HwDevice],
    pub apps: &'a [AppInfo],
    /// App icon textures by source key; keys without one fall back to the letter tile.
    pub icons: &'a IconTextures,
    /// Apps installed in /Applications etc., empty until the background scan finishes.
    pub installed: &'a [AppInfo],
    pub reading: Option<&'a MeterReading>,
    pub t: &'a Tokens,
    /// Microphone permission, which Audio Input sources need.
    pub mic: TccStatus,
    /// Minimum height of the graph area (so it fills the pane).
    pub min_h: f32,
}

#[derive(Default)]
pub struct GraphState {
    pub expanded: HashSet<String>,
    pub peaks: HashMap<(CardId, u16), PeakHold>,
    pub drag: Option<Port>,
    /// Filled once by a background scan (started on the first frame), see `sr_audio::apps::installed_apps`.
    pub installed: Arc<OnceLock<Vec<AppInfo>>>,
    pub scan_started: bool,
    /// Sources popup: search text, whether it takes focus on the next open, and whether
    /// daemons/agents are listed.
    pub filter: String,
    pub focus_search: bool,
    pub show_background: bool,
}

fn mon_rows(m: &Monitor, hw: &[HwDevice]) -> u16 {
    match hw.iter().find(|h| h.uid == m.device_uid) {
        Some(h) => h.out_ch.clamp(1, MAX_ROWS),
        None => (m.wires.iter().map(|w| w.dst_ch + 1).max().unwrap_or(0)).clamp(2, MAX_ROWS),
    }
}

/// `name` when known, else `fallback`: what an absent app or device is called.
fn last_seen(name: &str, fallback: &str) -> String {
    if name.is_empty() { fallback } else { name }.to_string()
}

fn source_title(g: &GraphCtx, s: &Source) -> (String, &'static str) {
    let (hw, apps) = (g.hw, g.apps);
    match &s.kind {
        SourceKind::App { bundle_id } => (
            apps.iter()
                .chain(g.installed)
                .find(|a| &a.bundle_id == bundle_id)
                .map_or_else(
                    || last_seen(&s.name, bundle_id.rsplit('.').next().unwrap_or(bundle_id)),
                    |a| a.name.clone(),
                ),
            "Application",
        ),
        SourceKind::Device { uid } => (
            hw.iter()
                .find(|h| &h.uid == uid)
                .map_or_else(|| last_seen(&s.name, uid), |h| h.name.clone()),
            "Input device",
        ),
        SourceKind::PassThru => ("Pass-Thru".to_string(), "Apps playing to this device"),
    }
}

fn monitor_title(m: &Monitor, hw: &[HwDevice]) -> String {
    hw.iter()
        .find(|h| h.uid == m.device_uid)
        .map_or_else(|| last_seen(&m.name, &m.device_uid), |h| h.name.clone())
}

fn sources_summary(dev: &VirtualDevice) -> String {
    let apps = dev
        .sources
        .iter()
        .filter(|s| matches!(s.kind, SourceKind::App { .. }))
        .count();
    let devs = dev
        .sources
        .iter()
        .filter(|s| matches!(s.kind, SourceKind::Device { .. }))
        .count();
    let mut parts = Vec::new();
    if apps > 0 {
        parts.push(format!("{apps} App{}", if apps == 1 { "" } else { "s" }));
    }
    if devs > 0 {
        parts.push(format!("{devs} Device{}", if devs == 1 { "" } else { "s" }));
    }
    if dev.sources.iter().any(|s| s.kind == SourceKind::PassThru) {
        parts.push("Pass-Thru".into());
    }
    if parts.is_empty() {
        "No Sources".into()
    } else {
        parts.join(", ")
    }
}

/// Single-line text that ends in an ellipsis instead of overflowing `max_w`.
fn fitted(
    ui: &Ui,
    text: &str,
    font: FontId,
    color: Color32,
    max_w: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.painter().layout_job(job)
}

pub fn text_left(ui: &Ui, x: f32, cy: f32, text: &str, font: FontId, color: Color32, max_w: f32) {
    let g = fitted(ui, text, font, color, max_w);
    ui.painter()
        .galley(pos2(x, cy - g.size().y / 2.0), g, color);
}

pub fn show(ui: &mut Ui, g: &GraphCtx, st: &mut GraphState, edits: &mut Vec<Edit>) -> Layout {
    let dev = g.dev;
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    // Tests seed `st.installed` themselves; a real scan would race with them.
    if !cfg!(test) && !std::mem::replace(&mut st.scan_started, true) {
        let (list, ctx) = (st.installed.clone(), ui.ctx().clone());
        std::thread::spawn(move || {
            let _ = list.set(sr_audio::apps::installed_apps());
            ctx.request_repaint();
        });
    }
    let key = |s: &str| format!("{}/{s}", dev.id);
    let src_specs: Vec<CardSpec> = dev
        .sources
        .iter()
        .map(|s| CardSpec {
            rows: s.channels.clamp(1, MAX_ROWS),
            expanded: st.expanded.contains(&key(&s.id)),
            mute_row: matches!(s.kind, SourceKind::App { .. }),
        })
        .collect();
    let mon_specs: Vec<CardSpec> = dev
        .monitors
        .iter()
        .map(|m| CardSpec {
            rows: mon_rows(m, g.hw),
            expanded: st.expanded.contains(&key(&format!("m:{}", m.device_uid))),
            mute_row: false,
        })
        .collect();
    let origin = ui.cursor().min;
    let width = ui.available_width();
    let layout = Layout::compute(
        origin,
        width,
        &src_specs,
        usize::from(dev.channels / 2),
        &mon_specs,
    );
    ui.allocate_exact_size(vec2(width, layout.size.y.max(g.min_h)), Sense::hover());

    column_headers(ui, g, st, &layout, edits);
    for c in &layout.cards {
        card(ui, g, st, edits, c, dt, &key);
    }
    wires_and_ports(ui, g, st, edits, &layout);
    layout
}

fn column_headers(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    layout: &Layout,
    edits: &mut Vec<Edit>,
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let base = layout.origin.y + TOP;
    let heads: [(&str, String); 3] = [
        ("Sources", sources_summary(dev)),
        ("Output Channels", format!("{} Channels", dev.channels)),
        (
            "Monitors",
            format!(
                "{} Device{}",
                dev.monitors.len(),
                if dev.monitors.len() == 1 { "" } else { "s" }
            ),
        ),
    ];
    for (i, (title, sub)) in heads.iter().enumerate() {
        let x = layout.origin.x + layout.col_x[i];
        text_left(
            ui,
            x + 2.0,
            base - 38.0,
            title,
            FontId::new(14.0, semibold()),
            t.text,
            layout.card_w - 50.0,
        );
        text_left(
            ui,
            x + 2.0,
            base - 20.0,
            sub,
            FontId::proportional(11.0),
            t.text2,
            layout.card_w - 50.0,
        );
        let right = x + layout.card_w;
        let id = Id::unique(("hdr", d, i));
        match i {
            0 => {
                let resp = plus_circle(
                    ui,
                    pos2(right - 20.0, base - 28.0),
                    id,
                    t,
                    false,
                    "Add source",
                );
                menu_hint(
                    ui.painter(),
                    pos2(right - 4.0, base - 28.0),
                    hint_color(&resp, t),
                );
                let shown = Popup::menu(&resp)
                    .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| source_menu(ui, g, st, edits));
                if shown.is_none() {
                    st.filter.clear();
                    st.focus_search = true;
                }
            }
            1 => {
                let resp = plus_circle(
                    ui,
                    pos2(right - 12.0, base - 28.0),
                    id,
                    t,
                    false,
                    "Add channel pair",
                );
                if resp.clicked() {
                    edits.push(Edit::GrowChannels(d));
                }
                resp.on_hover_text("Add a channel pair");
                if dev.channels > 2 {
                    let resp = plus_circle(
                        ui,
                        pos2(right - 38.0, base - 28.0),
                        id.with("minus"),
                        t,
                        true,
                        "Remove channel pair",
                    );
                    if resp.clicked() {
                        edits.push(Edit::ShrinkChannels(d));
                    }
                    resp.on_hover_text("Remove the last channel pair");
                }
            }
            _ => {
                let resp = plus_circle(
                    ui,
                    pos2(right - 20.0, base - 28.0),
                    id,
                    t,
                    false,
                    "Add monitor",
                );
                menu_hint(
                    ui.painter(),
                    pos2(right - 4.0, base - 28.0),
                    hint_color(&resp, t),
                );
                Popup::menu(&resp).show(|ui| monitor_menu(ui, g, edits));
            }
        }
    }
}

fn hint_color(resp: &Response, t: &Tokens) -> Color32 {
    if resp.hovered() { t.text } else { t.text2 }
}

/// One menu line: icon, label, accent highlight on hover. Returns true when clicked.
fn menu_row(
    ui: &mut Ui,
    t: &Tokens,
    enabled: bool,
    label: &str,
    detail: &str,
    icon: impl FnOnce(&egui::Painter, Rect),
) -> bool {
    const ICON: f32 = 18.0;
    let font = TextStyle::Button.resolve(ui.style());
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), sense);
    resp.widget_info(|| WidgetInfo::labeled(egui::Role::Button, enabled, label));
    let hot = enabled && resp.hovered();
    let p = ui.painter();
    if hot {
        p.rect_filled(rect, CornerRadius::same(4), t.accent);
    }
    let color = match (enabled, hot) {
        (_, true) => Color32::WHITE,
        (true, _) => t.text,
        _ => t.text2.gamma_multiply(0.6),
    };
    let icon_rect = Rect::from_center_size(
        pos2(rect.left() + 8.0 + ICON / 2.0, rect.center().y),
        vec2(ICON, ICON),
    );
    icon(p, icon_rect);
    let mut text_w = rect.width() - ICON - 22.0;
    if !detail.is_empty() {
        let d = p.layout_no_wrap(
            detail.to_string(),
            FontId::proportional(11.0),
            Color32::PLACEHOLDER,
        );
        text_w -= d.size().x + 8.0;
        let dc = if hot { color } else { t.text2 };
        p.galley(
            pos2(
                rect.right() - 8.0 - d.size().x,
                rect.center().y - d.size().y / 2.0,
            ),
            d,
            dc,
        );
    }
    let galley = fitted(ui, label, font, color, text_w);
    ui.painter().galley(
        pos2(
            rect.left() + ICON + 14.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        color,
    );
    enabled && resp.clicked()
}

/// The app's real icon, else its coloured letter tile.
fn app_icon(p: &egui::Painter, r: Rect, icons: &IconTextures, key: &str, name: &str, alpha: f32) {
    if let Some(tex) = icons.get(key) {
        let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        p.image(tex.id(), r, full, Color32::WHITE.gamma_multiply(alpha));
    } else {
        let glyph = name
            .chars()
            .next()
            .map_or("?".into(), |c| c.to_uppercase().to_string());
        tile(p, r, tile_tint(name), &glyph, r.height() * 0.53, alpha);
    }
}

/// Accent-tinted square with a glyph, the look of Pass-Thru and hardware devices.
fn glyph_tile(
    p: &egui::Painter,
    r: Rect,
    t: &Tokens,
    alpha: f32,
    glyph: impl FnOnce(&egui::Painter, Rect, Color32),
) {
    p.rect_filled(
        r,
        CornerRadius::same((r.width() * 0.24) as u8),
        t.accent.gamma_multiply(0.18 * alpha),
    );
    glyph(p, r, t.accent.gamma_multiply(alpha));
}

const MENU_W: f32 = 280.0;
/// Room kept under the scrolling list for the footer rows (Pass-Thru, Choose App, the toggle).
const MENU_FOOT_H: f32 = 110.0;
const NOT_CONNECTED: &str = "Not connected";

/// A device source's problem, shown on its card: the hardware is absent, or macOS denied the microphone.
fn source_note(g: &GraphCtx, s: &Source) -> Option<&'static str> {
    let SourceKind::Device { uid } = &s.kind else {
        return None;
    };
    if !g.hw.iter().any(|h| &h.uid == uid) {
        Some(NOT_CONNECTED)
    } else if g.mic == TccStatus::Denied {
        Some("Microphone denied")
    } else {
        None
    }
}

/// What the Sources popup offers for a search text.
pub struct Picks<'a> {
    /// Input devices (never our own virtual devices, which would feed back into themselves).
    pub inputs: Vec<&'a HwDevice>,
    /// Apps playing audio first, then regular GUI apps, each group sorted by name.
    pub apps: Vec<&'a AppInfo>,
    /// Daemons, agents and helpers that are not playing; hidden unless asked for.
    pub background: Vec<&'a AppInfo>,
}

pub fn picks<'a>(hw: &'a [HwDevice], apps: &'a [AppInfo], query: &str) -> Picks<'a> {
    let q = query.trim().to_lowercase();
    let hit = |name: &str| q.is_empty() || name.to_lowercase().contains(&q);
    let inputs = hw
        .iter()
        .filter(|h| h.in_ch > 0 && !sr_audio::engine::is_own_uid(&h.uid) && hit(&h.name))
        .collect();
    let (mut apps, mut background): (Vec<&AppInfo>, Vec<&AppInfo>) = apps
        .iter()
        .filter(|a| hit(&a.name))
        .partition(|a| a.running_output || a.regular);
    apps.sort_by_cached_key(|a| (!a.running_output, a.name.to_lowercase()));
    background.sort_by_cached_key(|a| a.name.to_lowercase());
    Picks {
        inputs,
        apps,
        background,
    }
}

fn app_row(ui: &mut Ui, g: &GraphCtx, a: &AppInfo, edits: &mut Vec<Edit>) {
    let kind = SourceKind::App {
        bundle_id: a.bundle_id.clone(),
    };
    let exists = g.dev.sources.iter().any(|s| s.kind == kind);
    let label = if a.running_output {
        format!("{}  (playing)", a.name)
    } else {
        a.name.clone()
    };
    let alpha = if exists { 0.5 } else { 1.0 };
    let paint = |p: &egui::Painter, r: Rect| app_icon(p, r, g.icons, &a.bundle_id, &a.name, alpha);
    if menu_row(ui, g.t, !exists, &label, "", paint) {
        edits.push(Edit::AddSource(g.dev_idx, kind, 2));
        ui.close();
    }
}

/// The Sources popup: search field, then a scrolling list (Audio Inputs, Applications, optionally
/// Background processes) capped to the window, then Pass-Thru, Choose App… and the background toggle.
fn source_menu(ui: &mut Ui, g: &GraphCtx, st: &mut GraphState, edits: &mut Vec<Edit>) {
    let (dev, d, t) = (g.dev, g.dev_idx, g.t);
    ui.set_width(MENU_W);
    let search = ui.add(
        egui::TextEdit::singleline(&mut st.filter)
            .hint_text("Search")
            .desired_width(f32::INFINITY),
    );
    name_widget(ui, &search, "Search sources");
    if std::mem::take(&mut st.focus_search) {
        search.request_focus();
    }
    let searching = !st.filter.trim().is_empty();
    let p = picks(g.hw, g.apps, &st.filter);
    let room = ui.ctx().content_rect().bottom() - ui.cursor().top() - MENU_FOOT_H;
    let heading = |ui: &mut Ui, text: &str| {
        ui.label(egui::RichText::new(text).size(11.0).color(t.text2));
    };
    egui::ScrollArea::vertical()
        .max_height(room.clamp(96.0, 420.0))
        .show(ui, |ui| {
            if !p.inputs.is_empty() || !searching {
                heading(ui, "Audio Inputs");
            }
            for h in &p.inputs {
                let kind = SourceKind::Device { uid: h.uid.clone() };
                let exists = dev.sources.iter().any(|s| s.kind == kind);
                let alpha = if exists { 0.5 } else { 1.0 };
                let paint = |pt: &egui::Painter, r: Rect| glyph_tile(pt, r, t, alpha, icons::mic);
                if menu_row(ui, t, !exists, &h.name, &format!("{} ch", h.in_ch), paint) {
                    edits.push(Edit::AddSource(d, kind, h.in_ch.min(MAX_ROWS)));
                    ui.close();
                }
            }
            if p.inputs.is_empty() && !searching {
                ui.label("No input devices");
            }
            if !p.apps.is_empty() || !searching {
                heading(ui, "Applications");
            }
            for a in &p.apps {
                app_row(ui, g, a, edits);
            }
            if st.show_background && !p.background.is_empty() {
                heading(ui, "Background processes");
                for a in &p.background {
                    app_row(ui, g, a, edits);
                }
            }
            if searching && p.inputs.is_empty() && p.apps.is_empty() && p.background.is_empty() {
                ui.label("No matches");
            }
        });
    ui.separator();
    let exists = dev.sources.iter().any(|s| s.kind == SourceKind::PassThru);
    let alpha = if exists { 0.5 } else { 1.0 };
    let paint = |pt: &egui::Painter, r: Rect| glyph_tile(pt, r, t, alpha, loops);
    if menu_row(ui, t, !exists, "Pass-Thru", "", paint) {
        edits.push(Edit::AddSource(d, SourceKind::PassThru, dev.channels));
        ui.close();
    }
    let mut picked = false;
    ui.menu_button("Choose App…", |ui| {
        ui.set_min_width(210.0);
        if g.installed.is_empty() {
            ui.label("Scanning applications…");
        }
        egui::ScrollArea::vertical()
            .max_height(320.0)
            .show(ui, |ui| {
                for a in g.installed {
                    let kind = SourceKind::App {
                        bundle_id: a.bundle_id.clone(),
                    };
                    let exists = dev.sources.iter().any(|s| s.kind == kind);
                    if ui
                        .add_enabled(!exists, egui::Button::new(&a.name))
                        .clicked()
                    {
                        edits.push(Edit::AddSource(d, kind, 2));
                        picked = true;
                        ui.close();
                    }
                }
            });
    });
    if picked {
        ui.close();
    }
    ui.checkbox(&mut st.show_background, "Show background processes");
}

fn monitor_menu(ui: &mut Ui, g: &GraphCtx, edits: &mut Vec<Edit>) {
    ui.set_min_width(210.0);
    ui.label(
        egui::RichText::new("Output Devices")
            .size(11.0)
            .color(g.t.text2),
    );
    let room = ui.ctx().content_rect().bottom() - ui.cursor().top() - 16.0;
    egui::ScrollArea::vertical()
        .max_height(room.clamp(96.0, 420.0))
        .show(ui, |ui| {
            let mut any = false;
            for h in g.hw.iter().filter(|h| h.out_ch > 0) {
                any = true;
                let exists = g.dev.monitors.iter().any(|m| m.device_uid == h.uid);
                let alpha = if exists { 0.5 } else { 1.0 };
                let paint =
                    |p: &egui::Painter, r: Rect| glyph_tile(p, r, g.t, alpha, icons::speaker);
                if menu_row(ui, g.t, !exists, &h.name, "", paint) {
                    edits.push(Edit::AddMonitor(
                        g.dev_idx,
                        h.uid.clone(),
                        h.out_ch.min(MAX_ROWS),
                    ));
                }
            }
            if !any {
                ui.label("No output devices");
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn card(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    edits: &mut Vec<Edit>,
    c: &CardLayout,
    dt: f32,
    key: &dyn Fn(&str) -> String,
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let r = c.rect;
    // Per-card facts: title, subtitle, icon, on-state, volume, option key.
    let (title, _sub, on, vol_db, opt_key) = match c.id {
        CardId::Src(i) => {
            let s = &dev.sources[i];
            let (title, sub) = source_title(g, s);
            (
                title,
                sub.to_string(),
                Some(!s.muted),
                s.volume_db,
                key(&s.id),
            )
        }
        CardId::Out(p) => (
            format!("Channels {} & {}", p * 2 + 1, p * 2 + 2),
            String::new(),
            None,
            0.0,
            String::new(),
        ),
        CardId::Mon(i) => {
            let m = &dev.monitors[i];
            (
                monitor_title(m, g.hw),
                String::new(),
                Some(m.enabled),
                m.volume_db,
                key(&format!("m:{}", m.device_uid)),
            )
        }
    };
    // A device source or monitor whose hardware is gone (or whose microphone access is off) says
    // so, dimmed.
    let note = match c.id {
        CardId::Src(i) => source_note(g, &dev.sources[i]),
        CardId::Mon(i) => {
            let uid = &dev.monitors[i].device_uid;
            (!g.hw.iter().any(|h| &h.uid == uid)).then_some(NOT_CONNECTED)
        }
        CardId::Out(_) => None,
    };
    // An unplugged monitor plays nothing, whatever reaches the output channels.
    let active = on.unwrap_or(true) && dev.enabled && note != Some(NOT_CONNECTED);
    let dim = if on.unwrap_or(true) && note != Some(NOT_CONNECTED) {
        1.0
    } else {
        0.45
    };
    let painter = ui.painter().clone();
    painter.add(
        Shadow {
            offset: [0, 3],
            blur: 14,
            spread: 0,
            color: t.shadow,
        }
        .as_shape(r, CornerRadius::same(11)),
    );
    painter.rect(
        r,
        CornerRadius::same(11),
        t.card,
        Stroke::new(1.0, t.card_stroke),
        StrokeKind::Inside,
    );

    // Header: title and On/Off switch.
    let note_w = note.map_or(0.0, |n| {
        painter
            .layout_no_wrap(n.to_string(), FontId::proportional(11.0), t.text2)
            .size()
            .x
            - 14.0
    });
    let title_w = r.width()
        - 28.0
        - if on.is_some() {
            62.0 + note_w.max(0.0)
        } else {
            0.0
        };
    text_left(
        ui,
        r.left() + 14.0,
        r.top() + HEAD_H / 2.0,
        &title,
        FontId::new(13.0, semibold()),
        t.text,
        title_w,
    );
    if let Some(n) = note {
        let hit = Rect::from_min_size(pos2(r.left() + 14.0, r.top()), vec2(title_w, HEAD_H));
        ui.interact(hit, Id::unique(("card_note", d, c.id)), Sense::hover())
            .widget_info(|| WidgetInfo::labeled(egui::Role::Label, true, format!("{title}: {n}")));
    }
    if let Some(on_now) = on {
        let sw = Rect::from_center_size(
            pos2(r.right() - 14.0 - 16.0, r.top() + HEAD_H / 2.0),
            vec2(32.0, 18.0),
        );
        let new = switch(
            ui,
            sw,
            Id::unique(("card_on", d, c.id)),
            on_now,
            t,
            &format!("{title} enabled"),
        );
        painter.text(
            pos2(sw.left() - 6.0, sw.center().y),
            Align2::RIGHT_CENTER,
            note.unwrap_or(if new { "On" } else { "Off" }),
            FontId::proportional(11.0),
            if note.is_some() { t.warn } else { t.text2 },
        );
        if new != on_now {
            edits.push(match c.id {
                CardId::Src(i) => Edit::SourceOn(d, i, new),
                CardId::Mon(i) => Edit::MonitorOn(d, i, new),
                CardId::Out(_) => unreachable!(),
            });
        }
    }

    painter.hline(
        r.x_range(),
        r.top() + HEAD_H,
        Stroke::new(1.0, t.card_stroke),
    );

    // Body: icon and channel rows with meters.
    let body_top = r.top() + HEAD_H;
    let body_h = body_height(c.rows, c.icon);
    let (label_x, meter_l) = if c.icon {
        let icon = Rect::from_min_size(
            pos2(r.left() + 14.0, body_top + (body_h - 36.0) / 2.0),
            vec2(36.0, 36.0),
        );
        match c.id {
            CardId::Src(i) => match &dev.sources[i].kind {
                SourceKind::PassThru => glyph_tile(&painter, icon, t, dim, loops),
                SourceKind::Device { .. } => glyph_tile(&painter, icon, t, dim, icons::mic),
                SourceKind::App { bundle_id } => {
                    app_icon(&painter, icon, g.icons, bundle_id, &title, dim);
                    if g.icons.get(bundle_id).is_some() {
                        let id = Id::unique(("card_icon", d, c.id));
                        ui.interact(icon, id, Sense::hover()).widget_info(|| {
                            WidgetInfo::labeled(egui::Role::Image, true, format!("{title} icon"))
                        });
                    }
                }
            },
            _ => unreachable!("only sources carry a big icon"),
        }
        (r.left() + 62.0, r.left() + 98.0)
    } else if matches!(c.id, CardId::Out(_)) {
        (r.left() + 16.0, r.left() + 96.0)
    } else {
        (r.left() + 16.0, r.left() + 52.0)
    };
    let total_rows = c.rows;
    for row in 0..c.rows {
        let y = Layout::row_y(c, row);
        let (ch_for_label, lbl_total) = match c.id {
            CardId::Out(p) => (p as u16 * 2 + row, 0),
            _ => (row, total_rows),
        };
        let lbl = match c.id {
            CardId::Out(_) => format!("Channel {}", ch_label(ch_for_label, lbl_total)),
            _ => ch_label(ch_for_label, lbl_total),
        };
        painter.text(
            pos2(label_x, y),
            Align2::LEFT_CENTER,
            lbl,
            FontId::proportional(11.0),
            t.text2.gamma_multiply(dim),
        );
        let lin = if active { row_level(g, c.id, row) } else { 0.0 };
        let lv = level(lin);
        let pk = st.peaks.entry((c.id, row)).or_default().update(lv, dt);
        meter(
            &painter,
            Rect::from_x_y_ranges(meter_l..=(r.right() - 18.0), (y - 3.0)..=(y + 3.0)),
            lv,
            pk,
            t,
            dim,
        );
    }

    // Options disclosure.
    if !c.options {
        return;
    }
    let y0 = body_top + body_h;
    painter.hline(r.x_range(), y0, Stroke::new(1.0, t.card_stroke));
    let opt = Rect::from_min_size(pos2(r.left(), y0), vec2(r.width(), FOOT_H));
    let resp = ui.interact(opt, Id::unique(("opt", d, c.id)), Sense::click());
    name_button(&resp, &format!("{title} options"));
    let col = if resp.hovered() { t.text } else { t.text2 };
    chevron(
        &painter,
        pos2(r.left() + 20.0, opt.center().y),
        c.expanded,
        col,
    );
    painter.text(
        pos2(r.left() + 32.0, opt.center().y),
        Align2::LEFT_CENTER,
        "Options",
        FontId::proportional(11.5),
        col,
    );
    if resp.clicked() && !st.expanded.remove(&opt_key) {
        st.expanded.insert(opt_key.clone());
    }
    if !c.expanded {
        return;
    }
    let vrow = Rect::from_min_size(
        pos2(r.left() + 14.0, y0 + FOOT_H),
        vec2(r.width() - 28.0, 22.0),
    );
    painter.text(
        pos2(vrow.left(), vrow.center().y),
        Align2::LEFT_CENTER,
        "Volume",
        FontId::proportional(11.0),
        t.text2,
    );
    // A volume outside the slider range (CLI or hand-edited config) is shown as is and only
    // changes when the slider is moved.
    let before = vol_db.clamp(-60.0, 12.0);
    let mut v = before;
    let slider =
        Rect::from_x_y_ranges((vrow.left() + 46.0)..=(vrow.right() - 52.0), vrow.y_range());
    let vol = ui.scope_builder(UiBuilder::new().max_rect(slider), |ui| {
        ui.spacing_mut().slider_width = slider.width();
        ui.add(Slider::new(&mut v, -60.0..=12.0).show_value(false))
    });
    name_widget(ui, &vol.inner, &format!("{title} volume"));
    slider_knob(&painter, vol.inner.rect, (v - -60.0) / 72.0, t);
    let moved = (v - before).abs() > 1e-4;
    painter.text(
        pos2(vrow.right(), vrow.center().y),
        Align2::RIGHT_CENTER,
        format!("{:+.1} dB", if moved { v } else { vol_db }),
        FontId::proportional(11.0),
        t.text2,
    );
    if moved {
        edits.push(match c.id {
            CardId::Src(i) => Edit::SourceVolume(d, i, v),
            CardId::Mon(i) => Edit::MonitorVolume(d, i, v),
            CardId::Out(_) => unreachable!(),
        });
    }
    let mut rm_y = y0 + FOOT_H + 26.0;
    if let CardId::Src(i) = c.id {
        let s = &dev.sources[i];
        if matches!(s.kind, SourceKind::App { .. }) {
            let row = Rect::from_min_size(
                pos2(r.left() + 14.0, y0 + FOOT_H + 24.0),
                vec2(r.width() - 28.0, 20.0),
            );
            let mut m = s.mute_original;
            let cb = ui.scope_builder(UiBuilder::new().max_rect(row), |ui| {
                ui.checkbox(
                    &mut m,
                    egui::RichText::new("Mute in original output").size(11.0),
                )
            });
            name_widget(ui, &cb.inner, &format!("{title} mute in original output"));
            if m != s.mute_original {
                edits.push(Edit::SourceMuteOriginal(d, i, m));
            }
            rm_y += MUTE_H;
        }
    }
    let rm = Rect::from_min_size(pos2(r.left() + 14.0, rm_y), vec2(120.0, 20.0));
    let resp = ui.interact(rm, Id::unique(("rm", d, c.id)), Sense::click());
    let (label, edit) = match c.id {
        CardId::Src(i) => ("Remove Source", Edit::RemoveSource(d, i)),
        CardId::Mon(i) => ("Remove Monitor", Edit::RemoveMonitor(d, i)),
        CardId::Out(_) => return,
    };
    name_button(&resp, &format!("{label} {title}"));
    painter.text(
        rm.left_center(),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(11.5),
        if resp.hovered() {
            t.danger
        } else {
            t.danger.gamma_multiply(0.75)
        },
    );
    if resp.clicked() {
        edits.push(edit);
    }
}

/// Linear level shown on a card row. Monitor rows apply the monitor volume: channels[out_ch] * 10^(vol/20).
fn row_level(g: &GraphCtx, id: CardId, row: u16) -> f32 {
    let Some(rd) = g.reading else { return 0.0 };
    match id {
        CardId::Src(i) => {
            let s = &g.dev.sources[i];
            rd.sources
                .iter()
                .find(|(sid, _)| *sid == s.id)
                .and_then(|(_, v)| v.get(usize::from(row)))
                .copied()
                .unwrap_or(0.0)
        }
        CardId::Out(p) => rd
            .channels
            .get(p * 2 + usize::from(row))
            .copied()
            .unwrap_or(0.0),
        CardId::Mon(i) => {
            let m = &g.dev.monitors[i];
            let gain = 10f32.powf(m.volume_db / 20.0);
            m.wires
                .iter()
                .filter(|w| w.dst_ch == row)
                .filter_map(|w| rd.channels.get(usize::from(w.out_ch)))
                .fold(0.0f32, |a, b| a.max(*b))
                * gain
        }
    }
}

fn link_color(l: Link, t: &Tokens) -> Color32 {
    let ch = match l.from {
        Port::Src { ch, .. } | Port::OutOut { ch } => ch,
        _ => 0,
    };
    t.ch[usize::from(ch % 2)]
}

fn port_color(p: Port, t: &Tokens) -> Color32 {
    link_color(Link { from: p, to: p }, t)
}

/// Accessible name of a port, e.g. "Safari channel 1 output".
fn port_label(g: &GraphCtx, p: Port) -> String {
    match p {
        Port::Src { src, ch } => format!(
            "{} channel {} output",
            source_title(g, &g.dev.sources[src]).0,
            ch + 1
        ),
        Port::OutIn { ch } => format!("Output channel {} input", ch + 1),
        Port::OutOut { ch } => format!("Output channel {} output", ch + 1),
        Port::Mon { mon, ch } => format!(
            "{} channel {} input",
            monitor_title(&g.dev.monitors[mon], g.hw),
            ch + 1
        ),
    }
}

fn wire_shape(a: egui::Pos2, b: egui::Pos2, color: Color32, w: f32) -> CubicBezierShape {
    CubicBezierShape::from_points_stroke(
        wire_points(a, b),
        false,
        Color32::TRANSPARENT,
        Stroke::new(w, color),
    )
}

fn wires_and_ports(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    edits: &mut Vec<Edit>,
    layout: &Layout,
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let ports = layout.ports();
    // Interaction first so this frame's drag state drives the painting below.
    for &p in &ports {
        let Some(pos) = layout.port_pos(p) else {
            continue;
        };
        let resp = ui.interact(
            Rect::from_center_size(pos, vec2(20.0, 20.0)),
            Id::unique(("port", d, p)),
            Sense::click_and_drag(),
        );
        name_button(&resp, &port_label(g, p));
        if resp.drag_started() {
            if p.is_output() {
                st.drag = Some(p);
            } else if let Some(l) = links(dev).into_iter().rev().find(|l| l.to == p) {
                edits.push(Edit::Unlink(d, l)); // grab an existing wire by its input end
                st.drag = Some(l.from);
            }
        }
        if resp.clicked() && !p.is_output() {
            edits.push(Edit::Disconnect(d, p));
        }
    }
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let hover = pointer.and_then(|pos| layout.hit_port(pos, 11.0, |_| true));
    let target = match (st.drag, pointer) {
        (Some(from), Some(pos)) => layout.hit_port(pos, 16.0, |p| can_link(from, p)),
        _ => None,
    };
    if let Some(from) = st.drag
        && ui.input(|i| i.pointer.any_released())
    {
        if let Some(to) = target {
            edits.push(Edit::Connect(d, Link { from, to }));
        }
        st.drag = None;
    }

    let painter = ui.painter().clone();
    let all = links(dev);
    let src_active = |p: Port| match p {
        Port::Src { src, .. } => !dev.sources[src].muted,
        _ => true,
    };
    // Only the graph's own layer: never a wire under an open menu or the setup sheet.
    let on_graph = |pos| ui.ctx().layer_id_at(pos).is_none_or(|l| l == ui.layer_id());
    let hovered_wire = match (hover, pointer, st.drag) {
        (None, Some(pos), None) if on_graph(pos) => {
            all.iter()
                .position(|l| match (layout.port_pos(l.from), layout.port_pos(l.to)) {
                    (Some(a), Some(b)) => wire_distance(&wire_points(a, b), pos) < 5.0,
                    _ => false,
                })
        }
        _ => None,
    };
    // Clicking a highlighted wire removes it (as does dragging its input end away).
    if let Some(n) = hovered_wire
        && ui.input(|i| i.pointer.primary_clicked())
    {
        edits.push(Edit::Unlink(d, all[n]));
    }
    for (n, l) in all.iter().enumerate() {
        let (Some(a), Some(b)) = (layout.port_pos(l.from), layout.port_pos(l.to)) else {
            continue;
        };
        let hot = hover == Some(l.from) || hover == Some(l.to) || hovered_wire == Some(n);
        let mut c = link_color(*l, t);
        if !src_active(l.from) || !dev.enabled {
            c = c.gamma_multiply(0.35);
        }
        painter.add(wire_shape(a, b, c, if hot { 3.5 } else { 2.0 }));
    }
    if let (Some(from), Some(pos)) = (st.drag, pointer)
        && let Some(a) = layout.port_pos(from)
    {
        let end = target.and_then(|p| layout.port_pos(p)).unwrap_or(pos);
        painter.add(wire_shape(
            a,
            end,
            port_color(from, t).gamma_multiply(0.75),
            2.0,
        ));
    }
    for &p in &ports {
        let Some(pos) = layout.port_pos(p) else {
            continue;
        };
        let c = port_color(p, t);
        let linked = all.iter().any(|l| l.from == p || l.to == p);
        let hot = hover == Some(p) || target == Some(p);
        let radius = if hot { PORT_R + 1.5 } else { PORT_R };
        painter.circle(
            pos,
            radius,
            if linked || hot { c } else { t.card },
            Stroke::new(1.5, c),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{apply, bare_config};
    use crate::theme::{install_fonts, tokens};
    use egui_kittest::{Harness, kittest::Queryable};

    struct S {
        st: GraphState,
        dev: VirtualDevice,
        edits: Vec<Edit>,
        layout: Option<Layout>,
    }

    fn rig(dev: VirtualDevice) -> Harness<'static, S> {
        Harness::builder()
            .with_size(vec2(1000.0, 600.0))
            .with_step_dt(0.05)
            .with_context_setup(install_fonts)
            .build_ui_state(
                |ui, s: &mut S| {
                    let tk = tokens(true);
                    let g = GraphCtx {
                        dev_idx: 0,
                        dev: &s.dev,
                        hw: &[],
                        apps: &[],
                        icons: &IconTextures::default(),
                        installed: &[],
                        reading: None,
                        t: &tk,
                        mic: TccStatus::Granted,
                        min_h: 500.0,
                    };
                    s.layout = Some(show(ui, &g, &mut s.st, &mut s.edits));
                },
                S {
                    st: GraphState::default(),
                    dev,
                    edits: Vec::new(),
                    layout: None,
                },
            )
    }

    fn port(h: &Harness<S>, p: Port) -> egui::Pos2 {
        h.state().layout.as_ref().unwrap().port_pos(p).unwrap()
    }

    /// Press at `from`, move through `to`, release; returns every edit emitted.
    fn drag(h: &mut Harness<S>, from: egui::Pos2, to: egui::Pos2) -> Vec<Edit> {
        h.state_mut().edits.clear();
        h.hover_at(from);
        h.step();
        h.drag_at(from);
        h.step();
        h.hover_at(from + (to - from) * 0.5);
        h.step();
        h.hover_at(to);
        h.step();
        h.step();
        h.drop_at(to);
        h.step();
        h.step();
        std::mem::take(&mut h.state_mut().edits)
    }

    fn device() -> VirtualDevice {
        let mut c = bare_config();
        apply(
            &mut c,
            Edit::AddSource(
                0,
                SourceKind::App {
                    bundle_id: "x.y".into(),
                },
                2,
            ),
        );
        apply(&mut c, Edit::AddMonitor(0, "spk".into(), 2));
        c.devices.remove(0)
    }

    #[test]
    fn dragging_output_port_to_input_port_connects() {
        let mut dev = device();
        dev.wires.clear();
        let mut h = rig(dev);
        let from = port(&h, Port::Src { src: 0, ch: 0 });
        let to = port(&h, Port::OutIn { ch: 1 });
        let edits = drag(&mut h, from, to + vec2(3.0, 2.0));
        let want = Link {
            from: Port::Src { src: 0, ch: 0 },
            to: Port::OutIn { ch: 1 },
        };
        assert_eq!(edits, vec![Edit::Connect(0, want)]);
    }

    #[test]
    fn dropping_on_empty_space_creates_nothing() {
        let mut dev = device();
        dev.wires.clear();
        let mut h = rig(dev);
        let from = port(&h, Port::Src { src: 0, ch: 0 });
        assert!(drag(&mut h, from, pos2(600.0, 500.0)).is_empty());
    }

    #[test]
    fn output_channel_drags_to_monitor_only() {
        let mut dev = device();
        dev.monitors[0].wires.clear();
        let mut h = rig(dev);
        let from = port(&h, Port::OutOut { ch: 0 });
        let to = port(&h, Port::Mon { mon: 0, ch: 1 });
        let edits = drag(&mut h, from, to);
        let want = Link {
            from: Port::OutOut { ch: 0 },
            to: Port::Mon { mon: 0, ch: 1 },
        };
        assert_eq!(edits, vec![Edit::Connect(0, want)]);
        // a source port may not drop straight onto a monitor
        let from = port(&h, Port::Src { src: 0, ch: 0 });
        assert!(drag(&mut h, from, to).is_empty());
    }

    #[test]
    fn dragging_an_input_end_rewires() {
        let mut h = rig(device());
        let grab = port(&h, Port::OutIn { ch: 0 });
        let drop = port(&h, Port::OutIn { ch: 1 });
        let edits = drag(&mut h, grab, drop);
        let old = Link {
            from: Port::Src { src: 0, ch: 0 },
            to: Port::OutIn { ch: 0 },
        };
        let new = Link {
            from: Port::Src { src: 0, ch: 0 },
            to: Port::OutIn { ch: 1 },
        };
        assert_eq!(edits, vec![Edit::Unlink(0, old), Edit::Connect(0, new)]);
    }

    #[test]
    fn clicking_an_input_port_disconnects_it() {
        let mut h = rig(device());
        let p = port(&h, Port::Mon { mon: 0, ch: 0 });
        h.hover_at(p);
        h.step();
        h.drag_at(p);
        h.step();
        h.drop_at(p);
        h.step();
        h.step();
        assert_eq!(
            h.state().edits,
            vec![Edit::Disconnect(0, Port::Mon { mon: 0, ch: 0 })]
        );
    }

    /// Press and release at `at` without moving; returns the edits.
    fn click(h: &mut Harness<S>, at: egui::Pos2) -> Vec<Edit> {
        h.state_mut().edits.clear();
        h.hover_at(at);
        h.step();
        h.drag_at(at);
        h.step();
        h.drop_at(at);
        h.step();
        h.step();
        std::mem::take(&mut h.state_mut().edits)
    }

    #[test]
    fn clicking_a_wire_removes_just_that_wire() {
        let mut h = rig(device());
        let (a, b) = (
            port(&h, Port::OutOut { ch: 1 }),
            port(&h, Port::Mon { mon: 0, ch: 1 }),
        );
        let [p0, p1, p2, p3] = wire_points(a, b);
        let mid = pos2(
            (p0.x + 3.0 * p1.x + 3.0 * p2.x + p3.x) / 8.0,
            (p0.y + 3.0 * p1.y + 3.0 * p2.y + p3.y) / 8.0,
        );
        let want = Link {
            from: Port::OutOut { ch: 1 },
            to: Port::Mon { mon: 0, ch: 1 },
        };
        assert_eq!(click(&mut h, mid), vec![Edit::Unlink(0, want)]);
        // empty space is not a wire
        assert!(click(&mut h, pos2(600.0, 500.0)).is_empty());
    }

    #[test]
    fn opening_options_never_rewrites_a_volume_outside_the_slider_range() {
        for db in [-80.0, -200.0, 18.0] {
            let mut dev = device();
            dev.sources[0].volume_db = db;
            dev.monitors[0].volume_db = db;
            let mut h = rig(dev);
            h.state_mut().st.expanded = ["v1/s1", "v1/m:spk"].map(String::from).into();
            h.run_steps(3);
            assert!(h.state().edits.is_empty(), "{db}: {:?}", h.state().edits);
            h.get_by_label("y volume");
            h.get_by_label("spk volume");
        }
    }

    #[test]
    fn an_absent_device_keeps_its_last_seen_name_and_says_not_connected() {
        let mut c = bare_config();
        apply(&mut c, Edit::AddMonitor(0, "spk".into(), 2));
        let mic = SourceKind::Device { uid: "mic".into() };
        apply(&mut c, Edit::AddSource(0, mic, 1));
        c.devices[0].monitors[0].name = "Desk Speakers".into();
        c.devices[0].sources[0].name = "USB Mic".into();
        let h = rig(c.devices.remove(0));
        h.get_by_label("Desk Speakers: Not connected");
        h.get_by_label("USB Mic: Not connected");
        h.get_by_label("Desk Speakers channel 1 input");
    }

    /// A screen reader (and the tests above) reach a port by name; it must sit on the painted port.
    #[test]
    fn ports_are_named_where_they_are_painted() {
        let h = rig(device());
        for (label, p) in [
            ("y channel 1 output", Port::Src { src: 0, ch: 0 }),
            ("Output channel 2 input", Port::OutIn { ch: 1 }),
            ("Output channel 1 output", Port::OutOut { ch: 0 }),
            ("spk channel 2 input", Port::Mon { mon: 0, ch: 1 }),
        ] {
            let at = h.get_by_label(label).rect().center();
            assert!(at.distance(port(&h, p)) < 1.0, "{label}: {at:?}");
        }
    }

    #[test]
    fn picks_offer_inputs_but_never_our_own_devices() {
        let hw = |uid: &str, in_ch, out_ch| HwDevice {
            uid: uid.into(),
            name: uid.into(),
            in_ch,
            out_ch,
        };
        let devices = [
            hw("BuiltInMicrophoneDevice", 1, 0),
            hw("BuiltInSpeakerDevice", 0, 2),
            hw("soundrouter:v1", 4, 4),
            hw("soundrouter:v1.engine", 4, 4),
            hw("soundrouter.agg.v1.3", 4, 4),
            hw("BlackHole2ch_UID", 2, 2),
        ];
        let got: Vec<_> = picks(&devices, &[], "")
            .inputs
            .iter()
            .map(|h| h.uid.as_str())
            .collect();
        assert_eq!(got, ["BuiltInMicrophoneDevice", "BlackHole2ch_UID"]);
        let got = picks(&devices, &[], " black ").inputs.len();
        assert_eq!(got, 1);
    }

    #[test]
    fn picks_put_playing_apps_first_then_gui_apps_and_hide_daemons() {
        let app = |name: &str, running_output, regular| AppInfo {
            bundle_id: format!("id.{name}"),
            name: name.into(),
            running_output,
            regular,
        };
        let apps = [
            app("zeta", false, true),
            app("heard", false, false),
            app("Beta", false, true),
            app("daemon-player", true, false),
            app("Alpha", true, true),
            app("assistantd", false, false),
        ];
        let names = |v: &[&AppInfo]| v.iter().map(|a| a.name.clone()).collect::<Vec<_>>();
        let p = picks(&[], &apps, "");
        // A playing process is always shown, even a daemon; the rest of the GUI apps follow by name.
        assert_eq!(names(&p.apps), ["Alpha", "daemon-player", "Beta", "zeta"]);
        assert_eq!(names(&p.background), ["assistantd", "heard"]);
        let p = picks(&[], &apps, "ASSIST");
        assert!(p.apps.is_empty());
        assert_eq!(names(&p.background), ["assistantd"]);
        assert!(picks(&[], &apps, "nothing like this").apps.is_empty());
    }
}
