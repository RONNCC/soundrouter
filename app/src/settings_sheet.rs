//! The Settings sheet: one scrolling body with section headings and a single "Done". Every control
//! applies at once (nothing waits for Done), so Esc and Done both just close it. The sheet only
//! draws and reports what was changed (`Change`); the app applies, saves and sends engine options.
use crate::model::{LOW_POWER_HINT, delay_ms, delay_name, delay_option, low_power_hint_due};
use crate::settings::Settings;
use crate::sheet::{self, Glyph, Kind, Sheet};
use crate::theme::{Tokens, label, num, radius, strong};
use crate::widgets::{halo, hit, switch};
use eframe::egui::{
    self, Context, CornerRadius, Id, Rect, RichText, Sense, Stroke, Ui, WidgetInfo,
    accesskit::Role, pos2, vec2,
};
use sr_core::backend::{EngineStatus, ReadAheadState};
use sr_core::config::BUFFER_FRAMES;

pub const TITLE: &str = "Settings";
pub const LOGIN: &str = "Start at login";
pub const MENU_BAR: &str = "Show in menu bar";
pub const KEEP_RUNNING: &str = "Keep routing audio when the window is closed";
pub const CLOSING_STOPS: &str = "Closing the window stops all routing.";
pub const PERMISSIONS: &str = "Permissions…";
pub const LOWER_DELAY: &str = "Lower delay (experimental)";
pub const NEEDS_DRIVER: &str = "Needs the newer audio driver";
pub const UPDATE_DRIVER: &str = "Update driver";
pub const TRIPPED: &str = "Turned itself off after driver dropouts";
pub const TRY_AGAIN: &str = "Try again";
pub const SPLIT: &str = "Split app capture from the main path";
pub const SPLIT_NOTE: &str = "Keeps mics and pass-thru fast; captured apps take their own path. Off puts everything in one path, about 20 ms slower for mics and pass-thru. Changing this pauses audio briefly.";
pub const MARGIN: &str = "App capture safety margin";
pub const MARGIN_1: &str = "1 buffer (default)";
pub const MARGIN_2: &str = "2 buffers (fewer dropouts, adds one buffer of delay)";
pub const COPY_DIAGNOSTICS: &str = "Copy diagnostics";
pub const SHOW_LOG: &str = "Show log in Finder";
pub const RESTORE: &str = "Restore default settings";
pub const RESTORE_ASK: &str = "Put every setting on this sheet back to its default? Your routing and presets are not changed.";
pub const DONE: &str = "Done";

/// What the sheet remembers between frames.
#[derive(Default)]
pub struct State {
    pub open: bool,
    /// "Restore default settings" asked "are you sure?".
    confirm_restore: bool,
    /// "Copy diagnostics" was pressed since the sheet opened.
    copied: bool,
}

impl State {
    pub fn open(&mut self) {
        *self = Self {
            open: true,
            ..Self::default()
        };
    }

    /// An open sheet already asking "Restore default settings?", for snapshots.
    #[cfg(all(test, feature = "snapshots"))]
    pub fn ask_restore(&mut self) {
        self.open = true;
        self.confirm_restore = true;
    }
}

/// The state and the list of changes, as the sheet's closures share them.
struct Ctx<'a> {
    state: &'a mut State,
    out: &'a mut Vec<Change>,
}

/// What the app tells the sheet each frame.
pub struct View<'a> {
    pub settings: &'a Settings,
    pub status: &'a EngineStatus,
    /// Start at login is on (the LaunchAgent file exists).
    pub login: bool,
    /// `Config::buffer_frames`.
    pub buffer_frames: u32,
    /// App capture can be split from the main path (a real engine, not the demo).
    pub split_available: bool,
    /// Driver overloads since lower delay turned on (`health::ReadAheadGuard`).
    pub overloads_since_on: u64,
}

/// What the user changed or asked for. The app applies each one and saves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Login(bool),
    MenuBar(bool),
    KeepRunning(bool),
    Permissions,
    /// The same value as the footer delay menu's `Edit::BufferFrames`.
    BufferFrames(u32),
    ReadAhead(bool),
    /// Clears the trip and turns lower delay on again.
    TryAgain,
    UpdateDriver,
    TapSplit(bool),
    /// 1 or 2 buffers of safety margin.
    Cushion(u8),
    CopyDiagnostics,
    ShowLog,
    Restore,
}

/// How the lower-delay switch reads, from the driver and the safety net.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadAheadLook {
    /// The installed driver has no such switch.
    Unsupported,
    /// The safety net turned it off and it waits for "Try again".
    Tripped,
    Off,
    On,
    /// Asked for; the status poll has not caught up.
    TurningOn,
    TurningOff,
}

