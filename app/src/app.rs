//! Application state, sidebar and the window frame.
use crate::graph::{self, GraphCtx, GraphState};
use crate::icons::IconTextures;
use crate::login;
use crate::model::{Edit, apply, db_to_pct, pct_to_db};
use crate::store;
use crate::theme::{Tokens, semibold, tokens};
use crate::tray::{self, Action, Event, Tray, WinState};
use crate::widgets::{
    name_button, name_widget, pencil, plus_circle, plus_glyph, slider_knob, speaker, switch,
};
use crate::worker::Worker;
use eframe::egui::{
    self, Align2, CornerRadius, FontId, Id, Margin, Rect, Sense, Slider, Stroke, TextEdit, Theme,
    UiBuilder, WidgetInfo, accesskit::Role, pos2, vec2,
};
use sr_core::backend::{DeviceState, EngineStatus};
use sr_core::config::Config;
use std::path::PathBuf;
use std::time::Duration;

const SIDEBAR_W: f32 = 250.0;
const ROW_H: f32 = 78.0;
const AUDIO_CAPTURE_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture";
pub const PERMISSION_BANNER: &str =
    "Waiting for System Audio Recording permission — allow it in the prompt / System Settings";
const BLANK_NAME: &str = "A device name can't be empty.";
/// Room for the traffic lights in the unified titlebar.
const TITLEBAR_H: f32 = 46.0;

pub struct App {
    cfg: Config,
    /// None in demo mode: nothing is persisted.
    path: Option<PathBuf>,
    worker: Worker,
    selected: usize,
    graph: GraphState,
    icon_tex: IconTextures,
    renaming: Option<(usize, String)>,
    toast: Option<String>,
    save_pending: bool,
    vibrancy: bool,
    demo: bool,
    /// Menu-bar item; None if it could not be created (tests, or the system refused).
    tray: Option<Tray>,
    win: WinState,
    /// Passes left before hiding a `--background` window (eframe shows it after the first frame).
    hide_in: u8,
}

/// Label and dot color for a device state; idle devices show no dot.
fn state_look(s: &DeviceState, t: &Tokens) -> Option<(String, egui::Color32)> {
    Some(match s {
        DeviceState::Idle => return None,
        DeviceState::Running => ("Running".into(), t.meter[0]),
        DeviceState::Starting => ("Starting".into(), t.meter[1]),
        DeviceState::WaitingForPermission => ("Waiting for permission".into(), t.meter[1]),
        DeviceState::Failed(e) => (format!("Failed: {e}"), t.danger),
    })
}

impl App {
    pub fn new(
        cfg: Config,
        path: Option<PathBuf>,
        worker: Worker,
        toast: Option<String>,
        vibrancy: bool,
        demo: bool,
    ) -> Self {
        worker.apply(&cfg);
        Self {
            cfg,
            path,
            worker,
            selected: 0,
            graph: GraphState::default(),
            icon_tex: IconTextures::default(),
            renaming: None,
            toast,
            save_pending: false,
            vibrancy,
            demo,
            tray: None,
            win: WinState::default(),
            hide_in: 0,
        }
    }

    /// Keep routing when the window closes: menu-bar item plus close-to-hide state.
    pub fn background(mut self, tray: Option<Tray>, win: WinState, start_hidden: bool) -> Self {
        self.tray = tray;
        self.win = win;
        self.hide_in = if start_hidden { 2 } else { 0 };
        self
    }

