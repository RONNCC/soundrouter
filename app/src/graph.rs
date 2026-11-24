//! The four-column graph (Sources | Processing | Output Channels | Monitors): painting and
//! interaction.
//! Reads the config, emits `Edit`s; never mutates it.
use crate::app_icons::{LOOK_AHEAD, Look, PickerIcons, visible_rows, wanted};
use crate::cables::{Cables, assign};
use crate::icons::{self, IconTextures};
use crate::layout::{
    CardId, CardLayout, CardSpec, FOOT_H, HEAD_H, Layout, MUTE_H, NodeSpec, ROW_H as ROW_PITCH,
    SAG_MAX, STRIP_LIMIT_H, STRIP_METER_H, STRIP_ROW_H, TOP, body_height, wire_distance, wire_mid,
    wire_points,
};
use crate::meter::{PeakHold, level};
use crate::model::{
    Edit, FxEdit, Link, Module, NewNode, Port, balance_db, balanced, can_add_fx, can_add_node,
    can_link, ch_label, fan_in, fx_kinds, fx_name, links, node_name, pan_of, wire_of,
};
use crate::theme::{Tokens, body, cable_color, caption, label, num, radius, section, tile_tint};
use crate::widgets::{
    MIN_HIT, Plug, chevron, darken, halo, hit, hit_rect, jack, loops, menu_hint, meter, meter_v,
    name_button, name_widget, plus_circle, plus_glyph, slider, switch, tile, trim_glyph, x_button,
    xmark,
};
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Id, Popup, PopupCloseBehavior, Pos2, Rect,
    Response, Sense, Shape, Stroke, StrokeKind, TextStyle, Ui, UiBuilder, Vec2, WidgetInfo,
    epaint::CubicBezierShape, pos2, text::LayoutJob, vec2,
};
use sr_core::backend::{AppInfo, HwDevice, MeterReading, TccStatus};
use sr_core::config::{Config, Fx, Monitor, Node, Source, SourceKind, VirtualDevice, Wire};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

const MAX_ROWS: u16 = 32;
/// Room kept below the column headers for an empty column's invitation.
const EMPTY_H: f32 = 190.0;

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
    /// When each wire of the shown device was plugged in (egui time), for the plug-in pop; a
    /// wire that was already there when the device came on screen holds `NEG_INFINITY`.
    pub plugs: HashMap<(Port, Port), f64>,
    plugs_dev: String,
    /// Palette slot of every cable of the shown device, kept until its wires change.
    cables: Cables,
    pub drag: Option<Port>,
    /// The source cable whose trim controls were showing last frame, so the pointer can travel from
    /// the cable to its x, which sits off the cable.
    trim_hot: Option<Link>,
    /// Text of the open trim popover's gain field while it is being typed in.
    trim_text: String,
    /// Text of the mixer inspector field that has the keyboard, while it is being typed in.
    field_text: String,
    /// Filled once by a background scan (started on the first frame), see `sr_audio::apps::installed_apps`.
    pub installed: Arc<OnceLock<Vec<AppInfo>>>,
    pub scan_started: bool,
    /// Sources popup: search text, whether it takes focus on the next open, and whether
    /// daemons/agents are listed.
    pub filter: String,
    pub focus_search: bool,
    pub show_background: bool,
    /// Which button's popup the search text belongs to, when more than one can open it.
    filter_owner: Option<Id>,
    /// Icons of the Choose App… list, loaded for the rows in view only.
    pub picker: PickerIcons,
    /// What the Add module menu applies a module to, as last chosen there.
    module_at: Option<NewNode>,
}

/// A row per hardware output channel, and for every wired channel past them (a headset that
/// dropped to one channel in a call), so a wire the engine warns about can be seen and removed.
fn mon_rows(m: &Monitor, hw: &[HwDevice]) -> u16 {
    let wired = m.wires.iter().map(|w| w.dst_ch + 1).max().unwrap_or(0);
    match hw.iter().find(|h| h.uid == m.device_uid) {
        Some(h) => h.out_ch.max(wired).clamp(1, MAX_ROWS),
        None => wired.clamp(2, MAX_ROWS),
    }
}

/// `name` when known, else `fallback`: what an absent app or device is called.
fn last_seen(name: &str, fallback: &str) -> String {
    if name.is_empty() { fallback } else { name }.to_string()
}

fn source_title(
    hw: &[HwDevice],
    apps: &[AppInfo],
    installed: &[AppInfo],
    s: &Source,
) -> (String, &'static str) {
    match &s.kind {
        SourceKind::App { bundle_id } => (
            apps.iter()
                .chain(installed)
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

/// What an undoable edit did, for the Undo toast: "{verb} {name}." A card removal names the card
/// as it shows it; balancing cables names the output channel.
pub fn undo_note(
    cfg: &Config,
    edit: &Edit,
    hw: &[HwDevice],
    apps: &[AppInfo],
    installed: &[AppInfo],
) -> Option<(&'static str, String)> {
    Some(match *edit {
        Edit::RemoveDevice(d) => ("Removed", cfg.devices.get(d)?.name.clone()),
        Edit::RemoveSource(d, s) => (
            "Removed",
            source_title(hw, apps, installed, cfg.devices.get(d)?.sources.get(s)?).0,
        ),
        Edit::RemoveMonitor(d, m) => (
            "Removed",
            monitor_title(cfg.devices.get(d)?.monitors.get(m)?, hw),
        ),
        Edit::BalanceCables(_, ch) => (
            "Balanced cables into",
            format!("channel {}", ch_label(ch, 0)),
        ),
        Edit::RemoveNode(d, n) => (
            "Removed",
            node_name(cfg.devices.get(d)?.nodes.get(n)?).to_string(),
        ),
        _ => return None,
    })
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
        parts.push(format!("{apps} app{}", if apps == 1 { "" } else { "s" }));
    }
    if devs > 0 {
        parts.push(format!("{devs} device{}", if devs == 1 { "" } else { "s" }));
    }
    if dev.sources.iter().any(|s| s.kind == SourceKind::PassThru) {
        parts.push("Pass-Thru".into());
    }
    if parts.is_empty() {
        "No sources".into()
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
    let node_specs: Vec<NodeSpec> = dev
        .nodes
        .iter()
        .map(|n| NodeSpec {
            rows: n.channels,
            fx: n.fx.len(),
            limiter: has_limiter(n),
        })
        .collect();
    let origin = ui.cursor().min;
    let width = ui.available_width();
    let mut layout = Layout::compute(
        origin,
        width,
        &src_specs,
        usize::from(dev.channels / 2),
        &mon_specs,
        &node_specs,
    );
    // The Processing column's empty state sits below every cable crossing the column (so they
    // show running straight through) and is as tall as its words. A cable hangs at most 3/4 of
    // `SAG_MAX` below its lower end.
    if let Some(r) = layout.proc_empty {
        let low = links(dev)
            .iter()
            .filter(|l| matches!(l.to, Port::OutIn { .. }))
            .filter_map(|l| Some(layout.port_pos(l.from)?.y.max(layout.port_pos(l.to)?.y)))
            .fold(r.top(), |m, y| m.max(y + 0.75 * SAG_MAX + 10.0));
        layout.proc_empty = Some(Rect::from_min_size(
            pos2(r.left(), low),
            vec2(r.width(), slot_height(ui, g.t, r.width(), PROC_EMPTY)),
        ));
    }
    let need = if dev.sources.is_empty() || dev.monitors.is_empty() {
        TOP + EMPTY_H
    } else {
        0.0
    };
    let need = layout
        .proc_empty
        .map_or(need, |r| need.max(r.bottom() - origin.y + 8.0));
    ui.allocate_exact_size(
        vec2(width, layout.size.y.max(g.min_h).max(need)),
        Sense::hover(),
    );

    column_headers(ui, g, st, &layout, edits);
    empty_columns(ui, g, st, &layout, edits);
    for c in &layout.cards {
        if !matches!(c.id, CardId::Node(_)) {
            card(ui, g, st, edits, c, dt, &key);
        }
    }
    // Module cards and the empty state go over the cables that cross the Processing column,
    // under the jacks and the cable controls.
    wires_and_ports(ui, g, st, edits, &layout, &mut |ui, st, edits| {
        if let Some(r) = layout.proc_empty {
            let resp = empty_slot(
                ui,
                g.t,
                r.min,
                r.width(),
                Id::unique(("empty_proc", g.dev_idx)),
                PROC_EMPTY,
                "Add module",
                "Add module",
            );
            Popup::menu(&resp)
                .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| module_menu(ui, g, st, edits));
        }
        for c in &layout.cards {
            if let CardId::Node(n) = c.id {
                strip(ui, g, st, edits, c, n, dt);
            }
        }
    });
    st.picker.end_frame();
    layout
}

/// What the Processing column says while it holds no module.
const PROC_EMPTY: &str = "No processing. Sound goes straight to the outputs.";

fn column_headers(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    layout: &Layout,
    edits: &mut Vec<Edit>,
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let base = layout.origin.y + TOP;
    let heads: [(&str, String); 4] = [
        ("Sources", sources_summary(dev)),
        ("Processing", "Optional".to_string()),
        ("Output channels", format!("{} channels", dev.channels)),
        (
            "Monitors",
            match dev.monitors.len() {
                0 => "No devices".to_string(),
                1 => "1 device".to_string(),
                n => format!("{n} devices"),
            },
        ),
    ];
    // Every title takes one size: the largest at which the widest fits its column (a step or two
    // down at the minimum window), never eliding.
    let max_w = |i: usize| (if i == 1 { layout.proc_w } else { layout.card_w }) - 34.0;
    let scale = heads
        .iter()
        .enumerate()
        .map(|(i, (title, _))| {
            let w = ui
                .painter()
                .layout_no_wrap(title.to_string(), section(), t.ink)
                .size()
                .x;
            (max_w(i) - 2.0) / w
        })
        .fold(1.0_f32, f32::min)
        .max(0.7);
    for (i, (title, sub)) in heads.iter().enumerate() {
        let x = layout.origin.x + layout.cols[i];
        let w = if i == 1 { layout.proc_w } else { layout.card_w };
        let mut font = section();
        font.size *= scale;
        text_left(ui, x + 2.0, base - 37.0, title, font, t.ink, max_w(i));
        text_left(ui, x + 2.0, base - 14.0, sub, caption(), t.muted, max_w(i));
        let right = x + w;
        let name = [
            "Add source",
            "Add processing module",
            "Output channel options",
            "Add monitor",
        ][i];
        let resp = plus_circle(
            ui,
            pos2(right - 20.0, base - 28.0),
            Id::unique(("hdr", d, i)),
            t,
            false,
            name,
        );
        menu_hint(
            ui.painter(),
            pos2(right - 4.0, base - 28.0),
            hint_color(&resp, t),
        );
        match i {
            0 => source_popup(g, st, edits, &resp),
            1 => {
                Popup::menu(&resp)
                    .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| module_menu(ui, g, st, edits));
            }
            2 => {
                Popup::menu(&resp).show(|ui| output_menu(ui, g, edits));
            }
            _ => {
                Popup::menu(&resp).show(|ui| monitor_menu(ui, g, edits));
            }
        }
    }
}

fn hint_color(resp: &Response, t: &Tokens) -> Color32 {
    if resp.hovered() { t.ink } else { t.muted }
}

/// The Sources popup under `anchor`. Its search starts empty and focused on every open, whichever
/// button opened it.
fn source_popup(g: &GraphCtx, st: &mut GraphState, edits: &mut Vec<Edit>, anchor: &Response) {
    let shown = Popup::menu(anchor)
        .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| source_menu(ui, g, st, edits));
    if shown.is_some() {
        st.filter_owner = Some(anchor.id);
    } else if st.filter_owner.is_none_or(|o| o == anchor.id) {
        st.filter_owner = None;
        st.filter.clear();
        st.focus_search = true;
    }
}

/// Closed outline of a rounded rectangle, to dash.
fn rounded_path(r: Rect, rad: f32) -> Vec<Pos2> {
    let corners = [
        (pos2(r.right() - rad, r.top() + rad), -90.0f32),
        (pos2(r.right() - rad, r.bottom() - rad), 0.0),
        (pos2(r.left() + rad, r.bottom() - rad), 90.0),
        (pos2(r.left() + rad, r.top() + rad), 180.0),
    ];
    let mut path = Vec::new();
    for (c, from) in corners {
        for k in 0..=6 {
            let a = (from + 15.0 * k as f32).to_radians();
            path.push(c + rad * vec2(a.cos(), a.sin()));
        }
    }
    path.push(path[0]);
    path
}

const SLOT_PAD: f32 = 16.0;

/// Height of an `empty_slot` `w` wide saying `text`.
fn slot_height(ui: &Ui, t: &Tokens, w: f32, text: &str) -> f32 {
    let words = ui
        .painter()
        .layout(text.to_string(), body(), t.muted, w - 2.0 * SLOT_PAD);
    SLOT_PAD + words.size().y + 12.0 + 28.0 + SLOT_PAD
}