pub fn read_ahead_look(state: ReadAheadState, wanted: bool, tripped: bool) -> ReadAheadLook {
    match (state, wanted, tripped) {
        (ReadAheadState::Unsupported, ..) => ReadAheadLook::Unsupported,
        (_, false, true) => ReadAheadLook::Tripped,
        (ReadAheadState::Off, true, _) => ReadAheadLook::TurningOn,
        (ReadAheadState::On, false, _) => ReadAheadLook::TurningOff,
        (ReadAheadState::Off, false, false) => ReadAheadLook::Off,
        (ReadAheadState::On, true, _) => ReadAheadLook::On,
    }
}

/// The status line under the lower-delay switch.
pub fn read_ahead_text(look: ReadAheadLook) -> &'static str {
    match look {
        ReadAheadLook::Unsupported => NEEDS_DRIVER,
        ReadAheadLook::Tripped => TRIPPED,
        ReadAheadLook::Off => "Off",
        ReadAheadLook::On => "On",
        ReadAheadLook::TurningOn => "Turning on…",
        ReadAheadLook::TurningOff => "Turning off…",
    }
}

/// "Driver dropouts since on: 3", shown while it is on.
pub fn dropouts_text(n: u64) -> String {
    format!("Driver dropouts since on: {n}")
}

/// The note under "Keep routing audio when the window is closed".
fn keep_running_note(settings: &Settings) -> (&'static str, bool) {
    match (settings.keep_running_on_close, settings.show_in_menu_bar) {
        (false, _) => (CLOSING_STOPS, true),
        (true, true) => (
            "SoundRouter keeps running in the menu bar. Quit it from there.",
            false,
        ),
        (true, false) => (
            "SoundRouter keeps running. Click its Dock icon to bring the window back.",
            false,
        ),
    }
}

fn heading(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(text).font(strong()).color(t.ink));
    ui.painter().hline(
        ui.max_rect().x_range(),
        ui.cursor().top() + 1.0,
        Stroke::new(1.0, t.hairline.gamma_multiply(0.7)),
    );
    ui.add_space(6.0);
}

/// One setting: label and a switch on the right, a note under the label. The label is painted (so a
/// screen reader meets it once, as the switch's name), the whole row is the click target.
fn toggle(
    ui: &mut Ui,
    t: &Tokens,
    key: &str,
    name: &str,
    text: Option<(&str, bool)>,
    on: bool,
) -> bool {
    let new = toggle_row(ui, t, key, name, text, on);
    ui.add_space(6.0);
    new
}

/// `toggle` without the space after it, for a row with its own status line under it.
fn toggle_row(
    ui: &mut Ui,
    t: &Tokens,
    key: &str,
    name: &str,
    text: Option<(&str, bool)>,
    on: bool,
) -> bool {
    const SWITCH_W: f32 = 32.0;
    let w = ui.available_width();
    let text_w = w - SWITCH_W - 20.0;
    let top = ui.cursor().min;
    let galley = ui.painter().layout(name.to_owned(), label(), t.ink, text_w);
    let (lr, _) = ui.allocate_exact_size(vec2(text_w, galley.size().y.max(18.0)), Sense::hover());
    ui.painter().galley(lr.min, galley, t.ink);
    if let Some((text, warn)) = text {
        ui.scope(|ui| {
            ui.set_max_width(text_w);
            ui.add(
                egui::Label::new(
                    RichText::new(text)
                        .size(13.0)
                        .line_height(Some(18.0))
                        .color(if warn { t.warn_ink } else { t.muted }),
                )
                .wrap(),
            );
        });
    }
    let row = Rect::from_min_max(top, pos2(top.x + w, ui.cursor().top()));
    let sw = Rect::from_min_size(
        pos2(row.right() - SWITCH_W, top.y + 0.0),
        vec2(SWITCH_W, 18.0),
    );
    switch(ui, sw, row, Id::unique(("settings", key)), on, t, name)
}

