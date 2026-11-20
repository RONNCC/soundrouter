//! Application state, sidebar and the window frame.
use crate::graph::{self, GraphCtx, GraphState};
use crate::health;
use crate::icons::IconTextures;
use crate::login;
use crate::model::{
    Edit, LOW_POWER_HINT, apply, apps_readout, db_to_pct, delay_chip_tip, delay_hint, delay_label,
    delay_ms, delay_name, delay_option, delay_readout, delay_readout_tip, device_delay_ms,
    device_tap_ms, low_power_hint_due, pct_to_db,
};
use crate::presets::{self, Presets};
use crate::setup::{self, AUDIO_CAPTURE_SETTINGS, Action as SetupAction, OPEN_SETTINGS, Setup};
use crate::store;
use crate::theme::{
    Tokens, body, cable_color, caption, device_title, label, num, radius, section, strong, tokens,
};
use crate::tray::{self, Action, Event, Tray, WinState};
use crate::widgets::{
    loops, name_button, name_widget, pencil, plus_glyph, slider_knob, speaker, switch, xmark,
};
use crate::worker::Worker;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Id, Margin, Rect, Sense, Slider, Stroke, TextEdit,
    TextStyle, Theme, UiBuilder, WidgetInfo, accesskit::Role, pos2, vec2,
};
use sr_core::backend::{DeviceState, EngineStatus, TccStatus};
use sr_core::config::{BUFFER_FRAMES, Config, MAX_NAME_CHARS, SourceKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const SIDEBAR_W: f32 = 250.0;
const ROW_H: f32 = 78.0;
/// Sidebar footer and window footer share a height, so their hairlines are one line.
const FOOTER_H: f32 = 44.0;
pub const PERMISSION_BANNER: &str =
    "Waiting for System Audio Recording permission. Allow it in the prompt or in System Settings.";
const BLANK_NAME: &str = "Device names can't be empty, so SoundRouter kept the old name.";
/// Sidebar button, empty-state hint and tests all use this one name.
pub const NEW_DEVICE: &str = "New device";
/// Window footer buttons; tests click them by these names.
const REMOVE_DEVICE: &str = "Remove device";
const TEST_SETUP: &str = "Test setup";
/// Room for the traffic lights in the unified titlebar.
const TITLEBAR_H: f32 = 46.0;
/// How long "Removed …. Undo" stays up, in egui seconds (frame time, so tests step it).
const UNDO_SECS: f64 = 6.0;

/// The config as it was before a removal or a preset, kept briefly so it can be taken back.
struct Undo {
    /// What happened, so the toast reads "{verb} {name}.": "Removed" or "Applied preset".
    verb: &'static str,
    name: String,
    prev: Config,
    selected: usize,
    until: f64,
}

pub struct App {
    cfg: Config,
    /// None in demo mode: nothing is persisted.
    path: Option<PathBuf>,
    worker: Worker,
    selected: usize,
    graph: GraphState,
    icon_tex: IconTextures,
    renaming: Option<(usize, String)>,
    /// Scroll the sidebar to the selected row on the next frame (it was created or restored).
    reveal: bool,
    toast: Option<String>,
    save_pending: bool,
    vibrancy: bool,
    demo: bool,
    /// Menu-bar item; None if it could not be created (tests, or the system refused).
    tray: Option<Tray>,
    win: WinState,
    /// Passes left before hiding a `--background` window (eframe shows it after the first frame).
    hide_in: u8,
    setup: Setup,
    /// The config has been sent to the engine at least once (and the driver is still there).
    applied: bool,
    undo: Option<Undo>,
    presets: Presets,
    /// A preset chosen in the toolbar menu this frame; applied after the frame's edits.
    preset_pick: Option<String>,
    /// egui time of the current frame.
    now: f64,
    /// Why the real audio engine did not start; the app then only edits and saves the config.
    engine_error: Option<String>,
    /// Health banners the user closed, and when each device's dropout counter last grew.
    dismissed: health::Dismissed,
    dropouts: health::Dropouts,
    os: Os,
}

/// What the window asks of the machine outside the audio engine: Start at login and System Settings
/// links. A field of `App`, so tests never touch `~/Library/LaunchAgents` or open System Settings.
#[derive(Clone)]
struct Os {
    login_is_on: Arc<dyn Fn() -> bool + Send + Sync>,
    set_login: Arc<dyn Fn(bool) -> std::io::Result<()> + Send + Sync>,
    open_url: Arc<dyn Fn(&str) + Send + Sync>,
}

impl Os {
    fn system() -> Self {
        Self {
            login_is_on: Arc::new(login::is_enabled),
            set_login: Arc::new(login::set_enabled),
            open_url: Arc::new(setup::open_url),
        }
    }
}

/// Label and dot color for a device state; idle devices show no dot.
fn state_look(s: &DeviceState, t: &Tokens) -> Option<(String, egui::Color32)> {
    Some(match s {
        DeviceState::Idle => return None,
        DeviceState::Running => ("Running".into(), t.ok),
        DeviceState::Starting => ("Starting".into(), t.warn),
        DeviceState::WaitingForPermission => ("Waiting for permission".into(), t.warn),
        DeviceState::Failed(e) => (format!("Failed: {e}"), t.bad),
    })
}

/// What the delay menu says about the machine, besides the setting itself.
struct DelayFacts {
    /// macOS Low Power Mode is on.
    low_power: bool,
    /// An enabled device exists to measure through.
    can_measure: bool,
    /// Dropouts since start, and whether they grew in the last few seconds.
    dropouts: u64,
    growing: bool,
}

/// The "Audio delay" popup: one plain sentence, then a radio-style row per buffer size, and the
/// action that measures the delay for real (`measure` is set when it is clicked).
fn delay_menu(
    ui: &mut egui::Ui,
    t: &Tokens,
    current: u32,
    facts: &DelayFacts,
    edits: &mut Vec<Edit>,
    measure: &mut bool,
) {
    use egui::text::{LayoutJob, TextFormat};
    const W: f32 = 360.0;
    const CHECK_W: f32 = 24.0;
    const MS_W: f32 = 64.0;
    ui.set_width(W);
    ui.label(
        egui::RichText::new("Audio delay")
            .font(strong())
            .color(t.ink),
    );
    // A menu justifies its labels; a paragraph reads better ragged-right.
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new("How long audio takes to pass through SoundRouter. Lower is snappier for live monitoring and calls, but uses more CPU and can crackle on a busy Mac. Higher is the most reliable.")
                    .font(body())
                    .line_height(Some(18.0))
                    .color(t.muted),
            )
            .wrap(),
        );
    });
    if low_power_hint_due(facts.low_power, current) {
        ui.add_space(4.0);
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(LOW_POWER_HINT)
                        .font(body())
                        .line_height(Some(18.0))
                        .color(t.muted),
                )
                .wrap(),
            );
        });
    }
    ui.add_space(4.0);
    for n in BUFFER_FRAMES {
        let mut job = LayoutJob::default();
        job.wrap.max_width = W - CHECK_W - MS_W - 8.0;
        job.append(delay_name(n), 0.0, TextFormat::simple(label(), t.ink));
        let hint = delay_hint(n);
        if !hint.is_empty() {
            job.append(
                &format!("\n{hint}"),
                0.0,
                TextFormat::simple(body(), t.muted),
            );
        }
        let galley = ui.painter().layout_job(job);
        let (rect, resp) = ui.allocate_exact_size(vec2(W, galley.size().y + 12.0), Sense::click());
        let on = n == current;
        let text = delay_option(n);
        resp.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, on, &text));
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(radius::INPUT), t.hairline);
        }
        if on {
            let c = pos2(rect.left() + 11.0, rect.top() + 6.0 + 8.0);
            p.line(
                vec![
                    c + vec2(-3.5, 0.5),
                    c + vec2(-1.0, 3.0),
                    c + vec2(3.5, -3.0),
                ],
                Stroke::new(1.8, t.accent),
            );
        }
        p.galley(pos2(rect.left() + CHECK_W, rect.top() + 6.0), galley, t.ink);
        p.text(
            pos2(rect.right() - 8.0, rect.top() + 6.0 + 9.0),
            Align2::RIGHT_CENTER,
            format!("+{:.1} ms", delay_ms(n)),
            num(12.0),
            t.muted,
        );
        let tip = format!(
            "{n} frames per buffer. Added delay = 2 buffers × {n} frames ÷ 48,000 frames per second = {:.1} ms, at 48 kHz.",
            delay_ms(n)
        );
        if resp.on_hover_text(tip).clicked() && !on {
            edits.push(Edit::BufferFrames(n));
        }
    }
    ui.separator();
    let larger = BUFFER_FRAMES.iter().any(|&n| n > current);
    let (line, hint) = health::health_line(facts.dropouts, facts.growing, larger);
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.label(egui::RichText::new(line).font(body()).color(t.muted));
        if let Some(hint) = hint {
            ui.add(
                egui::Label::new(egui::RichText::new(hint).font(body()).color(t.warn_ink)).wrap(),
            );
        }
    });
    ui.separator();
    let btn = ui.add_enabled(facts.can_measure, egui::Button::new("Measure delay…"));
    let btn = btn
        .on_hover_text("Plays one very quiet click into the virtual device and times how long it takes to come back.")
        .on_disabled_hover_text("Turn a device on first.");
    if btn.clicked() {
        *measure = true;
    }
}