    fn handle_window(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if self.hide_in > 0 {
            self.hide_in -= 1;
            if self.hide_in == 0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            events.push(Event::CloseRequested);
        }
        if tray::take_reopen() {
            events.push(Event::Reopen);
        }
        let running = self
            .engine_status()
            .per_device
            .iter()
            .filter(|(_, s)| *s == DeviceState::Running)
            .count();
        if let Some(t) = &mut self.tray {
            t.set_status(tray::status_text(running));
            for cmd in t.poll() {
                match cmd {
                    tray::Cmd::Open => events.push(Event::Open),
                    tray::Cmd::Quit => events.push(Event::Quit),
                    tray::Cmd::Login(on) => {
                        if let Err(e) = login::set_enabled(on) {
                            t.set_login_checked(!on);
                            self.toast = Some(format!("Could not change Start at Login: {e}"));
                        }
                    }
                }
            }
        }
        for e in events {
            let (win, action) = tray::step(self.win, e);
            self.win = win;
            match action {
                Action::Nothing => {}
                Action::Hide { hint } => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    if hint {
                        if let Some(t) = &mut self.tray {
                            t.hint();
                        }
                        if !self.demo {
                            tray::mark_hint_seen();
                        }
                    }
                }
                Action::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            }
        }
    }

    fn engine_status(&self) -> EngineStatus {
        self.worker.shared.lock().unwrap().status.clone()
    }

    fn commit(&mut self, edits: Vec<Edit>) {
        // A blank name can never be valid (the driver rejects it and then drops every device).
        let mut blank_rename = false;
        let edits: Vec<Edit> = edits
            .into_iter()
            .filter(|e| {
                let blank = matches!(e, Edit::Rename(_, n) if n.trim().is_empty());
                blank_rename |= blank;
                !blank
            })
            .collect();
        if edits.is_empty() {
            if blank_rename {
                self.toast = Some(BLANK_NAME.into());
            }
            return;
        }
        let prev = self.cfg.clone();
        let mut select_new = false;
        for e in edits {
            select_new |= e == Edit::NewDevice;
            apply(&mut self.cfg, e);
        }
        if let Err(e) = self.cfg.validate() {
            self.cfg = prev;
            self.toast = Some(e);
            return;
        }
        self.toast = blank_rename.then(|| BLANK_NAME.to_string());
        if select_new {
            self.selected = self.cfg.devices.len() - 1;
        }
        let last = self.cfg.devices.len().saturating_sub(1);
        if self.selected > last {
            self.selected = last;
        }
        self.worker.apply(&self.cfg);
        self.save_pending = true;
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, t: &Tokens, edits: &mut Vec<Edit>) {
        let fill = if self.vibrancy {
            t.sidebar.gamma_multiply(0.55)
        } else {
            t.sidebar
        };
        egui::Panel::left("devices")
            .exact_size(SIDEBAR_W)
            .resizable(false)
            .frame(egui::Frame::default().fill(fill))
            .show(ui, |ui| {
                let full = ui.max_rect();
                ui.painter().text(
                    pos2(full.left() + 84.0, full.top() + 23.0),
                    Align2::LEFT_CENTER,
                    "Devices",
                    FontId::new(13.0, semibold()),
                    t.text,
                );
                ui.painter().hline(
                    full.x_range(),
                    full.top() + TITLEBAR_H + 0.5,
                    Stroke::new(1.0, t.card_stroke.gamma_multiply(0.6)),
                );
                let footer_h = 48.0;
                let list = Rect::from_min_max(
                    pos2(full.left(), full.top() + TITLEBAR_H + 1.0),
                    pos2(full.right(), full.bottom() - footer_h),
                );
                ui.scope_builder(UiBuilder::new().max_rect(list), |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for i in 0..self.cfg.devices.len() {
                                self.device_row(ui, i, t, edits);
                            }
                        });
                });
                // Footer: New Virtual Device and remove.
                let foot =
                    Rect::from_min_max(pos2(full.left(), full.bottom() - footer_h), full.max);
                ui.painter().hline(
                    foot.x_range(),
                    foot.top() + 0.5,
                    Stroke::new(1.0, t.card_stroke.gamma_multiply(0.6)),
                );
                let cy = foot.center().y;
                let new = ui.interact(
                    Rect::from_min_size(pos2(foot.left() + 8.0, cy - 14.0), vec2(170.0, 28.0)),
                    Id::unique("new_dev"),
                    Sense::click(),
                );
                name_button(&new, "New Virtual Device");
                let c = if new.hovered() { t.text } else { t.text2 };
                plus_glyph(ui.painter(), pos2(foot.left() + 22.0, cy), c, false);
                ui.painter().text(
                    pos2(foot.left() + 38.0, cy),
                    Align2::LEFT_CENTER,
                    "New Virtual Device",
                    FontId::proportional(12.5),
                    c,
                );
                if new.clicked() {
                    edits.push(Edit::NewDevice);
                }
                let minus = plus_circle(
                    ui,
                    pos2(foot.right() - 24.0, cy),
                    Id::unique("del_dev_icon"),
                    t,
                    true,
                    "Delete selected device",
                );
                if minus.clicked() && self.selected < self.cfg.devices.len() {
                    edits.push(Edit::RemoveDevice(self.selected));
                }
                minus.on_hover_text("Delete the selected device");
            });
    }

    fn device_row(&mut self, ui: &mut egui::Ui, i: usize, t: &Tokens, edits: &mut Vec<Edit>) {
        let dev = &self.cfg.devices[i];
        let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
        name_button(&resp, &format!("Select {}", dev.name));
        if resp.clicked() {
            self.selected = i;
        }
        let inner = row.shrink2(vec2(8.0, 4.0));
        if i == self.selected {
            ui.painter()
                .rect_filled(inner, CornerRadius::same(9), t.row_sel);
        }
        let dim = if dev.enabled { 1.0 } else { 0.55 };
        let icon = Rect::from_min_size(
            pos2(inner.left() + 8.0, inner.top() + 9.0),
            vec2(30.0, 30.0),
        );
        crate::widgets::tile(ui.painter(), icon, t.accent, "∞", 19.0, dim);
        let name_pos = pos2(icon.right() + 10.0, inner.top() + 16.0);
        let name_rect = Rect::from_min_size(
            pos2(name_pos.x, name_pos.y - 10.0),
            vec2(inner.right() - 58.0 - name_pos.x, 20.0),
        );
        let renaming_here = matches!(&self.renaming, Some((r, _)) if *r == i);
        if renaming_here {
            let Some((_, buf)) = &mut self.renaming else {
                return;
            };
            let te = ui.put(
                name_rect,
                TextEdit::singleline(buf)
                    .font(FontId::new(13.5, semibold()))
                    .margin(Margin::ZERO)
                    .frame(egui::Frame::NONE)
                    .text_color(t.text),
            );
            name_widget(ui, &te, "Rename device");
            let esc = ui.input(|k| k.key_pressed(egui::Key::Escape));
            if te.lost_focus() || esc {
                if !esc {
                    edits.push(Edit::Rename(i, buf.trim().to_string()));
                }
                self.renaming = None;
            } else {
                // Only while still editing: re-requesting after Enter would undo the focus loss.
                te.request_focus();
            }
        } else {
            let g = ui.painter().layout_no_wrap(
                dev.name.clone(),
                FontId::new(13.5, semibold()),
                t.text.gamma_multiply(dim),
            );
            let shown = g.size().x.min(name_rect.width());
            ui.painter().with_clip_rect(name_rect).galley(
                pos2(name_pos.x, name_pos.y - g.size().y / 2.0),
                g,
                t.text,
            );
            let hit = ui.interact(
                Rect::from_min_size(name_rect.min, vec2(shown.max(40.0), 20.0)),
                Id::unique(("name", i)),
                Sense::click(),
            );
            name_button(&hit, &format!("Rename {}", dev.name));
            if hit.double_clicked() {
                self.renaming = Some((i, dev.name.clone()));
            }
            hit.on_hover_text("Double-click to rename");
        }
        let chans = format!("{} channels", dev.channels);
        ui.painter().text(
            pos2(name_pos.x, inner.top() + 34.0),
            Align2::LEFT_CENTER,
            chans,
            FontId::proportional(11.0),
            t.text2.gamma_multiply(dim),
        );
        let state = self
            .engine_status()
            .per_device
            .into_iter()
            .find_map(|(id, s)| (id == dev.id).then_some(s))
            .unwrap_or_default();
        if let Some((text, color)) = state_look(&state, t) {
            let c = pos2(inner.right() - 24.0, inner.top() + 34.0);
            ui.painter().circle_filled(c, 4.0, color);
            let dot = ui.interact(
                Rect::from_center_size(c, vec2(14.0, 14.0)),
                Id::unique(("state", i)),
                Sense::hover(),
            );
            let label = format!("{} status: {text}", dev.name);
            dot.widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
            dot.on_hover_text(text);
        }
        let sw = Rect::from_center_size(
            pos2(inner.right() - 8.0 - 16.0, inner.top() + 18.0),
            vec2(32.0, 18.0),
        );
        let on = switch(
            ui,
            sw,
            Id::unique(("dev_on", i)),
            dev.enabled,
            t,
            &format!("Enable {}", dev.name),
        );
        if on != dev.enabled {
            edits.push(Edit::Enable(i, on));
        }
        // Volume row: mute, slider, percent.
        let vy = inner.top() + 56.0;
        let mute = speaker(
            ui,
            pos2(inner.left() + 20.0, vy),
            Id::unique(("mute", i)),
            dev.muted,
            t,
            &format!("Mute {}", dev.name),
        );
        if mute.clicked() {
            edits.push(Edit::Mute(i, !dev.muted));
        }
        let mut pct = db_to_pct(dev.volume_db).clamp(0.0, 100.0);
        let before = pct;
        let sl = Rect::from_x_y_ranges(
            (inner.left() + 36.0)..=(inner.right() - 52.0),
            (vy - 10.0)..=(vy + 10.0),
        );
        let slider = ui.scope_builder(UiBuilder::new().max_rect(sl), |ui| {
            ui.spacing_mut().slider_width = sl.width() - 4.0;
            ui.add(Slider::new(&mut pct, 0.0..=100.0).show_value(false))
        });
        name_widget(ui, &slider.inner, &format!("{} volume", dev.name));
        slider_knob(ui.painter(), slider.inner.rect, pct / 100.0, t);
        if (pct - before).abs() > 1e-3 {
            edits.push(Edit::Volume(i, pct_to_db(pct)));
        }
        ui.painter().text(
            pos2(inner.right() - 10.0, vy),
            Align2::RIGHT_CENTER,
            format!("{pct:.0}%"),
            FontId::proportional(11.0),
            t.text2,
        );
    }

    fn main_pane(&mut self, ui: &mut egui::Ui, t: &Tokens, edits: &mut Vec<Edit>) {
        egui::Panel::bottom("bottom")
            .exact_size(44.0)
            .resizable(false)
            .frame(egui::Frame::default().fill(t.window))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(
                    r.x_range(),
                    r.top() + 0.5,
                    Stroke::new(1.0, t.card_stroke.gamma_multiply(0.6)),
                );
                let has = self.selected < self.cfg.devices.len();
                let btn =
                    Rect::from_center_size(pos2(r.left() + 58.0, r.center().y), vec2(72.0, 26.0));
                let resp = ui.interact(btn, Id::unique("delete"), Sense::click());
                name_button(&resp, "Delete");
                let fill = if resp.hovered() && has {
                    t.card_stroke
                } else {
                    t.card
                };
                let a = if has { 1.0 } else { 0.4 };
                ui.painter().rect(
                    btn,
                    CornerRadius::same(6),
                    fill.gamma_multiply(a),
                    Stroke::new(1.0, t.card_stroke),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    btn.center(),
                    Align2::CENTER_CENTER,
                    "Delete",
                    FontId::proportional(12.5),
                    t.text.gamma_multiply(a),
                );
                if has && resp.clicked() {
                    edits.push(Edit::RemoveDevice(self.selected));
                }
                let (apply_error, es) = {
                    let s = self.worker.shared.lock().unwrap();
                    (s.apply_error.clone(), s.status.clone())
                };
                let warn = (!es.warnings.is_empty()).then(|| es.warnings.join("; "));
                let status = self
                    .toast
                    .clone()
                    .or(apply_error)
                    .or(es.background_error)
                    .or(warn);
                let (msg, color) = match status {
                    Some(m) if m.contains("driver not installed") => (
                        "Driver not installed. Run `packaging/driver-dev.sh install`, then relaunch.".to_string(),
                        t.danger,
                    ),
                    Some(m) => (m, t.danger),
                    None if self.demo => (
                        "Demo mode: synthetic audio, nothing is saved".to_string(),
                        t.text2,
                    ),
                    None => return,
                };
                let x = btn.right() + 18.0;
                let width = r.right() - 18.0 - x;
                graph::text_left(ui, x, r.center().y, &msg, FontId::proportional(12.0), color, width);
                let status = ui.interact(
                    Rect::from_min_max(pos2(x, r.top()), pos2(r.right() - 18.0, r.bottom())),
                    Id::unique("status"),
                    Sense::hover(),
                );
                status.widget_info(|| WidgetInfo::labeled(Role::Label, true, &msg));
                status.on_hover_text(msg);
            });

        let waiting = self
            .engine_status()
            .per_device
            .iter()
            .any(|(_, s)| *s == DeviceState::WaitingForPermission);
        if waiting {
            egui::Panel::top("permission")
                .frame(
                    egui::Frame::default()
                        .fill(t.card)
                        .inner_margin(Margin::same(10)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(PERMISSION_BANNER);
                        if ui.button("Open Settings").clicked() {
                            let _ = std::process::Command::new("open")
                                .arg(AUDIO_CAPTURE_SETTINGS)
                                .spawn();
                        }
                    });
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(t.window))
            .show(ui, |ui| {
                let full = ui.max_rect();
                let Some(dev) = self.cfg.devices.get(self.selected) else {
                    ui.painter().text(
                        full.center(),
                        Align2::CENTER_CENTER,
                        "No virtual devices.\nUse “New Virtual Device” to create one.",
                        FontId::proportional(14.0),
                        t.text2,
                    );
                    return;
                };
                // Title (editable) in the titlebar zone.
                let title_rect = Rect::from_min_size(
                    pos2(full.left() + 26.0, full.top() + 12.0),
                    vec2(360.0, 26.0),
                );
                let mut name = dev.name.clone();
                let te = ui.put(
                    title_rect,
                    TextEdit::singleline(&mut name)
                        .font(FontId::new(19.0, semibold()))
                        .margin(Margin::ZERO)
                        .frame(egui::Frame::NONE)
                        .text_color(t.text),
                );
                name_widget(ui, &te, "Device name");
                let w = ui
                    .painter()
                    .layout_no_wrap(name.clone(), FontId::new(19.0, semibold()), t.text)
                    .size()
                    .x;
                pencil(
                    ui.painter(),
                    pos2(
                        title_rect.left() + w.min(330.0) + 16.0,
                        title_rect.center().y,
                    ),
                    if te.hovered() || te.has_focus() {
                        t.text
                    } else {
                        t.text2
                    },
                );
                if name != dev.name {
                    edits.push(Edit::Rename(self.selected, name));
                }

                let snap = self.worker.shared.lock().unwrap();
                let hw = snap.hardware.clone();
                let apps = snap.apps.clone();
                let installed = self.graph.installed.clone();
                let reading = snap.meters.get(&dev.id).cloned();
                self.icon_tex.sync(ui.ctx(), &snap.icons);
                drop(snap);
                let area = Rect::from_min_max(pos2(full.left(), full.top() + 44.0), full.max);
                ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| {
                            let g = GraphCtx {
                                dev_idx: self.selected,
                                dev,
                                hw: &hw,
                                apps: &apps,
                                icons: &self.icon_tex,
                                installed: installed.get().map_or(&[], Vec::as_slice),
                                reading: reading.as_ref(),
                                t,
                                min_h: area.height(),
                            };
                            graph::show(ui, &g, &mut self.graph, edits);
                        });
                });
            });
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_window(ctx);
    }

    /// Cmd-Q ends the process without dropping `App`: stop the worker so the engine shuts down.
    fn on_exit(&mut self) {
        log::info!("quitting: stopping the audio engine");
        self.worker.stop();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if self.vibrancy {
            [0.0; 4]
        } else {
            [0.0, 0.0, 0.0, 1.0]
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let t = tokens(ctx.theme() == Theme::Dark);
        ctx.global_style_mut(|s| {
            s.visuals.selection.bg_fill = t.accent;
            s.visuals.override_text_color = Some(t.text);
            s.visuals.widgets.inactive.bg_fill = t.switch_off;
            s.interaction.selectable_labels = false;
            s.visuals.handle_shape = egui::style::HandleShape::Circle;
            s.visuals.slider_trailing_fill = true;
            s.spacing.slider_rail_height = 4.0;
        });
        ctx.request_repaint_after(Duration::from_millis(33));

        let prev_selected = self.selected;
        let mut edits = Vec::new();
        self.sidebar(ui, &t, &mut edits);
        self.main_pane(ui, &t, &mut edits);
        if self.selected != prev_selected {
            self.graph.peaks.clear();
            self.graph.drag = None;
        }
        self.commit(edits);

        if self.save_pending && !ctx.input(|i| i.pointer.any_down()) {
            self.save_pending = false;
            if let Some(p) = &self.path
                && let Err(e) = store::save(p, &self.cfg)
            {
                self.toast = Some(format!("Could not save config: {e}"));
            }
        }
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