/// One choice of several, a radio dot and its text; `right` is a number in the right-hand column.
fn radio(
    ui: &mut Ui,
    t: &Tokens,
    key: (&str, u32),
    name: &str,
    text: &str,
    right: Option<String>,
    on: bool,
) -> bool {
    const DOT_W: f32 = 26.0;
    let w = ui.available_width();
    let right_w = if right.is_some() { 70.0 } else { 0.0 };
    let galley = ui
        .painter()
        .layout(text.to_owned(), label(), t.ink, w - DOT_W - right_w - 8.0);
    let (rect, _) =
        ui.allocate_exact_size(vec2(w, (galley.size().y + 10.0).max(28.0)), Sense::hover());
    let resp = hit(ui, rect, Id::unique(("settings_radio", key.0, key.1)));
    resp.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, on, name));
    if resp.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(radius::INPUT),
            t.hairline.gamma_multiply(0.6),
        );
    }
    halo(ui, &resp, t);
    let c = pos2(rect.left() + 11.0, rect.center().y);
    let p = ui.painter();
    p.circle_stroke(
        c,
        6.5,
        Stroke::new(1.5, if on { t.accent } else { t.muted }),
    );
    if on {
        p.circle_filled(c, 3.5, t.accent);
    }
    p.galley(
        pos2(rect.left() + DOT_W, rect.center().y - galley.size().y / 2.0),
        galley,
        t.ink,
    );
    if let Some(r) = right {
        p.text(
            pos2(rect.right() - 8.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            r,
            num(12.0),
            t.muted,
        );
    }
    resp.clicked() && !on
}

fn subhead(ui: &mut Ui, t: &Tokens, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).font(label()).color(t.ink));
}