/// A dashed slot where a column has no cards: one sentence and the button that fills it. The
/// slot is filled, so cables crossing the Processing column pass under it.
#[allow(clippy::too_many_arguments)]
fn empty_slot(
    ui: &mut Ui,
    t: &Tokens,
    top_left: Pos2,
    w: f32,
    id: Id,
    text: &str,
    button: &str,
    name: &str,
) -> Response {
    const PAD: f32 = SLOT_PAD;
    let p = ui.painter().clone();
    let words = p.layout(text.to_string(), body(), t.muted, w - 2.0 * PAD);
    let btn_w = p
        .layout_no_wrap(button.to_string(), label(), t.ink)
        .size()
        .x
        + 28.0;
    let rect = Rect::from_min_size(top_left, vec2(w, slot_height(ui, t, w, text)));
    p.rect_filled(rect, CornerRadius::same(radius::STRIP), t.canvas);
    let dash = Stroke::new(1.0, t.muted.gamma_multiply(0.5));
    p.extend(Shape::dashed_line(
        &rounded_path(rect, f32::from(radius::STRIP)),
        dash,
        5.0,
        4.0,
    ));
    let btn = Rect::from_min_size(
        pos2(rect.left() + PAD, rect.top() + PAD + words.size().y + 12.0),
        vec2(btn_w, 28.0),
    );
    // The sentence is read out too, not only painted.
    let said = Rect::from_min_size(rect.min + vec2(PAD, PAD), words.size());
    p.galley(said.min, words, t.muted);
    ui.interact(said, id.with("words"), Sense::hover())
        .widget_info(|| WidgetInfo::labeled(egui::Role::Label, true, text));
    let resp = hit(ui, btn, id);
    name_button(&resp, name);
    let fill = if resp.is_pointer_button_down_on() {
        t.selected.lerp_to_gamma(Color32::BLACK, 0.12)
    } else if resp.hovered() {
        t.selected
    } else {
        t.raised
    };
    let pill = CornerRadius::same(radius::PILL);
    p.rect(
        btn,
        pill,
        fill,
        Stroke::new(1.0, t.hairline),
        StrokeKind::Inside,
    );
    p.text(btn.center(), Align2::CENTER_CENTER, button, label(), t.ink);
    if resp.has_focus() {
        p.rect_stroke(btn, pill, Stroke::new(2.0, t.focus), StrokeKind::Outside);
    }
    resp
}

/// The invitation in a column with no cards.
fn empty_columns(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    layout: &Layout,
    edits: &mut Vec<Edit>,
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let top = layout.origin.y + TOP;
    if dev.sources.is_empty() {
        let at = pos2(layout.origin.x + layout.cols[0], top);
        let resp = empty_slot(
            ui,
            t,
            at,
            layout.card_w,
            Id::unique(("empty_src", d)),
            "Nothing plugged in yet. Add an app or a microphone and drag a cable to an output.",
            "Add source",
            "Add source (empty list)",
        );
        source_popup(g, st, edits, &resp);
    }
    if dev.monitors.is_empty() {
        let at = pos2(layout.origin.x + layout.cols[3], top);
        let resp = empty_slot(
            ui,
            t,
            at,
            layout.card_w,
            Id::unique(("empty_mon", d)),
            "Nobody is listening yet. Add speakers or headphones to hear this device.",
            "Add monitor",
            "Add monitor (empty list)",
        );
        Popup::menu(&resp).show(|ui| monitor_menu(ui, g, edits));
    }
}

/// The window with no devices: two jacks joined by a sagging cable, and the one thing to do.
/// Returns true when "Create your first device" is clicked.
pub fn no_devices(ui: &mut Ui, t: &Tokens, area: Rect) -> bool {
    let p = ui.painter().clone();
    let mid = area.center();
    let art = mid.y - 84.0;
    let (a, b) = (pos2(mid.x - 76.0, art), pos2(mid.x + 76.0, art));
    let cable = t.channel(0);
    for end in [a, b] {
        p.circle(end, 22.0, t.raised, Stroke::new(2.0, t.hairline));
    }
    draw_cable(&p, wire_points(a, b), cable, 6.0);
    for end in [a, b] {
        jack(&p, end, Plug::One(cable), 2.4, false, t);
    }
    p.text(
        pos2(mid.x, mid.y - 14.0),
        Align2::CENTER_CENTER,
        "No devices yet",
        section(),
        t.ink,
    );
    let words = p.layout(
        "A device collects sound from apps and microphones and sends it to speakers or headphones."
            .to_string(),
        body(),
        t.muted,
        340.0,
    );
    let top = mid.y + 10.0;
    p.galley(
        pos2(mid.x - words.size().x / 2.0, top),
        words.clone(),
        t.muted,
    );
    let text = "Create your first device";
    let btn_w = p
        .layout_no_wrap(text.to_string(), label(), Color32::WHITE)
        .size()
        .x
        + 36.0;
    let btn = Rect::from_center_size(
        pos2(mid.x, top + words.size().y + 16.0 + 16.0),
        vec2(btn_w, 32.0),
    );
    let resp = hit(ui, btn, Id::unique("create_first_device"));
    name_button(&resp, text);
    let fill = if resp.is_pointer_button_down_on() {
        t.accent.lerp_to_gamma(Color32::BLACK, 0.28)
    } else if resp.hovered() {
        t.accent.lerp_to_gamma(Color32::BLACK, 0.15)
    } else {
        t.accent
    };
    let pill = CornerRadius::same(radius::PILL);
    p.rect_filled(btn, pill, fill);
    p.text(
        btn.center(),
        Align2::CENTER_CENTER,
        text,
        label(),
        Color32::WHITE,
    );
    if resp.has_focus() {
        p.rect_stroke(btn, pill, Stroke::new(2.0, t.focus), StrokeKind::Outside);
    }
    resp.clicked()
}

/// Height of a menu row.
const ROW_H: f32 = 28.0;

/// One menu line: icon, label, accent highlight on hover. Returns true when clicked.
fn menu_row(
    ui: &mut Ui,
    t: &Tokens,
    enabled: bool,
    label: &str,
    detail: &str,
    icon: impl FnOnce(&egui::Painter, Rect),
) -> bool {
    let clicked = menu_item(ui, t, enabled, label, detail, icon).clicked();
    enabled && clicked
}