/// Bring the window forward (it may be hidden in the menu bar).
fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
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
        // Next to config.json; demo mode has no path and keeps its presets in memory.
        let (presets, preset_msg) =
            Presets::load(path.as_deref().map(|p| p.with_file_name("presets.json")));
        let toast = toast.or(preset_msg);
        Self {
            cfg,
            path,
            worker,
            selected: 0,
            graph: GraphState::default(),
            icon_tex: IconTextures::default(),
            renaming: None,
            reveal: false,
            toast,
            save_pending: false,
            vibrancy,
            demo,
            tray: None,
            win: WinState::default(),
            hide_in: 0,
            setup: {
                let mut s = Setup::system();
                if demo {
                    s.skip_intro();
                }
                s
            },
            applied: false,
            undo: None,
            presets,
            preset_pick: None,
            now: 0.0,
            engine_error: None,
            dismissed: health::Dismissed::default(),
            dropouts: health::Dropouts::default(),
            os: Os::system(),
        }
    }

    /// The real engine failed to start: show why until a real one runs.
    pub fn engine_unavailable(mut self, why: Option<String>) -> Self {
        self.engine_error = why;
        self
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
        let mut presets_wanted = Vec::new();
        let mut save_wanted = false;
        let mut access_wanted = false;
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
            self.presets.refresh();
            t.set_presets(&self.presets.names());
            for cmd in t.poll() {
                match cmd {
                    tray::Cmd::Open => events.push(Event::Open),
                    tray::Cmd::Quit => events.push(Event::Quit),
                    tray::Cmd::Preset(name) => presets_wanted.push(name),
                    tray::Cmd::SavePreset => save_wanted = true,
                    tray::Cmd::Permissions => access_wanted = true,
                    tray::Cmd::Login(on) => {
                        if let Err(e) = login::set_enabled(on) {
                            t.set_login_checked(!on);
                            self.toast = Some(format!("Start at Login did not change: {e}"));
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
                        // Not in demo mode, nor once the settings folder was deleted.
                        if self.path.is_some() {
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
        for url in crate::deeplink::take() {
            self.open_link(ctx, &url);
        }
        for name in presets_wanted {
            self.apply_preset(ctx, &name);
        }
        if save_wanted {
            show_window(ctx);
            self.presets.open_save();
        }
        if access_wanted {
            show_window(ctx);
            self.setup.open_access();
        }
    }

    /// A `soundrouter://` link. A problem is shown in the window, which then comes forward.
    fn open_link(&mut self, ctx: &egui::Context, url: &str) {
        log::info!("link: {url:?}");
        match sr_core::preset::parse_url(url) {
            Ok(sr_core::preset::UrlAction::Preset(name)) => self.apply_preset(ctx, &name),
            Err(e) => {
                self.toast = Some(e);
                show_window(ctx);
            }
        }
    }

    /// Apply a preset, or ask first when it needs virtual devices that are not set up (the driver's
    /// list is never changed without a yes). Problems show in the window, which comes forward.
    fn apply_preset(&mut self, ctx: &egui::Context, name: &str) {
        match self.presets.plan_for(&self.cfg, name) {
            Err(e) => {
                self.toast = Some(e);
                show_window(ctx);
            }
            Ok(plan) => {
                let name = self.presets.stored_name(name).unwrap_or(name).to_owned();
                if plan.confirm().is_some() {
                    if let Err(e) = self.presets.ask(&name, plan) {
                        self.toast = Some(e.into());
                    }
                    show_window(ctx);
                } else {
                    self.commit_preset(&name, plan.config);
                }
            }
        }
    }

    /// The user agreed to the device changes: plan again against the config as it is now.
    fn apply_confirmed(&mut self, name: &str) {
        match self.presets.plan_for(&self.cfg, name) {
            Ok(plan) => self.commit_preset(name, plan.config),
            Err(e) => self.toast = Some(e),
        }
    }

    /// Make `cfg` (a validated merge of the preset) the config, undoable like a removal.
    fn commit_preset(&mut self, name: &str, cfg: Config) {
        let count = |c: &Config| {
            c.devices
                .iter()
                .flat_map(|d| &d.sources)
                .filter(|s| matches!(s.kind, SourceKind::Device { .. }))
                .count()
        };
        log::info!("applying preset \"{name}\"");
        let adds_input = count(&cfg) > count(&self.cfg);
        let prev = std::mem::replace(&mut self.cfg, cfg);
        self.toast = None;
        self.undo = Some(Undo {
            verb: "Applied preset",
            name: name.to_owned(),
            prev,
            selected: self.selected,
            until: self.now + UNDO_SECS,
        });
        // As in `commit`: a microphone nobody asked macOS about waits for the setup sheet.
        if adds_input && self.engine_status().mic == TccStatus::Unknown {
            self.applied = false;
        }
        self.selected = self.selected.min(self.cfg.devices.len().saturating_sub(1));
        if self.applied {
            self.worker.apply(&self.cfg);
        }
        self.save_pending = true;
    }

    fn engine_status(&self) -> EngineStatus {
        self.worker.shared.lock().unwrap().status.clone()
    }

    /// What `setup::should_show_setup` decides on: this config, this engine status.
    fn needs(&self, status: &EngineStatus) -> setup::Needs {
        let enabled: Vec<_> = self.cfg.devices.iter().filter(|d| d.enabled).collect();
        setup::Needs {
            driver: status.driver_version.is_some(),
            mic: status.mic,
            capture: status.tcc,
            mic_source: !enabled.is_empty(),
            app_source: enabled
                .iter()
                .flat_map(|d| &d.sources)
                .any(|s| matches!(s.kind, SourceKind::App { .. })),
            seen: self.setup.seen(),
        }
    }

    /// Send the config to the engine once it can take it, and again after the driver came back.
    /// Runs from `logic`, which eframe also calls while the window is hidden, so a `--background`
    /// launch (Start at Login) routes audio without the window ever being shown. Returns whether
    /// the first apply waits for the setup sheet to explain the Microphone prompt.
    fn first_apply(&mut self) -> bool {
        let (ready, status) = {
            let s = self.worker.shared.lock().unwrap();
            (s.status_ready, s.status.clone())
        };
        let needs = self.needs(&status);
        if ready && !needs.driver {
            self.applied = false; // re-apply once a driver (re)appears
        }
        let pending = ready && !self.applied && needs.hold_first_apply();
        if ready && needs.driver && !self.applied && !pending {
            self.applied = true;
            self.worker.apply(&self.cfg);
        }
        pending
    }

    /// The setup sheet and what it asks for.
    fn drive_setup(&mut self, ctx: &egui::Context) {
        let pending = self.first_apply();
        let (ready, status, test, measure, apply_error) = {
            let s = self.worker.shared.lock().unwrap();
            (
                s.status_ready,
                s.status.clone(),
                s.self_test.clone(),
                s.measure.clone(),
                s.apply_error.clone(),
            )
        };
        let missing = setup::should_show_setup(&self.needs(&status));
        // The driver only publishes enabled devices: a disabled one can never pass the test.
        let enabled = |d: &&sr_core::config::VirtualDevice| d.enabled;
        let dev = self
            .cfg
            .devices
            .get(self.selected)
            .filter(enabled)
            .or_else(|| self.cfg.devices.iter().find(enabled));
        let name = dev.map_or("Loopback Audio", |d| d.name.as_str());
        let source = self.setup.source().map(std::path::Path::to_path_buf);
        let diagnostics = || {
            setup::diagnostics(
                &sr_audio::os_version(),
                &status,
                &test,
                apply_error.as_deref(),
                self.cfg.devices.len(),
                source.as_deref(),
            )
        };
        let view = setup::View {
            status: &status,
            ready,
            test: &test,
            measure: &measure,
            device_name: name,
            first_apply_pending: pending,
            missing,
            login: self.setup.wants_login() && (self.os.login_is_on)(),
            now: ctx.input(|i| i.time),
            diagnostics: &diagnostics,
        };
        let id = dev.map(|d| d.id.clone());
        let action = self.setup.show(ctx, &view);
        match action {
            Some(SetupAction::AllowMic | SetupAction::Test) => {
                if action == Some(SetupAction::AllowMic) && !self.applied {
                    self.applied = true;
                    self.worker.apply(&self.cfg);
                }
                match id {
                    Some(id) => self.worker.self_test(&id),
                    None => self
                        .worker
                        .fail_self_test("There is no enabled device to test. Turn one on in the sidebar, then try again."),
                }
            }
            Some(SetupAction::Measure) => self.measure_now(),
            Some(SetupAction::Demo) => {
                self.demo = true;
                self.engine_error = None;
                self.path = None; // never overwrite the saved config with demo edits
                self.presets = Presets::load(None).0; // nor the saved presets
                self.cfg = crate::fake::demo_config();
                // Both index the config just replaced.
                self.selected = 0;
                self.undo = None;
                self.applied = true;
                self.worker.use_demo();
                self.worker.apply(&self.cfg);
            }
            Some(SetupAction::Login(on)) => match (self.os.set_login)(on) {
                Ok(()) => {
                    if let Some(t) = &self.tray {
                        t.set_login_checked(on);
                    }
                }
                Err(e) => self.toast = Some(format!("Start at Login did not change: {e}")),
            },
            Some(SetupAction::SettingsDeleted) => {
                // The settings folder is gone: stop writing it back with every edit.
                self.path = None;
                self.presets = Presets::load(None).0;
                self.save_pending = false;
            }
            None => {}
        }
    }

    /// Time a click through the selected device, else the first enabled one (like the setup test).
    fn measure_now(&mut self) {
        let enabled = |d: &&sr_core::config::VirtualDevice| d.enabled;
        let id = self
            .cfg
            .devices
            .get(self.selected)
            .filter(enabled)
            .or_else(|| self.cfg.devices.iter().find(enabled))
            .map(|d| d.id.clone());
        match id {
            Some(id) => self.worker.measure_delay(&id),
            None => self.worker.fail_measure_delay(
                "There is no enabled device to measure. Turn one on in the sidebar, then try again.",
            ),
        }
    }

    fn commit(&mut self, edits: Vec<Edit>) {
        // A blank name can never be valid (the driver rejects it and then drops every device).
        let mut blank_rename = false;
        let mut edits: Vec<Edit> = edits
            .into_iter()
            .filter(|e| {
                let blank = matches!(e, Edit::Rename(_, n) if n.trim().is_empty());
                blank_rename |= blank;
                !blank
            })
            .collect();
        // Every index was read from this frame's config: a device removal applied first would shift
        // the others (a row rename committed by losing focus to another row's X in the same frame).
        edits.sort_by_key(|e| matches!(e, Edit::RemoveDevice(_)));
        if edits.is_empty() {
            if blank_rename {
                self.toast = Some(BLANK_NAME.into());
            }
            return;
        }
        let adds_input = edits
            .iter()
            .any(|e| matches!(e, Edit::AddSource(_, SourceKind::Device { .. }, _)));
        let removed = {
            let s = self.worker.shared.lock().unwrap();
            let installed = self.graph.installed.get().map_or(&[][..], Vec::as_slice);
            edits
                .iter()
                .find_map(|e| graph::removed_title(&self.cfg, e, &s.hardware, &s.apps, installed))
        };
        let prev = self.cfg.clone();
        let prev_selected = self.selected;
        let removed_dev = edits.iter().find_map(|e| match e {
            Edit::RemoveDevice(d) => Some(*d),
            _ => None,
        });
        let mut select_new = false;
        for e in edits {
            select_new |= e == Edit::NewDevice;
            apply(&mut self.cfg, e);
        }
        if let Err(e) = self.cfg.validate() {
            self.cfg = prev;
            self.toast = Some(format!("That change was not applied: {e}."));
            return;
        }
        self.toast = blank_rename.then(|| BLANK_NAME.to_string());
        self.undo = removed.map(|name| Undo {
            verb: "Removed",
            name,
            prev,
            selected: prev_selected,
            until: self.now + UNDO_SECS,
        });
        // A microphone is read through the Microphone permission: if macOS has not been asked yet,
        // hold the apply and let the setup sheet explain and ask first.
        if adds_input && self.engine_status().mic == TccStatus::Unknown {
            self.applied = false;
        }
        if select_new {
            self.selected = self.cfg.devices.len() - 1;
            self.reveal = true;
        }
        // Removing a device above the selected one shifts the selection's index down with it.
        let removed_above = removed_dev.is_some_and(|d| d < self.selected);
        if removed_above {
            self.selected -= 1;
        }
        let last = self.cfg.devices.len().saturating_sub(1);
        if self.selected > last {
            self.selected = last;
        }
        if self.applied {
            self.worker.apply(&self.cfg);
        }
        self.save_pending = true;
    }

    /// Take back the last removal: the saved config goes through the normal apply and save.
    fn undo_removal(&mut self) {
        let Some(u) = self.undo.take() else { return };
        self.cfg = u.prev;
        self.selected = u.selected.min(self.cfg.devices.len().saturating_sub(1));
        self.reveal = true;
        if self.applied {
            self.worker.apply(&self.cfg);
        }
        self.save_pending = true;
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, t: &Tokens, edits: &mut Vec<Edit>) {
        // Opaque: a translucent sidebar over the blur view made the row text and the cable-coloured
        // tiles shift with the wallpaper, and the flat panel is part of the look (docs/DESIGN.md).
        egui::Panel::left("devices")
            .exact_size(SIDEBAR_W)
            .resizable(false)
            .frame(egui::Frame::default().fill(t.panel))
            .show(ui, |ui| {
                let full = ui.max_rect();
                ui.painter().text(
                    pos2(full.left() + 84.0, full.top() + 23.0),
                    Align2::LEFT_CENTER,
                    "Devices",
                    section(),
                    t.ink,
                );
                ui.painter().hline(
                    full.x_range(),
                    full.top() + TITLEBAR_H + 0.5,
                    Stroke::new(1.0, t.hairline.gamma_multiply(0.6)),
                );
                let list = Rect::from_min_max(
                    pos2(full.left(), full.top() + TITLEBAR_H + 1.0),
                    pos2(full.right(), full.bottom() - FOOTER_H),
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
                // Footer: the one primary action, kept quiet so the cables stay the loudest thing.
                let foot =
                    Rect::from_min_max(pos2(full.left(), full.bottom() - FOOTER_H), full.max);
                ui.painter().hline(
                    foot.x_range(),
                    foot.top() + 0.5,
                    Stroke::new(1.0, t.hairline.gamma_multiply(0.6)),
                );
                let cy = foot.center().y;
                let new = ui.interact(
                    Rect::from_min_size(pos2(foot.left() + 10.0, cy - 14.0), vec2(132.0, 28.0)),
                    Id::unique("new_dev"),
                    Sense::click(),
                );
                name_button(&new, NEW_DEVICE);
                if new.hovered() {
                    ui.painter().rect_filled(
                        new.rect,
                        CornerRadius::same(radius::PILL),
                        t.selected,
                    );
                }
                plus_glyph(
                    ui.painter(),
                    pos2(foot.left() + 28.0, cy),
                    t.accent_ink,
                    false,
                );
                ui.painter().text(
                    pos2(foot.left() + 44.0, cy),
                    Align2::LEFT_CENTER,
                    NEW_DEVICE,
                    label(),
                    t.accent_ink,
                );
                if new.clicked() {
                    edits.push(Edit::NewDevice);
                }
            });
    }

    fn device_row(&mut self, ui: &mut egui::Ui, i: usize, t: &Tokens, edits: &mut Vec<Edit>) {
        let dev = &self.cfg.devices[i];
        let (row, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
        name_button(&resp, &format!("Select {}", dev.name));
        if resp.clicked() {
            self.selected = i;
        }
        if i == self.selected && std::mem::take(&mut self.reveal) {
            resp.scroll_to_me(None);
        }
        let inner = row.shrink2(vec2(8.0, 4.0));
        let cable = cable_color(i);
        if i == self.selected {
            ui.painter()
                .rect_filled(inner, CornerRadius::same(radius::ROW), t.selected);
            // The accent bar sits on the straight part of the rounded edge.
            let bar = Rect::from_x_y_ranges(
                inner.left()..=(inner.left() + 3.0),
                (inner.top() + radius::ROW as f32 + 2.0)
                    ..=(inner.bottom() - radius::ROW as f32 - 2.0),
            );
            ui.painter().rect_filled(bar, CornerRadius::same(2), cable);
        }
        let dim = if dev.enabled { 1.0 } else { 0.55 };
        let icon = Rect::from_min_size(
            pos2(inner.left() + 10.0, inner.top() + 11.0),
            vec2(30.0, 30.0),
        );
        ui.painter()
            .rect_filled(icon, CornerRadius::same(8), cable.gamma_multiply(dim));
        loops(ui.painter(), icon, Color32::WHITE.gamma_multiply(dim));
        if let Some(n) = self.dropouts.growing(&dev.id, self.now) {
            let ring = if i == self.selected {
                t.selected
            } else {
                t.panel
            };
            health::dropout_dot(
                ui,
                icon.right_top() + vec2(-1.0, 1.0),
                ring,
                t,
                &dev.name,
                n,
            );
        }
        let name_pos = pos2(icon.right() + 10.0, inner.top() + 18.0);
        let name_rect = Rect::from_min_size(
            pos2(name_pos.x, name_pos.y - 10.0),
            vec2(inner.right() - 72.0 - name_pos.x, 20.0),
        );
        let renaming_here = matches!(&self.renaming, Some((r, _)) if *r == i);
        if renaming_here {
            let Some((_, buf)) = &mut self.renaming else {
                return;
            };
            let te = ui.put(
                name_rect,
                TextEdit::singleline(buf)
                    .font(label())
                    .margin(Margin::ZERO)
                    .frame(egui::Frame::NONE)
                    .text_color(t.ink)
                    .char_limit(MAX_NAME_CHARS),
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
            let g =
                ui.painter()
                    .layout_no_wrap(dev.name.clone(), label(), t.ink.gamma_multiply(dim));
            let shown = g.size().x.min(name_rect.width());
            ui.painter().with_clip_rect(name_rect).galley(
                pos2(name_pos.x, name_pos.y - g.size().y / 2.0),
                g,
                t.ink,
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
        let status = self.engine_status();
        // A quiet live readout in the channel-count line: beside the count when the row has room,
        // else (apps captured: the longer readout) in its place.
        let ink = t.muted.gamma_multiply(dim);
        let line_y = inner.top() + 36.0;
        let chans =
            ui.painter()
                .layout_no_wrap(format!("{} channels", dev.channels), caption(), ink);
        let mut show_chans = true;
        if let Some(ms) = device_delay_ms(&status, &dev.id) {
            use egui::text::{LayoutJob, TextFormat};
            let apps = device_tap_ms(&status, &dev.id);
            let readout = |with_apps: bool| {
                let mut job = LayoutJob::default();
                job.append(&delay_readout(ms), 0.0, TextFormat::simple(num(11.0), ink));
                if let (true, Some(a)) = (with_apps, apps) {
                    // The second part is quieter than the first.
                    let quiet = TextFormat::simple(num(11.0), ink.gamma_multiply(0.7));
                    job.append(&format!(" {}", apps_readout(a)), 0.0, quiet);
                }
                ui.painter().layout_job(job)
            };
            let full = readout(true);
            let (g, x) = if apps.is_some() && name_pos.x + full.size().x <= inner.right() - 22.0 {
                show_chans = false;
                (full, name_pos.x)
            } else {
                let g = readout(false);
                let x = inner.right() - 30.0 - g.size().x;
                (g, x)
            };
            let rect = Rect::from_min_size(pos2(x, line_y - g.size().y / 2.0), g.size());
            if !show_chans || rect.left() > name_pos.x + chans.size().x + 8.0 {
                let label = match apps {
                    Some(a) if !show_chans => {
                        format!("{} {}, {}", dev.name, delay_readout(ms), apps_readout(a))
                    }
                    _ => format!("{} {}", dev.name, delay_readout(ms)),
                };
                ui.painter().galley(rect.min, g, t.muted);
                let hit = ui.interact(rect.expand(2.0), Id::unique(("delay", i)), Sense::hover());
                hit.widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
                hit.on_hover_text(delay_readout_tip(apps));
            }
        }
        if show_chans {
            let at = pos2(name_pos.x, line_y - chans.size().y / 2.0);
            ui.painter().galley(at, chans, ink);
        }
        let state = status
            .per_device
            .into_iter()
            .find_map(|(id, s)| (id == dev.id).then_some(s))
            .unwrap_or_default();
        if let Some((text, color)) = state_look(&state, t) {
            let c = pos2(inner.right() - 16.0, inner.top() + 36.0);
            if state != DeviceState::Running {
                ui.painter().circle_filled(c, 4.0, color);
            }
            let dot = ui.interact(
                Rect::from_center_size(c, vec2(14.0, 14.0)),
                Id::unique(("state", i)),
                Sense::hover(),
            );
            let label = format!("{} status: {text}", dev.name);
            dot.widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
            dot.on_hover_text(text);
        }
        let x_at = pos2(inner.right() - 16.0, inner.top() + 18.0);
        let x = ui.interact(
            Rect::from_center_size(x_at, vec2(18.0, 18.0)),
            Id::unique(("dev_x", i)),
            Sense::click(),
        );
        name_button(&x, &format!("Remove {}", dev.name));
        xmark(
            ui.painter(),
            x_at,
            if x.hovered() { t.bad_ink } else { t.muted },
        );
        // A double-click's second click would remove the row that moved up under the pointer.
        if x.clicked() && !x.double_clicked() {
            edits.push(Edit::RemoveDevice(i));
        }
        x.on_hover_text(format!("Remove {}", dev.name));
        let sw = Rect::from_center_size(pos2(x_at.x - 9.0 - 10.0 - 16.0, x_at.y), vec2(32.0, 18.0));
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
        let vy = inner.top() + 57.0;
        let mute = speaker(
            ui,
            pos2(inner.left() + 22.0, vy),
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
            (inner.left() + 38.0)..=(inner.right() - 52.0),
            (vy - 7.5)..=(vy + 7.5),
        );
        let slider = ui.scope_builder(UiBuilder::new().max_rect(sl), |ui| {
            // egui sizes a slider from the body text height and `interact_size`; shrink both so the
            // knob painted over its handle can be the sidebar's 14 px one.
            let style = ui.style_mut();
            style.spacing.interact_size.y = sl.height();
            if let Some(body) = style.text_styles.get_mut(&TextStyle::Body) {
                body.size = 8.0;
            }
            ui.spacing_mut().slider_width = sl.width() - 4.0;
            ui.add(Slider::new(&mut pct, 0.0..=100.0).show_value(false))
        });
        name_widget(ui, &slider.inner, &format!("{} volume", dev.name));
        slider_knob(ui.painter(), slider.inner.rect, pct / 100.0, t);
        if (pct - before).abs() > 1e-3 {
            edits.push(Edit::Volume(i, pct_to_db(pct)));
        }
        ui.painter().text(
            pos2(x_at.x + 4.0, vy),
            Align2::RIGHT_CENTER,
            format!("{pct:.0}%"),
            num(11.0),
            t.muted,
        );
    }

    /// The Presets button in the titlebar zone, right-aligned; its menu lists the presets.
    fn presets_button(&mut self, ui: &mut egui::Ui, t: &Tokens, full: Rect) {
        let w = setup::footer_width(ui, presets::BUTTON);
        let rect = Rect::from_min_size(
            pos2(full.right() - 18.0 - w, full.top() + 10.0),
            vec2(w, 26.0),
        );
        let resp = setup::footer_button(ui, rect, "presets", presets::BUTTON, t);
        resp.clone()
            .on_hover_text("Save the whole setup under a name, and switch between setups");
        let mut picked = None;
        egui::Popup::menu(&resp).show(|ui| picked = self.presets.menu(ui, t));
        self.preset_pick = picked.or(self.preset_pick.take());
    }

    fn main_pane(&mut self, ui: &mut egui::Ui, t: &Tokens, edits: &mut Vec<Edit>) {
        egui::Panel::bottom("bottom")
            .exact_size(FOOTER_H)
            .resizable(false)
            .frame(egui::Frame::default().fill(t.panel))
            .show(ui, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(
                    r.x_range(),
                    r.top() + 0.5,
                    Stroke::new(1.0, t.hairline.gamma_multiply(0.6)),
                );
                let has = self.selected < self.cfg.devices.len();
                let remove_w = setup::footer_width(ui, REMOVE_DEVICE);
                let btn = Rect::from_center_size(
                    pos2(r.left() + 26.0 + remove_w / 2.0, r.center().y),
                    vec2(remove_w, 26.0),
                );
                let resp = ui.interact(btn, Id::unique("delete"), Sense::click());
                name_button(&resp, REMOVE_DEVICE);
                let fill = if resp.hovered() && has {
                    t.hairline
                } else {
                    t.raised
                };
                let a = if has { 1.0 } else { 0.4 };
                ui.painter().rect(
                    btn,
                    CornerRadius::same(radius::PILL),
                    fill.gamma_multiply(a),
                    Stroke::new(1.0, t.hairline),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    btn.center(),
                    Align2::CENTER_CENTER,
                    REMOVE_DEVICE,
                    label(),
                    t.ink.gamma_multiply(a),
                );
                if has && resp.clicked() && !resp.double_clicked() {
                    edits.push(Edit::RemoveDevice(self.selected));
                }
                let real_driver = self
                    .engine_status()
                    .driver_version
                    .is_some_and(|v| v != crate::fake::DEMO_DRIVER);
                let mut right = r.right() - 18.0;
                let by = r.center().y - 13.0;
                if real_driver {
                    // Uninstall is destructive and rare: it lives behind a quiet "…" menu.
                    let more = Rect::from_min_size(pos2(right - 28.0, by), vec2(28.0, 26.0));
                    let resp = ui.interact(more, Id::unique("more"), Sense::click());
                    name_button(&resp, "More actions");
                    let tint = if resp.hovered() { t.ink } else { t.muted };
                    if resp.hovered() {
                        ui.painter().rect_filled(
                            more,
                            CornerRadius::same(radius::PILL),
                            t.hairline,
                        );
                    }
                    for dx in [-5.0, 0.0, 5.0] {
                        ui.painter()
                            .circle_filled(more.center() + vec2(dx, 0.0), 1.6, tint);
                    }
                    resp.clone().on_hover_text("More actions");
                    egui::Popup::menu(&resp).show(|ui| {
                        ui.set_min_width(140.0);
                        if ui.button("Permissions").clicked() {
                            self.setup.open_access();
                        }
                        ui.separator();
                        if ui
                            .button(egui::RichText::new("Uninstall…").color(t.bad_ink))
                            .clicked()
                        {
                            self.setup.ask_uninstall();
                        }
                    });
                    let test_w = setup::footer_width(ui, TEST_SETUP);
                    let test = Rect::from_min_size(
                        pos2(more.left() - 8.0 - test_w, by),
                        vec2(test_w, 26.0),
                    );
                    if setup::footer_button(ui, test, "test_setup", TEST_SETUP, t).clicked() {
                        self.setup.open_test();
                    }
                    right = test.left() - 8.0;
                }
                let label = delay_label(self.cfg.buffer_frames);
                let chip_w = setup::footer_width(ui, &label);
                let latency = Rect::from_min_size(pos2(right - chip_w, by), vec2(chip_w, 26.0));
                let resp = setup::footer_button(ui, latency, "latency", &label, t);
                let live = self.engine_status();
                // The selected device's reading, else any device's.
                let reading = self
                    .cfg
                    .devices
                    .get(self.selected)
                    .into_iter()
                    .chain(&self.cfg.devices)
                    .find_map(|d| Some((d.name.as_str(), device_delay_ms(&live, &d.id)?)));
                resp.clone()
                    .on_hover_text(delay_chip_tip(self.cfg.buffer_frames, reading));
                let facts = DelayFacts {
                    low_power: live.low_power_mode,
                    can_measure: self.cfg.devices.iter().any(|d| d.enabled),
                    dropouts: self.dropouts.total(),
                    growing: self.dropouts.any_growing(self.now),
                };
                let mut measure = false;
                egui::Popup::menu(&resp).show(|ui| {
                    delay_menu(ui, t, self.cfg.buffer_frames, &facts, edits, &mut measure)
                });
                if measure {
                    self.setup.open_measure();
                    self.measure_now();
                }
                let reserved = r.right() - latency.left() + 12.0;
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
                // A standing error or warning waits out the few seconds the undo is offered.
                if self.toast.is_none()
                    && let Some(u) = &self.undo
                {
                    let x = btn.right() + 18.0;
                    let width = r.right() - 18.0 - x - reserved;
                    let text = format!("{} {}.", u.verb, u.name);
                    let font = body();
                    let tw = ui
                        .painter()
                        .layout_no_wrap(text.clone(), font.clone(), t.muted)
                        .size()
                        .x
                        .min((width - 52.0).max(0.0));
                    graph::text_left(ui, x, r.center().y, &text, font.clone(), t.muted, tw);
                    ui.interact(
                        Rect::from_min_size(pos2(x, r.top()), vec2(tw, r.height())),
                        Id::unique("removed"),
                        Sense::hover(),
                    )
                    .widget_info(|| {
                        WidgetInfo::labeled(Role::Label, true, format!("{} {}", u.verb, u.name))
                    });
                    let hit = ui.interact(
                        Rect::from_center_size(
                            pos2(x + tw + 6.0 + 18.0, r.center().y),
                            vec2(36.0, 22.0),
                        ),
                        Id::unique("undo"),
                        Sense::click(),
                    );
                    name_button(&hit, "Undo");
                    ui.painter().text(
                        hit.rect.left_center(),
                        Align2::LEFT_CENTER,
                        "Undo",
                        font,
                        if hit.hovered() { t.ink } else { t.accent_ink },
                    );
                    if hit.clicked() {
                        // The frame's edits index the config being replaced (a row rename that
                        // lost focus to this click): drop them rather than hit another device.
                        edits.clear();
                        self.undo_removal();
                    }
                    return;
                }
                let (msg, color) = match status {
                    Some(m) => (m, t.bad_ink),
                    None if self.demo => (
                        "Demo mode: the audio is synthetic and nothing is saved.".to_string(),
                        t.muted,
                    ),
                    None => return,
                };
                let x = btn.right() + 18.0;
                let width = r.right() - 18.0 - x - reserved;
                graph::text_left(ui, x, r.center().y, &msg, body(), color, width);
                let status = ui.interact(
                    Rect::from_min_max(
                        pos2(x, r.top()),
                        pos2(r.right() - 18.0 - reserved, r.bottom()),
                    ),
                    Id::unique("status"),
                    Sense::hover(),
                );
                status.widget_info(|| WidgetInfo::labeled(Role::Label, true, &msg));
                status.on_hover_text(msg);
            });

        let status = self.engine_status();
        let app_source = self
            .cfg
            .devices
            .iter()
            .filter(|d| d.enabled)
            .flat_map(|d| &d.sources)
            .any(|s| matches!(s.kind, SourceKind::App { .. }));
        let list = health::issues(&status, app_source, |id| {
            self.cfg
                .devices
                .iter()
                .find(|d| d.id == id)
                .map_or_else(|| id.to_owned(), health::subject)
        });
        if let Some((issue, more)) = self.dismissed.pick(&list)
            && !self.demo
        {
            let clicked = egui::Panel::bottom("health")
                .resizable(false)
                .frame(egui::Frame::default().fill(t.raised))
                .show(ui, |ui| health::banner(ui, t, issue, more))
                .inner;
            if clicked.dismiss {
                self.dismissed.dismiss(&issue.key);
            }
            if clicked.fix == Some(health::Fix::Privacy) {
                (self.os.open_url)(AUDIO_CAPTURE_SETTINGS);
            }
        }

        let waiting = self
            .engine_status()
            .per_device
            .iter()
            .any(|(_, s)| *s == DeviceState::WaitingForPermission);
        if waiting && !self.setup.shown() {
            egui::Panel::top("permission")
                .frame(
                    egui::Frame::default()
                        .fill(t.raised)
                        .inner_margin(Margin::same(10)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(PERMISSION_BANNER);
                        if ui.button(OPEN_SETTINGS).clicked() {
                            let _ = std::process::Command::new("/usr/bin/open")
                                .arg(AUDIO_CAPTURE_SETTINGS)
                                .spawn();
                        }
                    });
                });
        }

        if let Some(why) = self.engine_error.clone().filter(|_| !self.demo) {
            egui::Panel::top("engine_unavailable")
                .frame(
                    egui::Frame::default()
                        .fill(t.raised)
                        .inner_margin(Margin::same(10)),
                )
                .show(ui, |ui| {
                    ui.colored_label(
                        t.bad_ink,
                        format!(
                            "SoundRouter could not start its audio engine ({why}). Nothing is routed. Quit and reopen SoundRouter to try again."
                        ),
                    );
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(t.canvas))
            .show(ui, |ui| {
                let full = ui.max_rect();
                self.presets_button(ui, t, full);
                let Some(dev) = self.cfg.devices.get(self.selected) else {
                    if graph::no_devices(ui, t, full) {
                        edits.push(Edit::NewDevice);
                    }
                    return;
                };
                // Title (editable) in the titlebar zone.
                let title_rect = Rect::from_min_size(
                    pos2(full.left() + 26.0, full.top() + 7.0),
                    vec2(360.0, 32.0),
                );
                let mut name = dev.name.clone();
                let te = ui.put(
                    title_rect,
                    TextEdit::singleline(&mut name)
                        .font(device_title())
                        .margin(Margin::ZERO)
                        .frame(egui::Frame::NONE)
                        .text_color(t.ink)
                        .char_limit(MAX_NAME_CHARS),
                );
                name_widget(ui, &te, "Device name");
                let w = ui
                    .painter()
                    .layout_no_wrap(name.clone(), device_title(), t.ink)
                    .size()
                    .x;
                pencil(
                    ui.painter(),
                    pos2(
                        title_rect.left() + w.min(330.0) + 16.0,
                        title_rect.center().y,
                    ),
                    if te.hovered() || te.has_focus() {
                        t.ink
                    } else {
                        t.muted
                    },
                );
                if name != dev.name {
                    edits.push(Edit::Rename(self.selected, name));
                }

                let snap = self.worker.shared.lock().unwrap();
                let hw = snap.hardware.clone();
                let mic = snap.status.mic;
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
                                mic,
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
        let visible = ctx.input(|i| i.viewport().visible().unwrap_or(true));
        self.worker.set_visible(visible);
        self.first_apply();
        self.handle_window(ctx);
    }

    /// Cmd-Q ends the process without dropping `App`: stop the worker so the engine shuts down.
    fn on_exit(&mut self) {
        // A save waits for the mouse button to come up (slider drags); quitting must not drop it.
        if self.save_pending
            && let Some(p) = &self.path
            && let Err(e) = store::save(p, &self.cfg)
        {
            log::warn!("could not save config on quit: {e}");
        }
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
        ctx.global_style_mut(|s| crate::theme::apply_style(s, &t));
        // Frames come from the worker (meters changed) and from animations; nothing polls here.
        self.now = ctx.input(|i| i.time);
        if self.undo.as_ref().is_some_and(|u| self.now > u.until) {
            self.undo = None;
        }
        if let Some(u) = &self.undo {
            ctx.request_repaint_after(Duration::from_secs_f64(u.until - self.now + 0.05));
        }
        self.dropouts.observe(&self.engine_status().xruns, self.now);
        // No repaint is scheduled for the amber dot to clear: the worker's status refresh (2 s) repaints.

        let prev_selected = self.selected;
        let learned = {
            let s = self.worker.shared.lock().unwrap();
            let installed = self.graph.installed.get().map_or(&[][..], Vec::as_slice);
            crate::model::learn_names(&mut self.cfg, &s.hardware, &s.apps, installed)
        };
        self.save_pending |= learned;
        // A click outside an open menu only closes the menu, as on macOS: without this it would
        // also remove the card or wire it landed on. Menus live on the Foreground layer.
        let dismissing = egui::Popup::is_any_open(&ctx)
            && ctx.input(|i| i.pointer.any_click())
            && ctx
                .input(|i| i.pointer.interact_pos())
                .and_then(|p| ctx.layer_id_at(p))
                .is_none_or(|l| l.order != egui::Order::Foreground);
        let mut edits = Vec::new();
        self.sidebar(ui, &t, &mut edits);
        self.main_pane(ui, &t, &mut edits);
        if self.selected != prev_selected {
            self.graph.peaks.clear();
            self.graph.drag = None;
        }
        if dismissing {
            edits.clear();
        }
        self.commit(edits);
        if let Some(name) = self.preset_pick.take() {
            self.apply_preset(&ctx, &name);
        }
        self.drive_setup(&ctx);
        let out = self.presets.show(&ctx, &t, &self.cfg);
        if let Some(msg) = out.toast {
            self.toast = Some(msg);
        }
        match out.apply {
            Some((name, true)) => self.apply_confirmed(&name),
            Some((name, false)) => self.apply_preset(&ctx, &name),
            None => {}
        }

        if self.save_pending && !ctx.input(|i| i.pointer.any_down()) {
            self.save_pending = false;
            if let Some(p) = &self.path
                && let Err(e) = store::save(p, &self.cfg)
            {
                self.toast = Some(format!(
                    "Could not save your settings: {e}. Changes last until you quit."
                ));
            }
        }
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "presets_tests.rs"]
mod preset_tests;

#[cfg(test)]
#[path = "health_tests.rs"]
mod health_tests;