/// The sheet. `Done` and Esc close it; everything else is reported in the return value.
pub fn show(ctx: &Context, t: &Tokens, state: &mut State, v: &View) -> Vec<Change> {
    let mut out = Vec::new();
    let s = v.settings;
    let look = read_ahead_look(
        v.status.read_ahead,
        s.engine.read_ahead,
        s.read_ahead_tripped,
    );
    sheet::modal(
        ctx,
        t,
        &Sheet {
            id: "settings_sheet",
            glyph: Glyph::App,
            title: TITLE,
            intro: "",
            buttons: true,
        },
        &mut Ctx {
            state,
            out: &mut out,
        },
        |ui, c| {
            let (state, out) = (&mut *c.state, &mut *c.out);
            // General
            heading(ui, t, "General");
            if toggle(
                ui,
                t,
                "login",
                LOGIN,
                Some((
                    "Opens SoundRouter in the menu bar when you log in, so your routes are ready.",
                    false,
                )),
                v.login,
            ) != v.login
            {
                out.push(Change::Login(!v.login));
            }
            if toggle(
                ui,
                t,
                "menu_bar",
                MENU_BAR,
                Some(("The loop icon with Open, Presets and Quit.", false)),
                s.show_in_menu_bar,
            ) != s.show_in_menu_bar
            {
                out.push(Change::MenuBar(!s.show_in_menu_bar));
            }
            let (text, warn) = keep_running_note(s);
            if toggle(
                ui,
                t,
                "keep_running",
                KEEP_RUNNING,
                Some((text, warn)),
                s.keep_running_on_close,
            ) != s.keep_running_on_close
            {
                out.push(Change::KeepRunning(!s.keep_running_on_close));
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if sheet::button(ui, t, PERMISSIONS, Kind::Secondary) {
                    out.push(Change::Permissions);
                }
            });

            // Audio
            heading(ui, t, "Audio");
            subhead(ui, t, "Audio delay");
            sheet::body(
                ui,
                t,
                "How long audio takes to pass through SoundRouter. Lower is snappier for live monitoring and calls, but can crackle on a busy Mac.",
            );
            ui.add_space(2.0);
            for n in BUFFER_FRAMES {
                let right = Some(format!("+{:.1} ms", delay_ms(n)));
                if radio(
                    ui,
                    t,
                    ("delay", n),
                    &delay_option(n),
                    delay_name(n),
                    right,
                    n == v.buffer_frames,
                ) {
                    out.push(Change::BufferFrames(n));
                }
            }
            if low_power_hint_due(v.status.low_power_mode, v.buffer_frames) {
                sheet::body(ui, t, LOW_POWER_HINT);
            }
            ui.add_space(8.0);
            let unsupported = look == ReadAheadLook::Unsupported;
            let shown_on = matches!(look, ReadAheadLook::On | ReadAheadLook::TurningOn);
            let mut changed = None;
            // Disabled, not just dimmed: it must not take focus or clicks, and says so to screen readers.
            ui.add_enabled_ui(!unsupported, |ui| {
                changed = Some(toggle_row(
                    ui,
                    t,
                    "read_ahead",
                    LOWER_DELAY,
                    Some(("Shortens the loop through a virtual device by up to one buffer. It switches itself off if the driver drops audio.", false)),
                    shown_on,
                ));
            });
            if !unsupported && changed.is_some_and(|c| c != shown_on) {
                out.push(Change::ReadAhead(!shown_on));
            }
            let (line, line_color) = match look {
                ReadAheadLook::Tripped | ReadAheadLook::Unsupported => {
                    (read_ahead_text(look), t.warn_ink)
                }
                _ => (read_ahead_text(look), t.muted),
            };
            ui.label(RichText::new(line).size(13.0).color(line_color));
            if look == ReadAheadLook::On {
                ui.label(
                    RichText::new(dropouts_text(v.overloads_since_on))
                        .size(13.0)
                        .color(t.muted),
                );
            }
            if look == ReadAheadLook::Unsupported
                && sheet::button(ui, t, UPDATE_DRIVER, Kind::Secondary)
            {
                out.push(Change::UpdateDriver);
            }
            if look == ReadAheadLook::Tripped && sheet::button(ui, t, TRY_AGAIN, Kind::Secondary) {
                out.push(Change::TryAgain);
            }

            // Capture
            if v.split_available {
                heading(ui, t, "Capture");
                if toggle(
                    ui,
                    t,
                    "split",
                    SPLIT,
                    Some((SPLIT_NOTE, false)),
                    s.engine.tap_split,
                ) != s.engine.tap_split
                {
                    out.push(Change::TapSplit(!s.engine.tap_split));
                }
                subhead(ui, t, MARGIN);
                for (blocks, text) in [(1u8, MARGIN_1), (2, MARGIN_2)] {
                    if radio(
                        ui,
                        t,
                        ("cushion", u32::from(blocks)),
                        text,
                        text,
                        None,
                        s.engine.tap_cushion_blocks == blocks,
                    ) {
                        out.push(Change::Cushion(blocks));
                    }
                }
            }

            // Advanced
            heading(ui, t, "Advanced");
            if state.confirm_restore {
                sheet::body(ui, t, RESTORE_ASK);
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if sheet::button(ui, t, "Restore", Kind::Danger) {
                        out.push(Change::Restore);
                        state.confirm_restore = false;
                    }
                    if sheet::button(ui, t, "Cancel", Kind::Secondary) {
                        state.confirm_restore = false;
                    }
                });
            } else {
                ui.horizontal_wrapped(|ui| {
                    if sheet::button(ui, t, COPY_DIAGNOSTICS, Kind::Secondary) {
                        state.copied = true;
                        out.push(Change::CopyDiagnostics);
                    }
                    if sheet::button(ui, t, SHOW_LOG, Kind::Secondary) {
                        out.push(Change::ShowLog);
                    }
                });
                if state.copied {
                    sheet::body(ui, t, "Copied to clipboard");
                }
                ui.add_space(2.0);
                if sheet::button(ui, t, RESTORE, Kind::Secondary) {
                    state.confirm_restore = true;
                }
            }
        },
        |ui, c| {
            sheet::buttons(
                ui,
                c,
                |_, _| {},
                |ui, c| {
                    if sheet::button(ui, t, DONE, Kind::Primary) {
                        c.state.open = false;
                    }
                },
            );
        },
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lower_delay_line_follows_the_driver_the_user_and_the_safety_net() {
        use ReadAheadLook::*;
        use ReadAheadState as S;
        // (driver says, user wants, safety net tripped) -> what the sheet shows
        let table = [
            (S::Unsupported, false, false, Unsupported),
            (S::Unsupported, true, false, Unsupported),
            (S::Unsupported, false, true, Unsupported),
            (S::Off, false, false, Off),
            (S::On, true, false, On),
            (S::Off, true, false, TurningOn),
            (S::On, false, false, TurningOff),
            (S::Off, false, true, Tripped),
            (S::On, false, true, Tripped),
        ];
        for (state, wanted, tripped, want) in table {
            assert_eq!(
                read_ahead_look(state, wanted, tripped),
                want,
                "{state:?} wanted={wanted} tripped={tripped}"
            );
        }
    }

    #[test]
    fn the_status_lines_use_the_words_of_the_design() {
        assert_eq!(
            read_ahead_text(ReadAheadLook::Unsupported),
            "Needs the newer audio driver"
        );
        assert_eq!(
            read_ahead_text(ReadAheadLook::Tripped),
            "Turned itself off after driver dropouts"
        );
        assert_eq!(read_ahead_text(ReadAheadLook::Off), "Off");
        assert_eq!(read_ahead_text(ReadAheadLook::On), "On");
        assert_eq!(dropouts_text(3), "Driver dropouts since on: 3");
    }

    #[test]
    fn keeping_routing_off_is_the_only_state_that_warns() {
        let mut s = Settings::default();
        assert!(!keep_running_note(&s).1);
        s.show_in_menu_bar = false;
        assert!(!keep_running_note(&s).1);
        s.keep_running_on_close = false;
        assert_eq!(keep_running_note(&s), (CLOSING_STOPS, true));
        s.show_in_menu_bar = true;
        assert_eq!(keep_running_note(&s), (CLOSING_STOPS, true));
    }
}