/// `menu_row`'s line, for a caller that names it differently to a screen reader.
fn menu_item(
    ui: &mut Ui,
    t: &Tokens,
    enabled: bool,
    label: &str,
    detail: &str,
    icon: impl FnOnce(&egui::Painter, Rect),
) -> Response {
    const ICON: f32 = 18.0;
    let font = TextStyle::Button.resolve(ui.style());
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), sense);
    let resp = if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    };
    resp.widget_info(|| WidgetInfo::labeled(egui::Role::Button, enabled, label));
    let hot = enabled && resp.hovered();
    let p = ui.painter();
    if hot {
        p.rect_filled(rect, CornerRadius::same(4), t.accent);
    }
    if resp.has_focus() {
        p.rect_stroke(
            rect,
            CornerRadius::same(4),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    let color = match (enabled, hot) {
        (_, true) => Color32::WHITE,
        (true, _) => t.ink,
        _ => t.muted.gamma_multiply(0.6),
    };
    let icon_rect = Rect::from_center_size(
        pos2(rect.left() + 8.0 + ICON / 2.0, rect.center().y),
        vec2(ICON, ICON),
    );
    icon(p, icon_rect);
    let mut text_w = rect.width() - ICON - 22.0;
    if !detail.is_empty() {
        let d = p.layout_no_wrap(detail.to_string(), caption(), Color32::PLACEHOLDER);
        text_w -= d.size().x + 8.0;
        let dc = if hot { color } else { t.muted };
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
    resp
}

/// The app's real icon, else its coloured letter tile.
fn app_icon(p: &egui::Painter, r: Rect, icons: &IconTextures, key: &str, name: &str, alpha: f32) {
    match icons.get(key) {
        Some(tex) => icon_image(p, r, tex, alpha),
        None => letter_tile(p, r, name, alpha),
    }
}

fn icon_image(p: &egui::Painter, r: Rect, tex: &egui::TextureHandle, alpha: f32) {
    let full = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    p.image(tex.id(), r, full, Color32::WHITE.gamma_multiply(alpha));
}

fn letter_tile(p: &egui::Painter, r: Rect, name: &str, alpha: f32) {
    let glyph = name
        .chars()
        .next()
        .map_or("?".into(), |c| c.to_uppercase().to_string());
    tile(p, r, tile_tint(name), &glyph, r.height() * 0.53, alpha);
}

/// A Choose App… row's icon: the real one, the letter tile when the app has none, and a neutral
/// tile of the same size while it loads, so nothing shifts when it arrives.
fn picker_icon(p: &egui::Painter, r: Rect, t: &Tokens, look: &Look, name: &str, alpha: f32) {
    match look {
        Look::Icon(tex) => icon_image(p, r, tex, alpha),
        Look::Missing => letter_tile(p, r, name, alpha),
        Look::Loading => {
            p.rect_filled(
                r,
                CornerRadius::same((r.width() * 0.24) as u8),
                t.muted.gamma_multiply(0.2 * alpha),
            );
        }
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
    if search.has_focus() {
        // egui outlines a focused field in the selected-text colour (white): draw the focus ring.
        ui.painter().rect_stroke(
            search.rect,
            CornerRadius::same(radius::INPUT),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    if std::mem::take(&mut st.focus_search) {
        search.request_focus();
    }
    let searching = !st.filter.trim().is_empty();
    let p = picks(g.hw, g.apps, &st.filter);
    let room = ui.ctx().content_rect().bottom() - ui.cursor().top() - MENU_FOOT_H;
    let heading = |ui: &mut Ui, text: &str| {
        ui.label(egui::RichText::new(text).size(11.0).color(t.muted));
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
        ui.set_width(MENU_W);
        if g.installed.is_empty() {
            ui.label("Scanning applications…");
        }
        picked = installed_list(ui, g, &mut st.picker, edits);
    });
    if picked {
        ui.close();
    }
    ui.checkbox(&mut st.show_background, "Show background processes");
}

/// Choose App…: every installed app, newest-first icons only for the rows in view (plus a little
/// look-ahead), loaded off the UI thread. Returns whether an app was picked.
fn installed_list(
    ui: &mut Ui,
    g: &GraphCtx,
    picker: &mut PickerIcons,
    edits: &mut Vec<Edit>,
) -> bool {
    let ctx = ui.ctx().clone();
    picker.begin(&ctx);
    let mut picked = false;
    egui::ScrollArea::vertical()
        .max_height(320.0)
        .show(ui, |ui| {
            let pitch = ROW_H + ui.spacing().item_spacing.y;
            let clip = ui.clip_rect();
            let seen = visible_rows(
                ui.cursor().top(),
                pitch,
                g.installed.len(),
                clip.top(),
                clip.bottom(),
            );
            let ask = wanted(&seen, g.installed.len(), LOOK_AHEAD)
                .into_iter()
                .map(|i| g.installed[i].bundle_id.as_str())
                .filter(|k| g.icons.get(k).is_none());
            picker.request(&ctx, ask);
            for (i, a) in g.installed.iter().enumerate() {
                let kind = SourceKind::App {
                    bundle_id: a.bundle_id.clone(),
                };
                let exists = g.dev.sources.iter().any(|s| s.kind == kind);
                let alpha = if exists { 0.5 } else { 1.0 };
                // Rows out of view are not touched, so they age out of the cache.
                let look = match g.icons.get(&a.bundle_id) {
                    Some(tex) => Look::Icon(tex.clone()),
                    None if seen.contains(&i) => picker.look(&a.bundle_id),
                    None => Look::Loading,
                };
                let paint =
                    |p: &egui::Painter, r: Rect| picker_icon(p, r, g.t, &look, &a.name, alpha);
                if menu_row(ui, g.t, !exists, &a.name, "", paint) {
                    edits.push(Edit::AddSource(g.dev_idx, kind, 2));
                    picked = true;
                    ui.close();
                }
            }
        });
    picked
}

fn monitor_menu(ui: &mut Ui, g: &GraphCtx, edits: &mut Vec<Edit>) {
    ui.set_min_width(210.0);
    ui.label(
        egui::RichText::new("Output Devices")
            .size(11.0)
            .color(g.t.muted),
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

/// The Output channels menu: grow or shrink the device by a pair.
fn output_menu(ui: &mut Ui, g: &GraphCtx, edits: &mut Vec<Edit>) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    ui.set_min_width(210.0);
    let grow = usize::from(dev.channels) + 2 <= sr_core::config::MAX_CH;
    let mark = |minus| move |p: &egui::Painter, r: Rect| plus_glyph(p, r.center(), t.muted, minus);
    if menu_row(ui, t, grow, "Add channel pair", "", mark(false)) {
        edits.push(Edit::GrowChannels(d));
    }
    if menu_row(
        ui,
        t,
        dev.channels > 2,
        "Remove channel pair",
        "",
        mark(true),
    ) {
        edits.push(Edit::ShrinkChannels(d));
    }
}

/// The Add module menu: what the module works on (an output pair, which every source into it
/// then passes through, or one source), then what it is. A Mixer goes in unwired, to wire by
/// hand.
fn module_menu(ui: &mut Ui, g: &GraphCtx, st: &mut GraphState, edits: &mut Vec<Edit>) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    ui.set_width(250.0);
    let heading = |ui: &mut Ui, text: &str| {
        ui.label(egui::RichText::new(text).size(11.0).color(t.muted));
    };
    heading(ui, "Apply to");
    let targets: Vec<(NewNode, String)> = (0..dev.channels / 2)
        .map(|p| {
            let text = format!("All sources (Channels {} & {})", 2 * p + 1, 2 * p + 2);
            (NewNode::ForOutputs(2 * p), text)
        })
        .chain(dev.sources.iter().enumerate().map(|(i, s)| {
            let name = source_title(g.hw, g.apps, g.installed, s).0;
            (NewNode::ForSource(i), format!("{name} only"))
        }))
        .collect();
    let mut at = st
        .module_at
        .clone()
        .filter(|a| targets.iter().any(|(x, _)| x == a))
        .unwrap_or(NewNode::ForOutputs(0));
    for (target, text) in &targets {
        ui.radio_value(&mut at, target.clone(), text);
    }
    let mut row = |ui: &mut Ui, at: NewNode, m: Module, detail: &str| {
        let name = m.name();
        let enabled = can_add_node(dev, &at, &m);
        let glyph = |p: &egui::Painter, r: Rect| trim_glyph(p, r.center(), t.muted);
        let resp = menu_item(ui, t, enabled, name, detail, glyph);
        resp.widget_info(|| {
            WidgetInfo::labeled(egui::Role::Button, enabled, format!("Add module: {name}"))
        });
        if enabled && resp.clicked() {
            edits.push(Edit::AddNode(d, at, m));
            ui.close();
        }
    };
    ui.separator();
    heading(ui, "Module");
    for m in fx_kinds().into_iter().map(Module::Fx).chain([Module::Gain]) {
        row(ui, at.clone(), m, "");
    }
    ui.separator();
    row(ui, NewNode::Empty, Module::Mixer, "Wire by hand");
    st.module_at = Some(at);
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
            let (title, sub) = source_title(g.hw, g.apps, g.installed, s);
            (
                title,
                sub.to_string(),
                Some(!s.muted),
                s.volume_db,
                key(&s.id),
            )
        }
        CardId::Node(_) => unreachable!("strips are drawn by `strip`"),
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
        CardId::Out(_) | CardId::Node(_) => None,
    };
    // An unplugged monitor plays nothing, whatever reaches the output channels.
    let active = on.unwrap_or(true) && dev.enabled && note != Some(NOT_CONNECTED);
    let dim = if on.unwrap_or(true) && note != Some(NOT_CONNECTED) {
        1.0
    } else {
        0.45
    };
    let painter = ui.painter().clone();
    // Cards are raised by fill and hairline alone: shadows belong to floating layers.
    painter.rect(
        r,
        CornerRadius::same(radius::STRIP),
        t.raised,
        Stroke::new(1.0, t.hairline),
        StrokeKind::Inside,
    );

    // Header: title and On/Off switch.
    let note_w = note.map_or(0.0, |n| {
        painter
            .layout_no_wrap(n.to_string(), caption(), t.muted)
            .size()
            .x
            - 14.0
    });
    // A source keeps its identity colour: a small dot before the title.
    let title_x = r.left()
        + 14.0
        + match c.id {
            CardId::Src(i) => {
                let at = pos2(r.left() + 14.0 + 5.0, r.top() + HEAD_H / 2.0);
                painter.circle_filled(at, 4.5, source_color(dev, i).gamma_multiply(dim));
                16.0
            }
            _ => 0.0,
        };
    // A narrow card (beside mixers at a small window) gives the "On"/"Off" word's room to the
    // title; the switch still shows its state by its knob.
    let word = note.is_some() || r.width() >= 200.0;
    let title_w = r.right()
        - 14.0
        - title_x
        - match (on.is_some(), word) {
            (false, _) => 0.0,
            (true, true) => 94.0 + note_w.max(0.0),
            (true, false) => 70.0,
        };
    text_left(
        ui,
        title_x,
        r.top() + HEAD_H / 2.0,
        &title,
        label(),
        t.ink,
        title_w,
    );
    if let Some(n) = note {
        let hit = Rect::from_min_size(pos2(title_x, r.top()), vec2(title_w, HEAD_H));
        ui.interact(hit, Id::unique(("card_note", d, c.id)), Sense::hover())
            .widget_info(|| WidgetInfo::labeled(egui::Role::Label, true, format!("{title}: {n}")));
    }
    if on.is_some() {
        let (label, edit) = match c.id {
            CardId::Src(i) => ("Remove", Edit::RemoveSource(d, i)),
            CardId::Mon(i) => ("Remove", Edit::RemoveMonitor(d, i)),
            CardId::Out(_) | CardId::Node(_) => unreachable!(),
        };
        let at = pos2(r.right() - 14.0 - 8.0, r.top() + HEAD_H / 2.0);
        let x = x_button(
            ui,
            at,
            Id::unique(("card_x", d, c.id)),
            t,
            &format!("{label} {title}"),
        );
        // A double-click's second click would remove the card that moved up under the pointer.
        if x.clicked() && !x.double_clicked() {
            edits.push(edit);
        }
        x.on_hover_text(format!("{label} {title}"));
    }
    if let Some(on_now) = on {
        // 4 px between the X's target and the switch's; "On"/"Off" toggles it too.
        let sw = Rect::from_center_size(
            pos2(r.right() - 14.0 - 8.0 - 40.0, r.top() + HEAD_H / 2.0),
            vec2(32.0, 18.0),
        );
        let mut reach = sw.expand2(vec2(6.0, 0.0));
        if note.is_none() && word {
            reach.min.x -= 6.0 + 28.0;
        }
        let new = switch(
            ui,
            sw,
            reach,
            Id::unique(("card_on", d, c.id)),
            on_now,
            t,
            &format!("{title} enabled"),
        );
        if word {
            painter.text(
                pos2(sw.left() - 6.0, sw.center().y),
                Align2::RIGHT_CENTER,
                note.unwrap_or(if new { "On" } else { "Off" }),
                caption(),
                if note.is_some() { t.warn_ink } else { t.muted },
            );
        }
        if new != on_now {
            edits.push(match c.id {
                CardId::Src(i) => Edit::SourceOn(d, i, new),
                CardId::Mon(i) => Edit::MonitorOn(d, i, new),
                CardId::Out(_) | CardId::Node(_) => unreachable!(),
            });
        }
    }

    painter.hline(r.x_range(), r.top() + HEAD_H, Stroke::new(1.0, t.hairline));

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
        let lbl = ch_label(ch_for_label, lbl_total);
        let ink = t.muted.gamma_multiply(dim);
        let mut x = label_x;
        if matches!(c.id, CardId::Out(_)) {
            x += painter
                .text(pos2(x, y), Align2::LEFT_CENTER, "Channel", caption(), ink)
                .width()
                + 5.0;
        }
        painter.text(pos2(x, y), Align2::LEFT_CENTER, lbl, num(11.0), ink);
        let lin = if active { row_level(g, c.id, row) } else { 0.0 };
        let lv = level(lin);
        let pk = st.peaks.entry((c.id, row)).or_default().update(lv, dt);
        if pk > lv {
            // The peak marker is still holding or falling, and no meter change will wake the UI.
            ui.request_repaint_after(std::time::Duration::from_millis(33));
        }
        meter(
            &painter,
            Rect::from_x_y_ranges(meter_l..=(r.right() - 18.0), (y - 4.0)..=(y + 4.0)),
            lv,
            pk,
            t,
            dim,
        );
    }

    // Options disclosure: a quiet chevron at the foot of the card while it is hot.
    if !c.options {
        return;
    }
    let y0 = body_top + body_h;
    // The strip reaches up into the card's last meter row: nothing there takes a click, and below
    // it the volume slider starts.
    let strip = Rect::from_min_max(
        pos2(r.left() + 14.0, y0 + FOOT_H - MIN_HIT),
        pos2(r.right() - 14.0, y0 + FOOT_H),
    );
    let resp = hit(ui, strip, Id::unique(("opt", d, c.id)));
    name_button(&resp, &format!("{title} options"));
    let aimed = resp.hovered() || resp.has_focus();
    if c.expanded || aimed || ui.rect_contains_pointer(r) {
        let col = if aimed { t.ink } else { t.muted };
        chevron(
            &painter,
            pos2(r.center().x, y0 + FOOT_H / 2.0),
            c.expanded,
            col,
        );
    }
    if resp.has_focus() {
        painter.rect_stroke(
            strip,
            CornerRadius::same(radius::INPUT),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    if resp.clicked() && !st.expanded.remove(&opt_key) {
        st.expanded.insert(opt_key.clone());
    }
    if !c.expanded {
        return;
    }
    painter.hline(r.x_range(), y0, Stroke::new(1.0, t.hairline));
    let vrow = Rect::from_min_size(
        pos2(r.left() + 14.0, y0 + FOOT_H),
        vec2(r.width() - 28.0, MIN_HIT),
    );
    painter.text(
        pos2(vrow.left(), vrow.center().y),
        Align2::LEFT_CENTER,
        "Volume",
        caption(),
        t.muted,
    );
    // A volume outside the slider range (CLI or hand-edited config) is shown as is and only
    // changes when the slider is moved.
    let before = vol_db.clamp(-60.0, 12.0);
    let mut v = before;
    let slider_rect =
        Rect::from_x_y_ranges((vrow.left() + 46.0)..=(vrow.right() - 60.0), vrow.y_range());
    slider(
        ui,
        slider_rect,
        Id::unique(("card_vol", d, c.id)),
        &format!("{title} volume"),
        &mut v,
        -60.0..=12.0,
        0.0,
        t,
    );
    let moved = (v - before).abs() > 1e-4;
    painter.text(
        pos2(vrow.right(), vrow.center().y),
        Align2::RIGHT_CENTER,
        format!("{:+.1} dB", if moved { v } else { vol_db }),
        num(11.0),
        t.muted,
    );
    if moved {
        edits.push(match c.id {
            CardId::Src(i) => Edit::SourceVolume(d, i, v),
            CardId::Mon(i) => Edit::MonitorVolume(d, i, v),
            CardId::Out(_) | CardId::Node(_) => unreachable!(),
        });
    }
    let mut rm_y = y0 + FOOT_H + MIN_HIT;
    if let CardId::Src(i) = c.id {
        let s = &dev.sources[i];
        if matches!(s.kind, SourceKind::App { .. }) {
            let row =
                Rect::from_min_size(pos2(r.left() + 14.0, rm_y), vec2(r.width() - 28.0, MIN_HIT));
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
    let (label, edit) = match c.id {
        CardId::Src(i) => ("Remove Source", Edit::RemoveSource(d, i)),
        CardId::Mon(i) => ("Remove Monitor", Edit::RemoveMonitor(d, i)),
        CardId::Out(_) | CardId::Node(_) => return,
    };
    // The word is "Remove" on the card, as everywhere; the reader hears what goes.
    let words = painter.layout_no_wrap("Remove".into(), caption(), Color32::PLACEHOLDER);
    let rm = Rect::from_min_size(
        pos2(r.left() + 14.0, rm_y),
        vec2(words.size().x + 8.0, MIN_HIT),
    );
    let resp = hit(ui, rm, Id::unique(("rm", d, c.id)));
    name_button(&resp, &format!("{label} {title}"));
    painter.galley(
        pos2(rm.left(), rm.center().y - words.size().y / 2.0),
        words,
        if resp.hovered() {
            t.bad_ink
        } else {
            t.bad_ink.gamma_multiply(0.75)
        },
    );
    if resp.clicked() && !resp.double_clicked() {
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
        CardId::Node(i) => {
            let n = &g.dev.nodes[i];
            rd.nodes
                .iter()
                .find(|(id, _)| *id == n.id)
                .and_then(|(_, v)| v.get(usize::from(row)))
                .copied()
                .unwrap_or(0.0)
        }
    }
}

/// Colour slot of a source: the number in its id (`s3` is slot 2), so adding or removing other
/// sources never recolours it. An id without a number takes its position.
pub fn source_slot(id: &str, index: usize) -> usize {
    id.strip_prefix('s')
        .and_then(|n| n.parse::<usize>().ok())
        .map_or(index, |n| n.saturating_sub(1))
}

/// The identity colour of `dev.sources[i]`: the dot before its card title. Cables and jacks
/// take per-cable colours instead (`cables::assign`).
fn source_color(dev: &VirtualDevice, i: usize) -> Color32 {
    cable_color(source_slot(&dev.sources[i].id, i))
}

/// The source whose signal passes through a port: its own for a source port, else whatever feeds
/// the port first, followed back through the output channels and mixer rows. `None` for a port
/// nothing reaches (a mixer with no source behind it).
fn feeding_source(all: &[Link], p: Port) -> Option<usize> {
    match p {
        Port::Src { src, .. } => Some(src),
        Port::OutOut { ch } => feeding_source(all, Port::OutIn { ch }),
        Port::NodeOut { node, ch } => feeding_source(all, Port::NodeIn { node, ch }),
        Port::OutIn { .. } | Port::Mon { .. } | Port::NodeIn { .. } => all
            .iter()
            .find(|l| l.to == p)
            .and_then(|l| feeding_source(all, l.from)),
    }
}

/// The output channel a port stands for, which colours its ring while it is open: a device
/// channel's own number, a monitor row's own number. Source and mixer ports have none and stay
/// neutral.
fn port_channel(p: Port) -> Option<u16> {
    match p {
        Port::Src { .. } | Port::NodeIn { .. } | Port::NodeOut { .. } => None,
        Port::OutIn { ch } | Port::OutOut { ch } | Port::Mon { ch, .. } => Some(ch),
    }
}

/// Look of the jack at `p`, from the cables that touch it (`slots[n]` is the palette slot of
/// `all[n]`). One cable: solid in its colour. Two or more: equal arcs of the first three, in
/// cable order, with the true count for the "+N". None: a neutral ring for a source jack, the
/// channel's own colour for an output or monitor jack.
fn jack_plug(all: &[Link], slots: &[u8], p: Port, t: &Tokens) -> Plug {
    let mut first = [t.muted; 3];
    let mut n = 0;
    for (_, &s) in all
        .iter()
        .zip(slots)
        .filter(|(l, _)| l.from == p || l.to == p)
    {
        if let Some(c) = first.get_mut(n) {
            *c = t.channel(usize::from(s));
        }
        n += 1;
    }
    if n == 0 {
        return Plug::Open(port_channel(p).map(|ch| t.channel(usize::from(ch))));
    }
    Plug::of(first, n)
}

/// "Safari, channel 1 (L) to Channel 3 (L)": what a cable joins, for its tooltip.
fn link_tip(g: &GraphCtx, l: Link) -> String {
    let node = |node: usize, ch: u16| {
        let n = &g.dev.nodes[node];
        format!("{}, channel {}", node_name(n), ch_label(ch, n.channels))
    };
    let from = match l.from {
        Port::Src { src, ch } => {
            let s = &g.dev.sources[src];
            let name = source_title(g.hw, g.apps, g.installed, s).0;
            format!("{name}, channel {}", ch_label(ch, s.channels))
        }
        Port::OutOut { ch } => format!("Output channel {}", ch_label(ch, 0)),
        Port::NodeOut { node: n, ch } => node(n, ch),
        Port::OutIn { .. } | Port::Mon { .. } | Port::NodeIn { .. } => String::new(),
    };
    let to = match l.to {
        Port::OutIn { ch } => format!("Channel {}", ch_label(ch, 0)),
        Port::Mon { mon, ch } => {
            let m = &g.dev.monitors[mon];
            format!(
                "{}, channel {}",
                monitor_title(m, g.hw),
                ch_label(ch, mon_rows(m, g.hw))
            )
        }
        Port::NodeIn { node: n, ch } => node(n, ch),
        Port::Src { .. } | Port::OutOut { .. } | Port::NodeOut { .. } => String::new(),
    };
    format!("{from} to {to}")
}

const POP_SECS: f64 = 0.12;

/// Scale of a jack `age` seconds after its wire was plugged in: swells to 1.5x and settles back.
pub fn pop_scale(age: f64) -> f32 {
    if !(0.0..POP_SECS).contains(&age) {
        return 1.0;
    }
    1.0 + 0.5 * (std::f64::consts::PI * age / POP_SECS).sin() as f32
}

/// Accessible name of a port, e.g. "Safari channel 1 output".
fn port_label(g: &GraphCtx, p: Port) -> String {
    match p {
        Port::Src { src, ch } => format!(
            "{} channel {} output",
            source_title(g.hw, g.apps, g.installed, &g.dev.sources[src]).0,
            ch + 1
        ),
        Port::OutIn { ch } => format!("Output channel {} input", ch + 1),
        Port::OutOut { ch } => format!("Output channel {} output", ch + 1),
        Port::Mon { mon, ch } => format!(
            "{} channel {} input",
            monitor_title(&g.dev.monitors[mon], g.hw),
            ch + 1
        ),
        // "Mixer input 1 (L)".
        Port::NodeIn { node, ch } | Port::NodeOut { node, ch } => {
            let n = &g.dev.nodes[node];
            let side = if p.is_output() { "output" } else { "input" };
            format!("{} {side} {}", node_name(n), ch_label(ch, n.channels))
        }
    }
}

/// A cable: a darker outline one pixel wider on each side under the cable colour.
fn draw_cable(p: &egui::Painter, pts: [Pos2; 4], color: Color32, width: f32) {
    for (w, c) in [(width + 2.0, darken(color)), (width, color)] {
        p.add(CubicBezierShape::from_points_stroke(
            pts,
            false,
            Color32::TRANSPARENT,
            Stroke::new(w, c),
        ));
    }
}

/// A device cable's trim control: a chip on its middle, always there so the keyboard can reach
/// it. It shows the cable's gain and polarity when they are set, and opens the trim popover.
struct Trim {
    mid: Pos2,
    /// Where the x sits relative to `mid` while the cable is hot, in a slot beside the chip that
    /// nothing else is in; `None` when every slot is taken.
    x_off: Option<Vec2>,
    chip: Rect,
    /// "-6 dB" and "inv", one per line, only while the cable is trimmed.
    badge: Vec<String>,
    resp: Response,
    open: bool,
}

const CHIP_H: f32 = 16.0;
/// Pitch of the chip's lines when it carries both a gain and a polarity.
const LINE_H: f32 = 12.0;
/// How far the hovered cable's x sits from its chip: clear of the chip's own `MIN_HIT` target.
const X_LIFT: f32 = 28.0;
/// Slots for the x beside a chip, best first: straight above or below it, then diagonally off
/// it (clear of neighbours' chips in a dense stack of straight cables).
const SLOTS: [Vec2; 6] = [
    vec2(0.0, -X_LIFT),
    vec2(0.0, X_LIFT),
    vec2(-32.0, -X_LIFT),
    vec2(32.0, -X_LIFT),
    vec2(-32.0, X_LIFT),
    vec2(32.0, X_LIFT),
];
/// Along the cable, where the chip may sit: the middle, else as near to it as keeps it off
/// the chips of the cables beside it.
const CHIP_AT: [f32; 7] = [0.5, 0.42, 0.58, 0.34, 0.66, 0.26, 0.74];
/// Width of the trim popover.
const TRIM_W: f32 = 280.0;
/// Lowest gain the trim slider reaches; a typed value goes lower.
const TRIM_MIN_DB: f32 = -40.0;

/// The `MIN_HIT` target of a small glyph button at `at`.
fn zone(at: Pos2) -> Rect {
    hit_rect(Rect::from_center_size(at, vec2(8.0, 8.0)))
}

impl Trim {
    fn x_at(&self) -> Option<Pos2> {
        self.x_off.map(|o| self.mid + o)
    }

    /// The chip with its x while the cable is hot: one capsule, so the pointer can travel from
    /// the chip to the x without the cable going cold.
    fn capsule(&self) -> Rect {
        let chip = Rect::from_center_size(
            self.mid,
            vec2(
                self.chip.width().max(MIN_HIT) + 4.0,
                self.chip.height() + 6.0,
            ),
        );
        self.x_at()
            .into_iter()
            .fold(chip, |r, at| r.union(zone(at)))
    }

    /// The chip, and when the cable is hot the capsule joining it to its x.
    fn paint(&self, p: &egui::Painter, t: &Tokens, hot: bool) {
        if hot {
            p.rect(
                self.capsule(),
                CornerRadius::same(radius::STRIP),
                t.raised,
                Stroke::new(1.0, t.hairline),
                StrokeKind::Inside,
            );
        }
        let aimed = self.resp.hovered() || self.resp.has_focus();
        if self.badge.is_empty() {
            if hot {
                trim_glyph(p, self.mid, if aimed { t.ink } else { t.muted });
            }
        } else {
            if !hot {
                p.rect(
                    self.chip,
                    CornerRadius::same(8),
                    t.raised,
                    Stroke::new(1.0, if aimed { t.muted } else { t.hairline }),
                    StrokeKind::Inside,
                );
            }
            let n = self.badge.len() as f32;
            for (i, line) in self.badge.iter().enumerate() {
                let dy = (i as f32 - (n - 1.0) / 2.0) * LINE_H;
                p.text(
                    self.mid + vec2(0.0, dy),
                    Align2::CENTER_CENTER,
                    line,
                    num(10.0),
                    t.ink,
                );
            }
        }
        if self.resp.has_focus() {
            p.rect_stroke(
                self.chip.expand(2.0),
                CornerRadius::same(10),
                Stroke::new(2.0, t.focus),
                StrokeKind::Outside,
            );
        }
    }
}

/// A dB value as the chip and the readouts print it: "+3", "-6", "-9.5".
fn db_text(db: f32) -> String {
    let r = (db * 10.0).round() / 10.0 + 0.0; // + 0.0 turns -0.0 into 0.0
    if r.fract() == 0.0 {
        format!("{r:+.0}")
    } else {
        format!("{r:+.1}")
    }
}

/// What the chip prints for a trimmed cable: the gain, "inv" for a flipped polarity, or both on
/// two lines (one line would not fit between the cards at the minimum window).
fn badge_lines(w: &Wire) -> Vec<String> {
    let gain = (w.gain_db != 0.0).then(|| format!("{} dB", db_text(w.gain_db)));
    gain.into_iter()
        .chain(w.invert.then(|| "inv".to_string()))
        .collect()
}

/// Point of the cable `pts` at parameter `t`.
fn wire_at(pts: [Pos2; 4], t: f32) -> Pos2 {
    CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, Stroke::NONE).sample(t)
}

/// Whether two targets get in each other's way. Two rows are 24 apart and a target 28 tall, so
/// the 4 px two straight neighbours share is allowed (they touch once shrunk; touching is not
/// crowding).
fn crowd(a: Rect, b: Rect) -> bool {
    let i = a.shrink(2.0).intersect(b.shrink(2.0));
    i.width() > 0.0 && i.height() > 0.0
}

/// The chip of every source cable (`None` for the others, same order as `all`), and its popover.
/// Cables of one source channel to both sides of a pair run close together, so a chip slides
/// along its cable until its target is clear of the chips placed before it.
fn trim_controls(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    edits: &mut Vec<Edit>,
    layout: &Layout,
    all: &[Link],
) -> Vec<Option<Trim>> {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    // Cards (module cards and the Processing column's empty state too) and jacks take the
    // clicks at the cables' ends and over the cards: chips keep off all of them where they can.
    let jacks = layout
        .ports()
        .into_iter()
        .filter_map(|p| layout.port_pos(p))
        .map(|at| Rect::from_center_size(at, vec2(MIN_HIT, ROW_PITCH)));
    // A chip never goes on a card's body, where its cable runs underneath (in a 40 px gutter it
    // may overhang the edges, which hold only jacks); it keeps off the jacks and the chips placed
    // before it where it can.
    let cards: Vec<Rect> = layout
        .cards
        .iter()
        .map(|c| c.rect)
        .chain(layout.proc_empty)
        .map(|r| r.shrink2(vec2(12.0, 0.0)))
        .collect();
    let jacks: Vec<Rect> = jacks.collect();
    let mut chips: Vec<Rect> = Vec::new();
    let mut trims: Vec<Option<Trim>> = all
        .iter()
        .map(|l| {
            let w = wire_of(dev, *l)?;
            let (a, b) = (layout.port_pos(l.from)?, layout.port_pos(l.to)?);
            let pts = wire_points(a, b);
            let badge = badge_lines(w);
            let width = badge.iter().fold(20.0_f32, |m, line| {
                let text_w = ui
                    .painter()
                    .layout_no_wrap(line.clone(), num(10.0), t.ink)
                    .size()
                    .x;
                m.max(text_w + 12.0)
            });
            let size = vec2(
                width,
                CHIP_H + LINE_H * badge.len().saturating_sub(1) as f32,
            );
            let at = |t: f32| Rect::from_center_size(wire_at(pts, t), size);
            // `CHIP_AT` first, then every 1 % out to the ends (the short runs into a module):
            // the first spot off the cards and clear, else the least crowded one off the cards,
            // covering no other chip and with its centre off the jacks where it can (a jack wins
            // the click where they overlap, so a click on the chip's middle still lands).
            let fine =
                (1..=45u8).flat_map(|k| [0.5 - f32::from(k) / 100.0, 0.5 + f32::from(k) / 100.0]);
            let t_at = CHIP_AT
                .into_iter()
                .chain(fine)
                .min_by_key(|&t| {
                    let target = hit_rect(at(t));
                    let on_card = cards.iter().any(|r| r.intersects(at(t)));
                    let near = |rs: &[Rect]| rs.iter().filter(|r| crowd(**r, target)).count();
                    let on_jack = jacks.iter().any(|r| r.contains(target.center()));
                    (on_card, near(&chips), on_jack, near(&jacks))
                })
                .unwrap_or(CHIP_AT[0]);
            let chip = at(t_at);
            chips.push(hit_rect(chip));
            let resp = hit(ui, chip, Id::unique(("trim", d, l.from, l.to)));
            let invert = if w.invert { ", inverted" } else { "" };
            let gain = if w.gain_db != 0.0 {
                format!(", {} dB", db_text(w.gain_db))
            } else {
                String::new()
            };
            name_button(
                &resp,
                &format!("Cable trim, {}{gain}{invert}", link_tip(g, *l)),
            );
            Some(Trim {
                mid: chip.center(),
                x_off: None,
                chip,
                badge,
                resp,
                open: false,
            })
        })
        .collect();
    // The x goes in the first slot beside its chip that nothing else is in, off the cards. Where
    // neighbours leave no slot free (a dense stack of straight cables) it does not show, so a
    // click never lands on a neighbour's chip: the cable still comes out by its input jack.
    for tr in trims.iter_mut().flatten() {
        let mid = tr.mid;
        // The cable's own chip is in `chips` once and clear of every slot by construction.
        tr.x_off = SLOTS.into_iter().find(|&o| {
            !jacks
                .iter()
                .chain(&chips)
                .chain(&cards)
                .any(|r| crowd(*r, zone(mid + o)))
        });
    }
    for (l, tr) in all.iter().zip(trims.iter_mut()) {
        let Some(tr) = tr else { continue };
        Popup::menu(&tr.resp)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| trim_popover(ui, g, st, edits, *l));
        tr.open = Popup::is_id_open(ui.ctx(), Popup::default_response_id(&tr.resp));
    }
    trims
}

/// "Two", "Three" ... for the fan-in hint; a plain count past five.
fn count_word(n: usize) -> String {
    match n {
        2 => "Two".into(),
        3 => "Three".into(),
        4 => "Four".into(),
        5 => "Five".into(),
        n => n.to_string(),
    }
}

/// The popover of one source cable: gain, polarity, pan of an L/R pair, and the fan-in hint.
fn trim_popover(ui: &mut Ui, g: &GraphCtx, st: &mut GraphState, edits: &mut Vec<Edit>, l: Link) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let Some(w) = wire_of(dev, l) else {
        ui.close();
        return;
    };
    ui.set_width(TRIM_W);
    ui.label(
        egui::RichText::new(link_tip(g, l))
            .size(11.0)
            .color(t.muted),
    );
    let row = |ui: &mut Ui| {
        ui.allocate_exact_size(vec2(TRIM_W, MIN_HIT), Sense::hover())
            .0
    };
    let label_at = |ui: &Ui, r: Rect, text: &str| {
        ui.painter().text(
            pos2(r.left(), r.center().y),
            Align2::LEFT_CENTER,
            text,
            caption(),
            t.muted,
        );
    };

    // Gain: slider and a field to type a value in.
    let r = row(ui);
    label_at(ui, r, "Gain");
    let before = w.gain_db.clamp(TRIM_MIN_DB, sr_core::config::MAX_DB);
    let mut v = before;
    let rail = Rect::from_x_y_ranges((r.left() + 40.0)..=(r.right() - 84.0), r.y_range());
    slider(
        ui,
        rail,
        Id::unique(("trim_gain", d, l.from, l.to)),
        "Cable gain, dB",
        &mut v,
        TRIM_MIN_DB..=sr_core::config::MAX_DB,
        0.0,
        t,
    );
    if (v - before).abs() > 1e-4 {
        edits.push(Edit::WireGain(d, l, (v * 10.0).round() / 10.0));
    }
    let id = Id::unique(("trim_db", d, l.from, l.to));
    if !ui.memory(|m| m.has_focus(id)) {
        st.trim_text = db_text(w.gain_db).trim_start_matches('+').to_string();
    }
    let field = Rect::from_x_y_ranges((r.right() - 76.0)..=(r.right() - 24.0), r.y_range())
        .shrink2(vec2(0.0, 2.0));
    ui.painter().rect(
        field,
        CornerRadius::same(radius::INPUT),
        t.canvas,
        Stroke::new(1.0, t.hairline),
        StrokeKind::Inside,
    );
    let te = ui.put(
        field,
        egui::TextEdit::singleline(&mut st.trim_text)
            .id(id)
            // egui ignores `margin` once a frame is set: the frame carries it.
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(6, 4)))
            .align(Align2::RIGHT_CENTER),
    );
    name_widget(ui, &te, "Cable gain value, dB");
    if te.has_focus() {
        ui.painter().rect_stroke(
            te.rect,
            CornerRadius::same(radius::INPUT),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    // Escape gives the keyboard back without committing; a value past the range is held to it, not
    // sent on to fail validation and roll the whole frame back.
    if te.lost_focus()
        && !ui.input(|i| i.key_pressed(egui::Key::Escape))
        && let Ok(typed) = st.trim_text.trim().parse::<f32>()
        && typed.is_finite()
    {
        let typed = typed.clamp(crate::model::MIN_DB, sr_core::config::MAX_DB);
        if (typed - w.gain_db).abs() > 1e-4 {
            edits.push(Edit::WireGain(d, l, typed));
        }
    }
    ui.painter().text(
        pos2(r.right(), r.center().y),
        Align2::RIGHT_CENTER,
        "dB",
        caption(),
        t.muted,
    );

    // Polarity.
    let r = row(ui);
    label_at(ui, r, "Flip polarity");
    let sw = Rect::from_center_size(pos2(r.right() - 16.0, r.center().y), vec2(32.0, 18.0));
    let on = switch(
        ui,
        sw,
        r,
        Id::unique(("trim_invert", d, l.from, l.to)),
        w.invert,
        t,
        "Flip polarity",
    );
    if on != w.invert {
        edits.push(Edit::WireInvert(d, l, on));
    }

    // Pan, when this cable and its partner carry one source channel to both sides of a pair.
    if let Some(pan) = pan_of(dev, l) {
        let r = row(ui);
        label_at(ui, r, "Pan");
        let before = pan * 100.0;
        let mut v = before;
        let rail = Rect::from_x_y_ranges((r.left() + 40.0)..=(r.right() - 64.0), r.y_range());
        slider(
            ui,
            rail,
            Id::unique(("trim_pan", d, l.from, l.to)),
            "Cable pan",
            &mut v,
            -100.0..=100.0,
            0.0,
            t,
        );
        if (v - before).abs() > 1e-3 {
            edits.push(Edit::WirePan(d, l, v / 100.0));
        }
        let shown = if (v - before).abs() > 1e-3 { v } else { before };
        let text = match shown.round() as i32 {
            0 => "Centre".to_string(),
            n if n < 0 => format!("L {}", -n),
            n => format!("R {n}"),
        };
        ui.painter().text(
            pos2(r.right(), r.center().y),
            Align2::RIGHT_CENTER,
            text,
            num(11.0),
            t.muted,
        );
    }

    // Cables that meet in one output channel add up.
    let out_ch = match l.to {
        Port::OutIn { ch } => Some(ch),
        _ => None,
    };
    let n = out_ch.map_or(0, |ch| fan_in(dev, ch));
    if let Some(out_ch) = out_ch
        && n >= 2
    {
        ui.separator();
        let each = balance_db(n);
        let text = if balanced(dev, out_ch) {
            format!(
                "The {n} cables into this channel are balanced, each trimmed by {} dB.",
                db_text(each)
            )
        } else {
            format!(
                "{} cables add up ({} dB). Trim each by {} dB.",
                count_word(n),
                db_text(-each),
                db_text(each)
            )
        };
        ui.add(egui::Label::new(egui::RichText::new(text).size(11.5).color(t.ink)).wrap());
        if !balanced(dev, out_ch) {
            let r = row(ui);
            let id = Id::unique(("trim_balance", d, out_ch));
            if text_button(ui, t, r.min, id, "Balance cables", "Balance cables").clicked() {
                edits.push(Edit::BalanceCables(d, out_ch));
                // Done: the toast offers Undo, and a click there would only close this first.
                ui.close();
            }
        }
    }
}

/// A quiet outlined button that says `text` (and `name` to a screen reader), at `at` (left top),
/// one row tall.
fn text_button(ui: &mut Ui, t: &Tokens, at: Pos2, id: Id, text: &str, name: &str) -> Response {
    let words = ui.painter().layout_no_wrap(text.to_string(), body(), t.ink);
    let btn = Rect::from_min_size(at, vec2(words.size().x + 28.0, MIN_HIT)).shrink2(vec2(0.0, 2.0));
    let resp = hit(ui, btn, id);
    name_button(&resp, name);
    ui.painter().rect(
        btn,
        CornerRadius::same(radius::INPUT),
        if resp.hovered() {
            t.muted.gamma_multiply(0.12)
        } else {
            Color32::TRANSPARENT
        },
        Stroke::new(
            if resp.has_focus() { 2.0 } else { 1.0 },
            if resp.has_focus() {
                t.focus
            } else {
                t.hairline
            },
        ),
        StrokeKind::Inside,
    );
    ui.painter()
        .galley(btn.center() - words.size() / 2.0, words, t.ink);
    resp
}

/// What the limiter of `node` took off in the last reading, in dB (<= 0). Looked up by id: the
/// reading is the engine's, and a node inserted ahead of this one since then shifts the positions.
fn limiter_db(reading: Option<&MeterReading>, node: &Node) -> f32 {
    let red = reading
        .and_then(|rd| rd.limiter_reduction.iter().find(|(id, _)| *id == node.id))
        .map_or(0.0, |(_, r)| *r)
        .clamp(0.0, 0.999);
    20.0 * (1.0 - red).log10()
}

/// Whether a node shows the limiter line on its strip.
fn has_limiter(n: &Node) -> bool {
    n.fx.iter().any(|f| matches!(f, Fx::Limiter { .. }))
}

/// A number as the fields and readouts print it: whole when it is whole, else one decimal.
fn short_num(x: f32) -> String {
    let r = (x * 10.0).round() / 10.0 + 0.0; // + 0.0 turns -0.0 into 0.0
    if r.fract() == 0.0 {
        format!("{r:.0}")
    } else {
        format!("{r:.1}")
    }
}

/// What an effect's chip says: its name, and for a delay the time it adds.
fn fx_chip(fx: &Fx) -> String {
    match fx {
        Fx::Delay { ms } => format!("Delay {} ms", short_num(*ms)),
        _ => fx_name(fx).to_string(),
    }
}

/// What a module works on, for its name: the output channels it feeds ("Channels 1 & 2").
fn module_scope(g: &GraphCtx, n: usize) -> String {
    let mut outs: Vec<u16> = links(g.dev)
        .iter()
        .filter_map(|l| match (l.from, l.to) {
            (Port::NodeOut { node, .. }, Port::OutIn { ch }) if node == n => Some(ch + 1),
            _ => None,
        })
        .collect();
    outs.sort_unstable();
    outs.dedup();
    match outs.as_slice() {
        [] => "not connected to an output".into(),
        [one] => format!("processing Channel {one}"),
        [rest @ .., last] => {
            let rest: Vec<String> = rest.iter().map(u16::to_string).collect();
            format!("processing Channels {} & {last}", rest.join(", "))
        }
    }
}

/// A module card in the Processing column: the title (opens the inspector), the channel rows
/// with their jacks on the edges, one standing meter per channel, the limiter line, the fader,
/// the mute and one chip per effect (each opens the inspector too).
fn strip(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    edits: &mut Vec<Edit>,
    c: &CardLayout,
    n: usize,
    dt: f32,
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let node = &dev.nodes[n];
    let name = node_name(node);
    let r = c.rect;
    let dim = if node.muted { 0.45 } else { 1.0 };
    let painter = ui.painter().clone();
    let popover = |st: &mut GraphState, edits: &mut Vec<Edit>, anchor: &Response| {
        Popup::menu(anchor)
            .close_behavior(PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| inspector(ui, g, st, edits, n));
    };

    // Title: the button that opens the inspector.
    let head = Rect::from_min_max(
        pos2(r.left() + 4.0, r.top() + 3.0),
        pos2(r.right() - 4.0, r.top() + HEAD_H - 3.0),
    );
    let title = hit(ui, head, Id::unique(("strip_title", d, n)));
    name_button(&title, &format!("{name} module, {}", module_scope(g, n)));
    halo(ui, &title, t);
    text_left(
        ui,
        head.left() + 6.0,
        head.center().y,
        name,
        label(),
        t.ink,
        head.width() - 24.0,
    );
    menu_hint(
        &painter,
        pos2(head.right() - 9.0, head.center().y),
        hint_color(&title, t),
    );
    if title.has_focus() {
        painter.rect_stroke(
            head,
            CornerRadius::same(radius::INPUT),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    popover(st, edits, &title);
    painter.hline(r.x_range(), r.top() + HEAD_H, Stroke::new(1.0, t.hairline));

    // Channel rows: the jacks sit on the edges, the row's name between them.
    for row in 0..c.rows {
        painter.text(
            pos2(r.center().x, Layout::row_y(c, row)),
            Align2::CENTER_CENTER,
            ch_label(row, node.channels),
            num(10.0),
            t.muted.gamma_multiply(dim),
        );
    }

    // One standing ladder per channel, after the fader.
    let top = r.top() + HEAD_H + ROW_PITCH * f32::from(c.rows);
    let block = Rect::from_min_max(
        pos2(r.left() + 12.0, top + 6.0),
        pos2(r.right() - 12.0, top + STRIP_METER_H - 6.0),
    );
    let k = f32::from(c.rows.max(1));
    let w = ((block.width() - 3.0 * (k - 1.0)) / k).min(8.0);
    let x0 = block.center().x - (k * w + 3.0 * (k - 1.0)) / 2.0;
    let active = dev.enabled && !node.muted;
    for row in 0..c.rows {
        let lin = if active { row_level(g, c.id, row) } else { 0.0 };
        let lv = level(lin);
        let pk = st.peaks.entry((c.id, row)).or_default().update(lv, dt);
        if pk > lv {
            ui.request_repaint_after(std::time::Duration::from_millis(33));
        }
        let at = pos2(x0 + f32::from(row) * (w + 3.0), block.top());
        meter_v(
            &painter,
            Rect::from_min_size(at, vec2(w, block.height())),
            lv,
            pk,
            t,
            dim,
        );
    }
    let mut y = top + STRIP_METER_H;

    // How much the limiter takes off, in words (never colour alone).
    if has_limiter(node) {
        let db = limiter_db(g.reading, node);
        let (text, ink) = if active && db < -0.05 {
            (format!("Limit {} dB", db_text(db)), t.warn_ink)
        } else {
            ("Limit 0 dB".to_string(), t.muted)
        };
        painter.text(
            pos2(r.center().x, y + STRIP_LIMIT_H / 2.0),
            Align2::CENTER_CENTER,
            text,
            caption(),
            ink,
        );
        y += STRIP_LIMIT_H;
    }

    // Fader.
    let fr = Rect::from_min_size(
        pos2(r.left() + 8.0, y + 2.0),
        vec2(r.width() - 16.0, MIN_HIT),
    );
    let before = node.gain_db.clamp(-60.0, 12.0);
    let mut v = before;
    slider(
        ui,
        fr,
        Id::unique(("strip_vol", d, n)),
        &format!("{name} volume"),
        &mut v,
        -60.0..=12.0,
        0.0,
        t,
    );
    let moved = (v - before).abs() > 1e-4;
    if moved {
        edits.push(Edit::NodeVolume(d, n, (v * 10.0).round() / 10.0));
    }
    y += STRIP_ROW_H;

    // Mute, which says its state in words, and the fader's dB.
    let mr = Rect::from_min_size(pos2(r.left() + 8.0, y + 6.0), vec2(42.0, 20.0));
    let mute = hit(ui, mr, Id::unique(("strip_mute", d, n)));
    let muted = node.muted;
    mute.widget_info(|| {
        WidgetInfo::selected(egui::Role::CheckBox, true, muted, format!("Mute {name}"))
    });
    let pill = CornerRadius::same(radius::PILL);
    let (fill, ink) = if muted {
        (t.selected, t.warn_ink)
    } else if mute.hovered() {
        (t.muted.gamma_multiply(0.12), t.ink)
    } else {
        (Color32::TRANSPARENT, t.muted)
    };
    painter.rect(
        mr,
        pill,
        fill,
        Stroke::new(1.0, if muted { t.warn_ink } else { t.hairline }),
        StrokeKind::Inside,
    );
    painter.text(
        mr.center(),
        Align2::CENTER_CENTER,
        if muted { "Muted" } else { "Mute" },
        caption(),
        ink,
    );
    if mute.has_focus() {
        painter.rect_stroke(mr, pill, Stroke::new(2.0, t.focus), StrokeKind::Outside);
    }
    if mute.clicked() {
        edits.push(Edit::NodeMute(d, n, !muted));
    }
    painter.text(
        pos2(r.right() - 8.0, mr.center().y),
        Align2::RIGHT_CENTER,
        db_text(if moved { v } else { node.gain_db }),
        num(10.0),
        t.muted,
    );
    y += STRIP_ROW_H;

    // One chip per effect, in the order they run.
    for (i, fx) in node.fx.iter().enumerate() {
        let cr = Rect::from_min_size(
            pos2(r.left() + 8.0, y + 4.0),
            vec2(r.width() - 16.0, STRIP_ROW_H - 8.0),
        );
        let chip = hit(ui, cr, Id::unique(("strip_fx", d, n, i)));
        name_button(&chip, &format!("{} on {name}", fx_chip(fx)));
        painter.rect(
            cr,
            pill,
            t.canvas,
            Stroke::new(1.0, if chip.hovered() { t.muted } else { t.hairline }),
            StrokeKind::Inside,
        );
        let room = cr.width() - 16.0;
        let mut text = fx_chip(fx);
        // "Delay 200 ms" overflows a slim strip: the chip keeps the time, its name says the rest.
        if let Fx::Delay { ms } = fx
            && ui
                .painter()
                .layout_no_wrap(text.clone(), caption(), t.ink)
                .size()
                .x
                > room
        {
            text = format!("+{} ms", short_num(*ms));
        }
        text_left(
            ui,
            cr.left() + 10.0,
            cr.center().y,
            &text,
            caption(),
            t.ink.gamma_multiply(dim),
            room,
        );
        if chip.has_focus() {
            painter.rect_stroke(cr, pill, Stroke::new(2.0, t.focus), StrokeKind::Outside);
        }
        popover(st, edits, &chip);
        y += STRIP_ROW_H;
    }
}

/// Width of the mixer inspector.
const INSPECTOR_W: f32 = 300.0;

/// A text field in `r` named `name` for screen readers, showing `value` until it is typed in;
/// returns what was typed when the field lets go of the keyboard. One field types at a time, so
/// one buffer serves them all.
#[allow(clippy::too_many_arguments)]
fn field(
    ui: &mut Ui,
    st: &mut GraphState,
    t: &Tokens,
    r: Rect,
    id: Id,
    name: &str,
    value: String,
    align: Align2,
) -> Option<String> {
    let had = ui.memory(|m| m.has_focus(id));
    let mut text = if had {
        std::mem::take(&mut st.field_text)
    } else {
        value
    };
    ui.painter().rect(
        r,
        CornerRadius::same(radius::INPUT),
        t.canvas,
        Stroke::new(1.0, t.hairline),
        StrokeKind::Inside,
    );
    let te = ui.put(
        r,
        egui::TextEdit::singleline(&mut text)
            .id(id)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(6, 4)))
            .align(align),
    );
    name_widget(ui, &te, name);
    if te.has_focus() {
        ui.painter().rect_stroke(
            te.rect,
            CornerRadius::same(radius::INPUT),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    // Escape cancels: the typing is dropped, not committed.
    let done =
        (te.lost_focus() && !ui.input(|i| i.key_pressed(egui::Key::Escape))).then(|| text.clone());
    if te.has_focus() {
        st.field_text = text;
    }
    done
}

/// One effect parameter: what it is, a slider, a field to type a value in, and its unit.
struct Param {
    /// What the row says ("Low gain") and what a screen reader hears ("Low shelf gain, dB").
    label: &'static str,
    name: &'static str,
    unit: &'static str,
    value: f32,
    lo: f32,
    hi: f32,
    reset: f32,
}

/// Draws `p`'s row; returns its new value when the slider moved or a value was typed (clamped
/// to the range, as `Config::validate` wants it).
fn param_row(ui: &mut Ui, st: &mut GraphState, t: &Tokens, id: Id, p: &Param) -> Option<f32> {
    let r = ui
        .allocate_exact_size(vec2(INSPECTOR_W, MIN_HIT), Sense::hover())
        .0;
    ui.painter().text(
        pos2(r.left(), r.center().y),
        Align2::LEFT_CENTER,
        p.label,
        caption(),
        t.muted,
    );
    let before = p.value.clamp(p.lo, p.hi);
    let mut v = before;
    let rail = Rect::from_x_y_ranges((r.left() + 72.0)..=(r.right() - 88.0), r.y_range());
    slider(
        ui,
        rail,
        id.with("slider"),
        &format!("{}, {}", p.name, p.unit),
        &mut v,
        p.lo..=p.hi,
        p.reset,
        t,
    );
    // Whole hertz, tenths of the rest.
    let round = |x: f32| {
        if p.unit == "Hz" {
            x.round()
        } else {
            (x * 10.0).round() / 10.0
        }
    };
    let mut out = ((v - before).abs() > 1e-4).then(|| round(v));
    let fr = Rect::from_x_y_ranges((r.right() - 80.0)..=(r.right() - 26.0), r.y_range())
        .shrink2(vec2(0.0, 2.0));
    let name = format!("{} value, {}", p.name, p.unit);
    let typed = field(
        ui,
        st,
        t,
        fr,
        id.with("field"),
        &name,
        short_num(p.value),
        Align2::RIGHT_CENTER,
    );
    if let Some(x) = typed.and_then(|s| s.trim().parse::<f32>().ok())
        && x.is_finite()
        && (x - p.value).abs() > 1e-4
    {
        out = Some(round(x.clamp(p.lo, p.hi)));
    }
    ui.painter().text(
        pos2(r.right(), r.center().y),
        Align2::RIGHT_CENTER,
        p.unit,
        caption(),
        t.muted,
    );
    out
}

/// The parameters of an effect, and the effect rebuilt with parameter `k` set to `x`.
fn fx_params(fx: &Fx) -> Vec<Param> {
    let p = |label, name, unit, value, lo, hi, reset| Param {
        label,
        name,
        unit,
        value,
        lo,
        hi,
        reset,
    };
    match *fx {
        Fx::Mono => Vec::new(),
        Fx::Eq {
            low_hz,
            low_db,
            high_hz,
            high_db,
        } => vec![
            p(
                "Low shelf",
                "Low shelf frequency",
                "Hz",
                low_hz,
                20.0,
                2000.0,
                120.0,
            ),
            p("Low gain", "Low shelf gain", "dB", low_db, -18.0, 18.0, 0.0),
            p(
                "High shelf",
                "High shelf frequency",
                "Hz",
                high_hz,
                1000.0,
                20000.0,
                8000.0,
            ),
            p(
                "High gain",
                "High shelf gain",
                "dB",
                high_db,
                -18.0,
                18.0,
                0.0,
            ),
        ],
        Fx::Limiter {
            ceiling_db,
            knee_db,
        } => vec![
            p(
                "Ceiling",
                "Limiter ceiling",
                "dB",
                ceiling_db,
                -24.0,
                0.0,
                -1.0,
            ),
            p("Knee", "Limiter knee", "dB", knee_db, 0.0, 12.0, 6.0),
        ],
        Fx::Delay { ms } => vec![p(
            "Time",
            "Delay time",
            "ms",
            ms,
            0.0,
            sr_core::config::MAX_DELAY_MS,
            0.0,
        )],
    }
}

fn with_param(fx: &Fx, k: usize, x: f32) -> Fx {
    let mut fx = fx.clone();
    match &mut fx {
        Fx::Mono => {}
        Fx::Eq {
            low_hz,
            low_db,
            high_hz,
            high_db,
        } => *[low_hz, low_db, high_hz, high_db][k] = x,
        Fx::Limiter {
            ceiling_db,
            knee_db,
        } => *[ceiling_db, knee_db][k] = x,
        Fx::Delay { ms } => *ms = x,
    }
    fx
}

/// What an effect does, in one line under its heading.
fn fx_note(fx: &Fx) -> String {
    match fx {
        Fx::Mono => "Folds each left and right pair to mono.".into(),
        Fx::Eq { .. } => "Two shelves: lift or cut the lows and the highs.".into(),
        Fx::Limiter { .. } => "Soft clip with no look-ahead: it adds no delay.".into(),
        Fx::Delay { ms } => format!("Adds {} ms", short_num(*ms)),
    }
}

/// A small arrow button that moves an effect up or down the list.
fn arrow_button(ui: &mut Ui, t: &Tokens, at: Pos2, id: Id, up: bool, name: &str) -> Response {
    let resp = hit(ui, Rect::from_center_size(at, vec2(10.0, 10.0)), id);
    name_button(&resp, name);
    halo(ui, &resp, t);
    let c = if resp.hovered() { t.ink } else { t.muted };
    let s = if up { -1.0 } else { 1.0 };
    let tip = at + vec2(0.0, 3.5 * s);
    for side in [-1.0, 1.0] {
        ui.painter()
            .line_segment([at + vec2(4.0 * side, -2.0 * s), tip], Stroke::new(1.4, c));
    }
    if resp.has_focus() {
        ui.painter().rect_stroke(
            resp.rect.shrink(2.0),
            CornerRadius::same(radius::INPUT),
            Stroke::new(2.0, t.focus),
            StrokeKind::Inside,
        );
    }
    resp
}

/// The mixer inspector: its name, its effects with their parameters (move, remove), the Add
/// effect row and Remove mixer.
fn inspector(ui: &mut Ui, g: &GraphCtx, st: &mut GraphState, edits: &mut Vec<Edit>, n: usize) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let Some(node) = dev.nodes.get(n) else {
        ui.close();
        return;
    };
    let name = node_name(node);
    ui.set_width(INSPECTOR_W);
    let row = |ui: &mut Ui| {
        ui.allocate_exact_size(vec2(INSPECTOR_W, MIN_HIT), Sense::hover())
            .0
    };
    let note = |ui: &mut Ui, text: &str| {
        ui.add(egui::Label::new(egui::RichText::new(text).size(11.0).color(t.muted)).wrap());
    };

    // Name.
    let r = row(ui);
    ui.painter().text(
        pos2(r.left(), r.center().y),
        Align2::LEFT_CENTER,
        "Name",
        caption(),
        t.muted,
    );
    let fr =
        Rect::from_x_y_ranges((r.left() + 72.0)..=r.right(), r.y_range()).shrink2(vec2(0.0, 2.0));
    let id = Id::unique(("insp_name", d, n));
    if let Some(typed) = field(
        ui,
        st,
        t,
        fr,
        id,
        "Module name",
        name.to_string(),
        Align2::LEFT_CENTER,
    ) && !typed.trim().is_empty()
        && typed.trim() != name
    {
        edits.push(Edit::RenameNode(d, n, typed));
    }

    // Effects, in the order they run.
    for (i, fx) in node.fx.iter().enumerate() {
        ui.separator();
        let what = fx_name(fx);
        let r = row(ui);
        ui.painter().text(
            pos2(r.left(), r.center().y),
            Align2::LEFT_CENTER,
            what,
            label(),
            t.ink,
        );
        let key = |s: &str| Id::unique(("insp_fx", d, n, i, s.to_string()));
        let x = x_button(
            ui,
            pos2(r.right() - 14.0, r.center().y),
            key("remove"),
            t,
            &format!("Remove {what}"),
        );
        if x.clicked() {
            edits.push(Edit::NodeFx(d, n, FxEdit::Remove(i)));
        }
        if i + 1 < node.fx.len() {
            let down = format!("Move {what} down");
            let at = pos2(r.right() - 46.0, r.center().y);
            if arrow_button(ui, t, at, key("down"), false, &down).clicked() {
                edits.push(Edit::NodeFx(d, n, FxEdit::Move(i, i + 1)));
            }
        }
        if i > 0 {
            let up = format!("Move {what} up");
            let at = pos2(r.right() - 78.0, r.center().y);
            if arrow_button(ui, t, at, key("up"), true, &up).clicked() {
                edits.push(Edit::NodeFx(d, n, FxEdit::Move(i, i - 1)));
            }
        }
        note(ui, &fx_note(fx));
        for (k, p) in fx_params(fx).iter().enumerate() {
            if let Some(x) = param_row(ui, st, t, key(p.name), p) {
                edits.push(Edit::NodeFx(d, n, FxEdit::Set(i, with_param(fx, k, x))));
            }
        }
    }

    // Add effect: one button per kind that still fits.
    ui.separator();
    let room = can_add_fx(dev, n);
    let kinds = fx_kinds();
    let r = row(ui);
    ui.painter().text(
        pos2(r.left(), r.center().y),
        Align2::LEFT_CENTER,
        "Add effect",
        caption(),
        t.muted,
    );
    let r = row(ui);
    let mut x = r.left();
    for fx in kinds.into_iter().filter(|fx| room(fx)) {
        let what = fx_name(&fx);
        let id = Id::unique(("insp_add", d, n, what));
        let resp = text_button(ui, t, pos2(x, r.top()), id, what, &format!("Add {what}"));
        x = resp.rect.right() + 4.0;
        if resp.clicked() {
            edits.push(Edit::NodeFx(d, n, FxEdit::Add(fx)));
        }
    }
    if node.fx.len() >= sr_core::config::MAX_FX {
        note(ui, "A module holds four effects at most.");
    }

    // Remove: the cables around it join up again; the toast offers Undo.
    ui.separator();
    let r = row(ui);
    let id = Id::unique(("insp_remove", d, n));
    let resp = text_button(
        ui,
        t,
        r.min,
        id,
        "Remove module",
        &format!("Remove {name} module"),
    );
    if resp.clicked() {
        edits.push(Edit::RemoveNode(d, n));
        ui.close();
    }
}

fn wires_and_ports(
    ui: &mut Ui,
    g: &GraphCtx,
    st: &mut GraphState,
    edits: &mut Vec<Edit>,
    layout: &Layout,
    strips: &mut dyn FnMut(&mut Ui, &mut GraphState, &mut Vec<Edit>),
) {
    let (dev, t, d) = (g.dev, g.t, g.dev_idx);
    let ports = layout.ports();
    let all = links(dev);
    // Before the ports: where a chip's target runs into a port's, the port wins.
    let trims = trim_controls(ui, g, st, edits, layout, &all);
    // Interaction first so this frame's drag state drives the painting below.
    for &p in &ports {
        let Some(pos) = layout.port_pos(p) else {
            continue;
        };
        // Rows sit 24 px apart, so a port's target is 28 wide and 24 tall: the one place the
        // pointer minimum is the macOS 24 rather than `MIN_HIT`.
        let resp = ui
            .interact(
                Rect::from_center_size(pos, vec2(MIN_HIT, ROW_PITCH)),
                Id::unique(("port", d, p)),
                Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
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
    // The x of the hot cable wins over a jack beside it.
    let on_buttons = |pos: Pos2| {
        all.iter().zip(&trims).any(|(l, tr)| {
            st.trim_hot == Some(*l)
                && tr
                    .as_ref()
                    .is_some_and(|tr| tr.x_at().is_some_and(|x| zone(x).contains(pos)))
        })
    };
    let hover = pointer
        .filter(|&pos| !on_buttons(pos))
        .and_then(|pos| layout.hit_port(pos, 11.0, |_| true));
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
    let slots = st.cables.slots(dev, &all).to_vec();
    let now = ui.input(|i| i.time);
    // A wire seen for the first time pops its destination jack. The first frame of a device only
    // learns the wires it already has.
    let fresh = if st.plugs_dev == dev.id {
        now
    } else {
        st.plugs.clear();
        st.plugs_dev.clone_from(&dev.id);
        f64::NEG_INFINITY
    };
    for l in &all {
        st.plugs.entry((l.from, l.to)).or_insert(fresh);
    }
    st.plugs
        .retain(|k, _| all.iter().any(|l| (l.from, l.to) == *k));
    if st
        .plugs
        .values()
        .any(|t0| (0.0..POP_SECS).contains(&(now - t0)))
    {
        ui.request_repaint();
    }
    // A muted source, a muted mixer or a disabled device fades its cables and jacks into the
    // canvas.
    let fade = |c: Color32, p: Port| {
        let quiet = !dev.enabled
            || feeding_source(&all, p).is_some_and(|s| dev.sources[s].muted)
            || matches!(p, Port::NodeOut { node, .. } if dev.nodes[node].muted);
        if quiet {
            c.lerp_to_gamma(t.canvas, 0.65)
        } else {
            c
        }
    };
    // Only the graph's own layer: never a wire under an open menu or the setup sheet.
    let on_graph = |pos| ui.ctx().layer_id_at(pos).is_none_or(|l| l == ui.layer_id());
    // The pointer over a module card or the Processing column's empty state is on it, not on a
    // cable passing under it.
    let on_card = |pos: Pos2| {
        layout
            .cards
            .iter()
            .filter(|c| matches!(c.id, CardId::Node(_)))
            .map(|c| c.rect)
            .chain(layout.proc_empty)
            .any(|r| r.contains(pos))
    };
    // A device cable is hovered over its trim chip, or its x while that was showing, else
    // anywhere along it; a monitor cable anywhere along it.
    let hovered_wire = match (hover, pointer, st.drag) {
        (None, Some(pos), None) if on_graph(pos) => trims
            .iter()
            .position(|tr| tr.as_ref().is_some_and(|tr| tr.resp.hovered()))
            .or_else(|| {
                all.iter().enumerate().position(|(n, l)| {
                    st.trim_hot == Some(*l)
                        && trims[n]
                            .as_ref()
                            .is_some_and(|tr| tr.capsule().contains(pos))
                })
            })
            .or_else(|| {
                all.iter().position(|l| {
                    !on_card(pos)
                        && match (layout.port_pos(l.from), layout.port_pos(l.to)) {
                            (Some(a), Some(b)) => wire_distance(&wire_points(a, b), pos) < 7.0,
                            _ => false,
                        }
                })
            }),
        _ => None,
    };
    // The x also stays while the chip or the x has the keyboard.
    let x_id = |l: &Link| Id::unique(("unplug", d, l.from, l.to));
    st.trim_hot = hovered_wire
        .or_else(|| {
            all.iter().enumerate().position(|(n, l)| {
                trims[n].as_ref().is_some_and(|tr| tr.resp.has_focus())
                    || ui.memory(|m| m.has_focus(x_id(l)))
            })
        })
        .map(|n| all[n]);
    // Clicking a highlighted monitor cable removes it (as does dragging its input end away); a
    // source cable only comes out by its x.
    if let Some(n) = hovered_wire
        && matches!(all[n].to, Port::Mon { .. })
        && ui.input(|i| i.pointer.primary_clicked())
    {
        edits.push(Edit::Unlink(d, all[n]));
    }
    let open_trim = trims
        .iter()
        .position(|tr| tr.as_ref().is_some_and(|tr| tr.open));
    // A hovered jack or cable lights every cable it touches and the jacks at their ends, and
    // the rest step back a little.
    let lit = |l: &Link, n: usize| {
        hovered_wire == Some(n)
            || open_trim == Some(n)
            || hover.is_some_and(|h| h == l.from || h == l.to)
    };
    let any_lit = st.drag.is_none() && all.iter().enumerate().any(|(n, l)| lit(l, n));
    let step_back = |c: Color32, on: bool| {
        if on || !any_lit {
            c
        } else {
            c.lerp_to_gamma(t.canvas, 0.4)
        }
    };
    // The lit cables go on top of the others.
    for top in [false, true] {
        for (n, l) in all.iter().enumerate() {
            let (Some(a), Some(b)) = (layout.port_pos(l.from), layout.port_pos(l.to)) else {
                continue;
            };
            if lit(l, n) != top {
                continue;
            }
            // A mixer with nothing behind it sends nothing: its cables are grey.
            let c = if matches!(l.from, Port::NodeOut { .. })
                && feeding_source(&all, l.from).is_none()
            {
                t.muted
            } else {
                t.channel(usize::from(slots[n]))
            };
            draw_cable(
                &painter,
                wire_points(a, b),
                step_back(fade(c, l.from), top),
                if top { 4.5 } else { 3.0 },
            );
        }
    }
    // Module cards stand in front of the cables that cross the Processing column.
    for c in &layout.cards {
        if matches!(c.id, CardId::Node(_)) {
            painter.rect(
                c.rect,
                CornerRadius::same(radius::STRIP),
                t.raised,
                Stroke::new(1.0, t.hairline),
                StrokeKind::Inside,
            );
        }
    }
    strips(ui, st, edits);
    // The cable in flight shows the colour it will take where it lands: the slot of the cable
    // that is already there, else the one the colouring would give the new cable.
    let landing = st.drag.zip(target).map(|(from, to)| {
        let new = Link { from, to };
        let slot = all.iter().position(|l| *l == new).map_or_else(
            || {
                let mut with = all.clone();
                with.push(new);
                assign(dev, &with).last().copied().unwrap_or(0)
            },
            |n| slots[n],
        );
        t.channel(usize::from(slot))
    });
    if let (Some(from), Some(pos)) = (st.drag, pointer)
        && let Some(a) = layout.port_pos(from)
    {
        let end = target.and_then(|p| layout.port_pos(p)).unwrap_or(pos);
        let c = landing.unwrap_or(t.muted);
        draw_cable(&painter, wire_points(a, end), c, 3.0);
        if target.is_none() {
            // The loose end carries its plug until it lands.
            jack(&painter, end, Plug::One(c), 1.0, false, t);
        }
    }
    let pop = |p: Port| {
        all.iter()
            .filter(|l| l.to == p)
            .filter_map(|l| st.plugs.get(&(l.from, l.to)))
            .map(|t0| pop_scale(now - t0))
            .fold(1.0, f32::max)
    };
    for &p in &ports {
        let Some(pos) = layout.port_pos(p) else {
            continue;
        };
        let mut plug = jack_plug(&all, &slots, p, t);
        if target == Some(p)
            && let Some(c) = landing
        {
            plug = Plug::One(c);
        }
        let lights = all
            .iter()
            .enumerate()
            .any(|(n, l)| (l.from == p || l.to == p) && lit(l, n));
        let hot = lights || hover == Some(p) || target == Some(p) || st.drag == Some(p);
        let plug = plug.map(|c| step_back(fade(c, p), hot));
        let grow = if hot { 1.25 } else { 1.0 };
        jack(&painter, pos, plug, grow * pop(p), hot, t);
    }
    // Trim badges, and over the hovered cable its trim button with the x that unplugs it. A
    // monitor cable shows the x on the cable itself.
    for (n, l) in all.iter().enumerate() {
        let hot = hovered_wire == Some(n);
        if let Some(tr) = &trims[n] {
            let focused = tr.resp.has_focus() || ui.memory(|m| m.has_focus(x_id(l)));
            let hot = hot || tr.open || focused;
            tr.paint(&painter, t, hot);
            if hot && let Some(at) = tr.x_at() {
                let x = x_button(
                    ui,
                    at,
                    x_id(l),
                    t,
                    &format!("Unplug cable, {}", link_tip(g, *l)),
                );
                if x.clicked() {
                    edits.push(Edit::Unlink(d, *l));
                }
            }
        } else if hot && let (Some(a), Some(b)) = (layout.port_pos(l.from), layout.port_pos(l.to)) {
            let mid = wire_mid(&wire_points(a, b));
            painter.circle(mid, 9.0, t.raised, Stroke::new(1.0, t.bad_ink));
            xmark(&painter, mid, t.bad_ink);
        }
    }
    // What the hovered cable (or every cable of the hovered jack) joins, in words.
    if st.drag.is_none() && open_trim.is_none() && (hovered_wire.is_some() || hover.is_some()) {
        let tip = all
            .iter()
            .enumerate()
            .filter(|&(n, l)| hovered_wire == Some(n) || (hovered_wire.is_none() && lit(l, n)))
            .map(|(_, l)| link_tip(g, *l))
            .take(6)
            .collect::<Vec<_>>()
            .join("\n");
        if !tip.is_empty() {
            egui::Tooltip::always_open(
                ui.ctx().clone(),
                ui.layer_id(),
                Id::unique(("cable_tip", d)),
                egui::PopupAnchor::Pointer,
            )
            .show(|ui| ui.label(tip));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{apply, bare_config};
    use crate::theme::{install_fonts, tokens};
    use egui_kittest::{Harness, kittest::Queryable};

    /// A mixer inserted ahead of another since the engine's last reading shifts config positions: the
    /// reduction still belongs to the strip with its id.
    #[test]
    fn the_limiter_reduction_follows_the_node_id_not_its_position() {
        let node = |id: &str| Node {
            id: id.into(),
            name: String::new(),
            channels: 2,
            gain_db: 0.0,
            muted: false,
            fx: Vec::new(),
        };
        let reading = MeterReading {
            limiter_reduction: vec![("n1".into(), 0.5)],
            ..MeterReading::default()
        };
        // Config now reads [n2, n1]; the reading still says [n1].
        assert!((limiter_db(Some(&reading), &node("n1")) + 6.02).abs() < 0.01);
        assert_eq!(limiter_db(Some(&reading), &node("n2")), 0.0);
        assert_eq!(limiter_db(None, &node("n1")), 0.0);
    }

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

    fn sources(n: usize) -> VirtualDevice {
        let mut c = bare_config();
        for i in 0..n {
            let kind = SourceKind::App {
                bundle_id: format!("app.n{i}"),
            };
            apply(&mut c, Edit::AddSource(0, kind, 2));
        }
        c.devices.remove(0)
    }

    fn colours(dev: &VirtualDevice) -> Vec<(String, Color32)> {
        (0..dev.sources.len())
            .map(|i| (dev.sources[i].id.clone(), source_color(dev, i)))
            .collect()
    }

    #[test]
    fn a_source_keeps_its_cable_colour_when_others_come_and_go() {
        let mut c = bare_config();
        for i in 0..4 {
            let kind = SourceKind::App {
                bundle_id: format!("app.n{i}"),
            };
            apply(&mut c, Edit::AddSource(0, kind, 2));
            let now = colours(&c.devices[0]);
            // the colour of an earlier source never changes when a later one is added
            assert_eq!(now.len(), i + 1);
            assert_eq!(now[i].1, cable_color(i));
        }
        let before = colours(&c.devices[0]);
        apply(&mut c, Edit::RemoveSource(0, 1));
        let after = colours(&c.devices[0]);
        assert_eq!(after.len(), 3);
        for (id, colour) in &after {
            assert_eq!(before.iter().find(|b| b.0 == *id).unwrap().1, *colour);
        }
        // every source of a device with fewer than eight has its own colour
        let mut distinct: Vec<Color32> = after.iter().map(|a| a.1).collect();
        distinct.dedup();
        assert_eq!(distinct.len(), 3);
    }

    #[test]
    fn colour_slots_follow_the_id_number_else_the_position() {
        assert_eq!(source_slot("s1", 5), 0);
        assert_eq!(source_slot("s8", 0), 7);
        assert_eq!(source_slot("s0", 3), 0);
        assert_eq!(source_slot("mic", 3), 3);
        assert_eq!(source_slot("sx", 2), 2);
        assert_eq!(source_slot("", 1), 1);
    }

    #[test]
    fn a_wire_carries_the_colour_of_the_source_behind_it() {
        let src = |s, ch| Port::Src { src: s, ch };
        let all = [
            Link {
                from: src(1, 0),
                to: Port::OutIn { ch: 0 },
            },
            Link {
                from: src(0, 0),
                to: Port::OutIn { ch: 0 },
            },
            Link {
                from: Port::OutOut { ch: 0 },
                to: Port::Mon { mon: 0, ch: 1 },
            },
        ];
        assert_eq!(feeding_source(&all, src(0, 1)), Some(0));
        assert_eq!(feeding_source(&all, Port::OutIn { ch: 0 }), Some(1));
        assert_eq!(feeding_source(&all, Port::OutOut { ch: 0 }), Some(1));
        assert_eq!(feeding_source(&all, Port::Mon { mon: 0, ch: 1 }), Some(1));
        // nothing feeds channel 2, nor a monitor channel without a wire
        assert_eq!(feeding_source(&all, Port::OutOut { ch: 1 }), None);
        assert_eq!(feeding_source(&all, Port::Mon { mon: 0, ch: 0 }), None);
    }

    fn link(from: Port, to: Port) -> Link {
        Link { from, to }
    }

    #[test]
    fn a_jack_wears_its_cables_and_an_open_one_its_channel_or_grey() {
        let t = tokens(true);
        let src = Port::Src { src: 0, ch: 0 };
        let out = |ch| Port::OutIn { ch };
        let c = |s: u8| t.channel(usize::from(s));
        // nothing wired: a source jack is neutral, an output jack still wears its channel
        assert_eq!(jack_plug(&[], &[], src, &t), Plug::Open(None));
        assert_eq!(jack_plug(&[], &[], out(2), &t), Plug::Open(Some(c(2))));
        // one cable: its slot, at both of its ends
        let one = [link(src, out(2))];
        assert_eq!(jack_plug(&one, &[5], src, &t), Plug::One(c(5)));
        assert_eq!(jack_plug(&one, &[5], out(2), &t), Plug::One(c(5)));
        assert_eq!(jack_plug(&one, &[5], out(1), &t), Plug::Open(Some(c(1))));
        // two cables: equal arcs in cable order
        let two = [link(src, out(3)), link(src, out(1))];
        assert_eq!(
            jack_plug(&two, &[4, 2], src, &t),
            Plug::Split([c(4), c(2), t.muted], 2)
        );
        // an output jack fed by two sources splits too
        let fed = [link(src, out(0)), link(Port::Src { src: 1, ch: 0 }, out(0))];
        assert_eq!(
            jack_plug(&fed, &[1, 6], out(0), &t),
            Plug::Split([c(1), c(6), t.muted], 2)
        );
        // many: the first three and the true count, which is what "+N" counts from
        let many: Vec<Link> = [5, 2, 0, 7, 4].map(|n| link(src, out(n))).to_vec();
        assert_eq!(
            jack_plug(&many, &[3, 1, 2, 0, 7], src, &t),
            Plug::Split([c(3), c(1), c(2)], 5)
        );
        // another source's cables do not colour this jack
        let other = [link(Port::Src { src: 1, ch: 0 }, out(0))];
        assert_eq!(jack_plug(&other, &[1], src, &t), Plug::Open(None));
        // a monitor jack follows the same rule with its own row
        let mon = Port::Mon { mon: 0, ch: 1 };
        assert_eq!(jack_plug(&[], &[], mon, &t), Plug::Open(Some(c(1))));
        let wired = [link(Port::OutOut { ch: 1 }, mon)];
        assert_eq!(jack_plug(&wired, &[6], mon, &t), Plug::One(c(6)));
    }

    #[test]
    fn cables_into_one_channel_wear_different_colours() {
        // three sources into channel 1, each cable its own colour, and the two cables of a
        // stereo source differ as well
        let mut c = bare_config();
        for n in 0..3 {
            apply(
                &mut c,
                Edit::AddSource(
                    0,
                    SourceKind::App {
                        bundle_id: format!("x.{n}"),
                    },
                    2,
                ),
            );
        }
        let mut dev = c.devices.remove(0);
        dev.wires.clear();
        for s in 0..3 {
            dev.wires
                .push(sr_core::config::Wire::new(dev.sources[s].id.clone(), 0, 0));
        }
        dev.wires
            .push(sr_core::config::Wire::new(dev.sources[0].id.clone(), 1, 1));
        let all = links(&dev);
        let slots = crate::cables::assign(&dev, &all);
        let distinct: HashSet<u8> = slots.iter().copied().collect();
        assert_eq!(distinct.len(), 4);
        for dark in [false, true] {
            let t = tokens(dark);
            let colours: HashSet<Color32> =
                slots.iter().map(|&s| t.channel(usize::from(s))).collect();
            assert_eq!(colours.len(), 4);
        }
    }

    #[test]
    fn a_cable_tooltip_names_both_ends() {
        let tk = tokens(true);
        let mut dev = device();
        dev.wires.clear();
        let g = GraphCtx {
            dev_idx: 0,
            dev: &dev,
            hw: &[],
            apps: &[],
            icons: &IconTextures::default(),
            installed: &[],
            reading: None,
            t: &tk,
            mic: TccStatus::Granted,
            min_h: 500.0,
        };
        let l = link(Port::Src { src: 0, ch: 0 }, Port::OutIn { ch: 2 });
        assert_eq!(link_tip(&g, l), "y, channel 1 (L) to Channel 3 (L)");
        let m = link(Port::OutOut { ch: 1 }, Port::Mon { mon: 0, ch: 1 });
        assert_eq!(
            link_tip(&g, m),
            "Output channel 2 (R) to spk, channel 2 (R)"
        );
    }

    #[test]
    fn the_plug_in_pop_swells_then_settles_and_only_while_young() {
        assert_eq!(pop_scale(-1.0), 1.0);
        assert_eq!(pop_scale(0.0), 1.0);
        assert!((pop_scale(POP_SECS / 2.0) - 1.5).abs() < 1e-6);
        assert_eq!(pop_scale(POP_SECS), 1.0);
        assert_eq!(pop_scale(5.0), 1.0);
        assert_eq!(pop_scale(f64::INFINITY), 1.0);
        assert!(pop_scale(POP_SECS * 0.25) > 1.0);
    }

    #[test]
    fn a_new_wire_asks_for_frames_only_while_its_jack_pops() {
        let mut dev = device();
        dev.wires.clear();
        let mut h = rig(dev);
        h.run_steps(10);
        assert!(
            !h.ctx.has_requested_repaint(),
            "{:?}",
            h.ctx.repaint_causes()
        );
        let source = h.state().dev.sources[0].id.clone();
        h.state_mut()
            .dev
            .wires
            .push(sr_core::config::Wire::new(source, 0, 0));
        h.step();
        assert!(h.ctx.has_requested_repaint());
        h.run_steps(6); // 0.3 s
        assert!(
            !h.ctx.has_requested_repaint(),
            "{:?}",
            h.ctx.repaint_causes()
        );
    }

    #[test]
    fn wires_that_were_already_there_do_not_pop() {
        let mut h = rig(device());
        h.run_steps(2);
        assert!(!h.ctx.has_requested_repaint());
    }

    #[test]
    fn an_empty_device_invites_you_to_add_a_source_and_a_monitor() {
        let mut dev = sources(0);
        dev.monitors.clear();
        let mut h = rig(dev);
        h.get_by_label("Add monitor (empty list)");
        h.get_by_label("Add source (empty list)").click();
        h.run_steps(3);
        h.get_by_label("Search sources");
    }

    #[test]
    fn the_invitation_goes_away_with_the_first_source() {
        let h = rig(device());
        assert!(h.query_by_label("Add source (empty list)").is_none());
        assert!(h.query_by_label("Add monitor (empty list)").is_none());
    }

    #[test]
    fn options_stay_reachable_without_the_footer_row() {
        let mut h = rig(device());
        h.get_by_label("y options").click();
        h.run_steps(3);
        h.get_by_label("y volume");
    }

    /// Snapshots: `cargo test -p soundrouter-app --features snapshots` (`UPDATE_SNAPSHOTS=1` rewrites).
    #[cfg(feature = "snapshots")]
    mod snapshots {
        use super::*;
        use crate::fake::{FakeBackend, demo_config};
        use eframe::egui::Theme;
        use sr_core::backend::Backend;

        struct P {
            st: GraphState,
            dev: VirtualDevice,
            hw: Vec<HwDevice>,
            reading: Option<MeterReading>,
            layout: Option<Layout>,
            dark: bool,
            none: bool,
        }

        fn harness(
            theme: Theme,
            dev: VirtualDevice,
            loud: bool,
            none: bool,
        ) -> Harness<'static, P> {
            let reading = loud.then(|| MeterReading {
                sources: dev
                    .sources
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let lv = [[0.9, 0.55], [0.12, 0.05], [0.0, 0.0]][i % 3];
                        (s.id.clone(), lv[..usize::from(s.channels).min(2)].to_vec())
                    })
                    .collect(),
                channels: vec![0.9, 0.55],
                ..Default::default()
            });
            Harness::builder()
                .with_size(vec2(1000.0, 560.0))
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(install_fonts)
                .wgpu()
                .build_ui_state(
                    |ui, s: &mut P| {
                        let tk = tokens(s.dark);
                        ui.painter()
                            .rect_filled(ui.max_rect(), CornerRadius::ZERO, tk.canvas);
                        if s.none {
                            no_devices(ui, &tk, ui.max_rect());
                            return;
                        }
                        let g = GraphCtx {
                            dev_idx: 0,
                            dev: &s.dev,
                            hw: &s.hw,
                            apps: &[],
                            icons: &IconTextures::default(),
                            installed: &[],
                            reading: s.reading.as_ref(),
                            t: &tk,
                            mic: TccStatus::Granted,
                            min_h: 520.0,
                        };
                        s.layout = Some(show(ui, &g, &mut s.st, &mut Vec::new()));
                    },
                    P {
                        st: GraphState::default(),
                        dev,
                        hw: FakeBackend::new().hardware(),
                        reading,
                        layout: None,
                        dark: theme == Theme::Dark,
                        none,
                    },
                )
        }

        fn populated() -> VirtualDevice {
            let mut c = demo_config();
            c.devices.remove(0)
        }

        #[test]
        fn populated_device() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [(Theme::Dark, "graph_dark"), (Theme::Light, "graph_light")] {
                let mut h = harness(theme, populated(), true, false);
                h.run_steps(30);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }

        #[test]
        fn empty_device() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [
                (Theme::Dark, "graph_empty_dark"),
                (Theme::Light, "graph_empty_light"),
            ] {
                let mut dev = populated();
                dev.sources.clear();
                dev.wires.clear();
                dev.nodes.clear();
                dev.monitors.clear();
                let mut h = harness(theme, dev, false, false);
                h.run_steps(3);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }

        /// A 4-channel device fed by three sources: Safari's left channel leaves by two cables
        /// (a split jack), Pass-Thru's by four (a split jack with "+1"), every cable its own colour.
        fn four_channel() -> VirtualDevice {
            let mut c = demo_config();
            apply(&mut c, Edit::GrowChannels(0));
            let dev = &mut c.devices[0];
            let ids: Vec<String> = dev.sources.iter().map(|s| s.id.clone()).collect();
            // Only the cables between sources and channels: no mixer.
            dev.nodes.clear();
            dev.wires = [
                (0, 0, 0),
                (0, 1, 1),
                (0, 0, 2),
                (1, 0, 2),
                (1, 1, 3),
                (2, 0, 0),
                (2, 0, 1),
                (2, 0, 2),
                (2, 0, 3),
            ]
            .map(|(s, src_ch, out_ch)| sr_core::config::Wire::new(ids[s].clone(), src_ch, out_ch))
            .to_vec();
            c.devices.remove(0)
        }

        /// Three stereo sources into the same two channels, which play on two monitors: ten
        /// cables, none sharing a colour with a cable it meets.
        #[test]
        fn three_sources_into_two_channels_and_two_monitors() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [
                (Theme::Dark, "graph_three_into_two_dark"),
                (Theme::Light, "graph_three_into_two_light"),
            ] {
                let mut c = demo_config();
                apply(&mut c, Edit::AddMonitor(0, "USBHeadset".into(), 2));
                let dev = &mut c.devices[0];
                dev.nodes.clear();
                dev.wires = (0..3)
                    .flat_map(|s| (0..2).map(move |ch| (s, ch)))
                    .map(|(s, ch)| sr_core::config::Wire::new(dev.sources[s].id.clone(), ch, ch))
                    .collect();
                let mut h = harness(theme, c.devices.remove(0), true, false);
                h.run_steps(30);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }

        #[test]
        fn four_channel_device_wired_from_three_sources() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [
                (Theme::Dark, "graph_four_channel_dark"),
                (Theme::Light, "graph_four_channel_light"),
            ] {
                let mut h = harness(theme, four_channel(), true, false);
                h.run_steps(30);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }

        #[test]
        fn hovering_a_jack_lights_its_cables_and_names_them() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [
                (Theme::Dark, "graph_hover_dark"),
                (Theme::Light, "graph_hover_light"),
            ] {
                let mut h = harness(theme, four_channel(), true, false);
                h.run_steps(3);
                let at = h
                    .state()
                    .layout
                    .as_ref()
                    .unwrap()
                    .port_pos(Port::Src { src: 0, ch: 0 })
                    .unwrap();
                h.hover_at(at);
                h.run_steps(6);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }

        #[test]
        fn no_devices_at_all() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [
                (Theme::Dark, "graph_no_devices_dark"),
                (Theme::Light, "graph_no_devices_light"),
            ] {
                let mut h = harness(theme, populated(), false, true);
                h.run_steps(3);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }

        #[test]
        fn cable_in_flight_and_hovered_wire() {
            let mut results = egui_kittest::SnapshotResults::new();
            for (theme, name) in [
                (Theme::Dark, "graph_drag_dark"),
                (Theme::Light, "graph_drag_light"),
            ] {
                let mut dev = populated();
                dev.wires.clear();
                dev.nodes.clear();
                let mut h = harness(theme, dev, true, false);
                h.run_steps(3);
                let from = h
                    .state()
                    .layout
                    .as_ref()
                    .unwrap()
                    .port_pos(Port::Src { src: 1, ch: 0 })
                    .unwrap();
                h.hover_at(from);
                h.step();
                h.drag_at(from);
                h.step();
                h.hover_at(from + vec2(150.0, 90.0));
                h.run_steps(3);
                h.snapshot(name);
                results.extend_harness(&mut h);
            }
        }
    }
}
