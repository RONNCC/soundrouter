//! The Settings sheet through the whole window: how it opens, what each control sends, the read-ahead
//! states, the safety net, and the file it saves.
use super::tests::{click, has, settle, window};
use super::*;
use crate::fake::{FakeBackend, demo_config, demo_status};
use crate::model::delay_option;
use crate::settings_sheet::{
    CLOSING_STOPS, COPY_DIAGNOSTICS, DONE, KEEP_RUNNING, LOGIN, LOWER_DELAY, MARGIN_1, MARGIN_2,
    MENU_BAR, NEEDS_DRIVER, PERMISSIONS, RESTORE, RESTORE_ASK, SHOW_LOG, SPLIT, SPLIT_NOTE,
    TRIPPED, TRY_AGAIN, UPDATE_DRIVER,
};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use sr_core::backend::{
    AppInfo, Backend, EngineOptions, EngineStatus, HwDevice, MeterReading, ReadAheadState,
};
use std::sync::Mutex;

const ON: ReadAheadState = ReadAheadState::On;
const OFF: ReadAheadState = ReadAheadState::Off;

/// What the engine was asked, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Apply,
    Options(EngineOptions),
}

type Calls = Arc<Mutex<Vec<Call>>>;

/// A `FakeBackend` that remembers its calls.
struct Spy(FakeBackend, Calls);

impl Backend for Spy {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()> {
        self.1.lock().unwrap().push(Call::Apply);
        self.0.apply(cfg)
    }
    fn hardware(&self) -> Vec<HwDevice> {
        self.0.hardware()
    }
    fn apps(&self) -> Vec<AppInfo> {
        self.0.apps()
    }
    fn meters(&self, id: &str) -> Option<MeterReading> {
        self.0.meters(id)
    }
    fn status(&self) -> EngineStatus {
        self.0.status()
    }
    fn set_options(&mut self, o: &EngineOptions) -> anyhow::Result<()> {
        self.1.lock().unwrap().push(Call::Options(o.clone()));
        self.0.set_options(o)
    }
}

/// The machine: Start at login in a cell, opened links, and how often the menu-bar item was asked for.
struct Machine {
    login: Arc<Mutex<bool>>,
    opened: Arc<Mutex<Vec<String>>>,
    trays: Arc<Mutex<u32>>,
}

fn machine(h: &mut Harness<App>) -> Machine {
    let m = Machine {
        login: Arc::default(),
        opened: Arc::default(),
        trays: Arc::default(),
    };
    let (read, write, opened, trays) = (
        m.login.clone(),
        m.login.clone(),
        m.opened.clone(),
        m.trays.clone(),
    );
    h.state_mut().os = Os {
        login_is_on: Arc::new(move || *read.lock().unwrap()),
        set_login: Arc::new(move |on| {
            *write.lock().unwrap() = on;
            Ok(())
        }),
        open_url: Arc::new(move |u| opened.lock().unwrap().push(u.to_owned())),
        // The sheet jumps into place instead of sliding, so a click lands where the tree says.
        reduce_motion: Arc::new(|| true),
        new_tray: Arc::new(move |_| {
            *trays.lock().unwrap() += 1;
            None
        }),
    };
    m
}

/// An installed (non-demo) driver with `read_ahead` as it reports.
fn status(read_ahead: ReadAheadState) -> EngineStatus {
    EngineStatus {
        driver_version: Some("1".into()),
        read_ahead,
        ..demo_status()
    }
}

/// A sheet takes a few frames to find its size and place after it opens or its content changes;
/// a click aimed with the tree of an earlier frame would miss.
fn settle_sheet(h: &mut Harness<App>) {
    h.run_steps(60);
}

struct Rig {
    h: Harness<'static, App>,
    calls: Calls,
    m: Machine,
    dir: tempfile::TempDir,
}

impl Rig {
    fn file(&self) -> std::path::PathBuf {
        self.dir.path().join("settings.json")
    }
    fn saved(&self) -> Settings {
        let (s, msg) = settings::load(&self.file());
        assert!(msg.is_none(), "{msg:?}");
        s
    }
    fn settings(&self) -> &Settings {
        &self.h.state().settings
    }
    fn options(&self) -> Vec<EngineOptions> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                Call::Options(o) => Some(o.clone()),
                Call::Apply => None,
            })
            .collect()
    }
    fn last_options(&self) -> EngineOptions {
        self.options().pop().expect("options were sent")
    }
    fn open(&mut self) {
        self.h
            .key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
        settle_sheet(&mut self.h);
        assert!(self.h.state().settings_sheet.open);
    }
    fn click(&mut self, label: &str) {
        click(&mut self.h, label);
        settle_sheet(&mut self.h);
    }
}

fn rig_with(status: EngineStatus, demo: bool, setup: impl FnOnce(&mut App)) -> Rig {
    let calls = Calls::default();
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    let mut h = window(
        demo_config(),
        Box::new(Spy(b, calls.clone())),
        None,
        Theme::Dark,
        demo,
    );
    // Tall enough that the whole sheet is on screen: a click needs its control in view.
    h.set_size(vec2(1180.0, 1500.0));
    let m = machine(&mut h);
    let dir = tempfile::tempdir().unwrap();
    h.state_mut().settings_path = Some(dir.path().join("settings.json"));
    setup(h.state_mut());
    settle(&mut h);
    Rig { h, calls, m, dir }
}

fn rig(status: EngineStatus) -> Rig {
    rig_with(status, false, |_| {})
}

/// An open sheet.
fn opened(status: EngineStatus) -> Rig {
    let mut r = rig(status);
    r.open();
    r
}

// --- opening and closing -----------------------------------------------------------------------

#[test]
fn cmd_comma_opens_the_sheet_and_done_closes_it() {
    let mut r = rig(status(OFF));
    assert!(!has(&r.h, LOGIN));
    r.open();
    for name in [LOGIN, MENU_BAR, KEEP_RUNNING, LOWER_DELAY, SPLIT] {
        r.h.get_by_label(name);
    }
    r.click(DONE);
    assert!(!r.h.state().settings_sheet.open && !has(&r.h, LOGIN));
    // Again, now that it was closed once.
    r.open();
}

#[test]
fn the_footer_menu_lists_settings_above_permissions() {
    let mut r = rig(status(OFF));
    r.click("More actions");
    let at = |label| r.h.get_by_label(label).rect().top();
    assert!(at("Settings…") < at("Permissions"));
    r.click("Settings…");
    assert!(r.h.state().settings_sheet.open);
    r.h.get_by_label(LOGIN);
}

#[test]
fn the_menu_bar_item_opens_it_too() {
    let mut r = rig(status(OFF));
    r.h.state_mut().tray_cmds.push(tray::Cmd::Settings);
    settle(&mut r.h);
    assert!(r.h.state().settings_sheet.open);
    r.h.get_by_label(LOGIN);
}

#[test]
fn the_shortcut_is_ignored_while_a_setup_sheet_is_up() {
    let mut r = rig(status(OFF));
    r.h.state_mut().setup.open_access();
    settle(&mut r.h);
    r.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    settle(&mut r.h);
    assert!(!r.h.state().settings_sheet.open);
}

#[test]
fn esc_closes_the_sheet_and_applies_nothing() {
    let mut r = opened(status(OFF));
    let before = r.settings().clone();
    let sent = r.calls.lock().unwrap().len();
    r.h.key_press(egui::Key::Escape);
    settle(&mut r.h);
    assert!(!r.h.state().settings_sheet.open && !has(&r.h, LOGIN));
    assert_eq!(r.settings(), &before);
    assert_eq!(
        r.calls.lock().unwrap().len(),
        sent,
        "nothing reached the engine"
    );
    assert!(!r.file().exists(), "nothing was saved");
    assert!(!*r.m.login.lock().unwrap());
}

// --- General -------------------------------------------------------------------------------------

#[test]
fn start_at_login_goes_through_the_same_seam_as_the_permissions_row() {
    let mut r = opened(status(OFF));
    r.click(LOGIN);
    assert!(*r.m.login.lock().unwrap());
    r.click(LOGIN);
    assert!(!*r.m.login.lock().unwrap());
    assert!(!r.file().exists(), "the plist is the only record");
}

#[test]
fn show_in_menu_bar_creates_and_drops_the_item_and_is_saved() {
    let mut r = opened(status(OFF));
    r.click(MENU_BAR);
    assert!(!r.settings().show_in_menu_bar && r.h.state().tray.is_none());
    assert!(!r.saved().show_in_menu_bar);
    assert_eq!(*r.m.trays.lock().unwrap(), 0);
    r.click(MENU_BAR);
    assert!(r.settings().show_in_menu_bar && r.saved().show_in_menu_bar);
    assert_eq!(*r.m.trays.lock().unwrap(), 1, "the item was asked for");
}

#[test]
fn keep_routing_off_says_what_closing_does_and_makes_the_close_button_quit() {
    let mut r = opened(status(OFF));
    assert!(!has(&r.h, CLOSING_STOPS));
    assert_eq!(
        tray::step(r.h.state().win, tray::Event::CloseRequested).1,
        tray::Action::Hide { hint: true }
    );
    r.click(KEEP_RUNNING);
    r.h.get_by_label(CLOSING_STOPS);
    assert!(!r.settings().keep_running_on_close && !r.saved().keep_running_on_close);
    assert_eq!(
        tray::step(r.h.state().win, tray::Event::CloseRequested).1,
        tray::Action::Quit
    );
    r.click(KEEP_RUNNING);
    assert!(!has(&r.h, CLOSING_STOPS));
    assert!(r.settings().keep_running_on_close && !r.h.state().win.close_quits);
}

#[test]
fn permissions_button_closes_settings_and_opens_the_permissions_sheet() {
    let mut r = opened(status(OFF));
    r.click(PERMISSIONS);
    assert!(!r.h.state().settings_sheet.open);
    assert!(r.h.state().setup.wants_login());
    r.h.get_by_label("Start at login: Off");
}

// --- Audio ---------------------------------------------------------------------------------------

#[test]
fn the_audio_delay_is_the_footers_delay_and_the_same_edit() {
    let mut r = opened(status(OFF));
    assert_eq!(r.h.state().cfg.buffer_frames, 256);
    let before = r.calls.lock().unwrap().len();
    r.click(&delay_option(128));
    assert_eq!(r.h.state().cfg.buffer_frames, 128);
    r.h.get_by_label("Audio delay: Low");
    assert!(
        r.calls.lock().unwrap()[before..].contains(&Call::Apply),
        "the engine got the new config"
    );
    // Choosing the current one again changes nothing.
    let n = r.calls.lock().unwrap().len();
    r.click(&delay_option(128));
    assert_eq!(r.calls.lock().unwrap().len(), n);
}

#[test]
fn lower_delay_off_to_on_sends_the_option_and_saves_it() {
    let mut r = opened(status(OFF));
    r.h.get_by_label("Off");
    r.click(LOWER_DELAY);
    assert!(r.settings().engine.read_ahead && r.saved().engine.read_ahead);
    assert!(r.last_options().read_ahead);
    r.click(LOWER_DELAY);
    assert!(!r.last_options().read_ahead && !r.saved().engine.read_ahead);
}

#[test]
fn lower_delay_on_shows_its_state_and_the_dropout_count() {
    let mut r = rig_with(status(ON), false, |a| {
        a.settings.engine.read_ahead = true;
        a.guard = health::ReadAheadGuard::with_overloads(3);
    });
    r.open();
    r.h.get_by_label("On");
    r.h.get_by_label("Driver dropouts since on: 3");
    assert!(!has(&r.h, TRIPPED) && !has(&r.h, UPDATE_DRIVER) && !has(&r.h, TRY_AGAIN));
}

#[test]
fn lower_delay_with_an_old_driver_asks_for_the_new_one_and_cannot_be_switched() {
    let mut r = opened(status(ReadAheadState::Unsupported));
    r.h.get_by_label(NEEDS_DRIVER);
    assert!(
        r.h.get_by_label(LOWER_DELAY).accesskit_node().is_disabled(),
        "a disabled control, not a dimmed one"
    );
    r.click(LOWER_DELAY);
    assert!(!r.settings().engine.read_ahead);
    assert!(r.options().is_empty(), "nothing was sent");
    // The button opens the existing driver install step, with a way back.
    r.click(UPDATE_DRIVER);
    assert!(!r.h.state().settings_sheet.open && r.h.state().setup.shown());
    r.h.get_by_label("Install driver");
    r.click("Cancel");
    assert!(!r.h.state().setup.shown());
}

#[test]
fn a_tripped_lower_delay_says_so_and_try_again_clears_it() {
    let mut r = rig_with(status(OFF), false, |a| a.settings.trip());
    r.open();
    r.h.get_by_label(TRIPPED);
    assert!(!has(&r.h, NEEDS_DRIVER));
    r.click(TRY_AGAIN);
    assert!(r.settings().engine.read_ahead && !r.settings().read_ahead_tripped);
    assert!(r.last_options().read_ahead);
    let saved = r.saved();
    assert!(saved.engine.read_ahead && !saved.read_ahead_tripped);
    assert!(!has(&r.h, TRIPPED) && !has(&r.h, TRY_AGAIN));
}

#[test]
fn flipping_the_switch_of_a_tripped_lower_delay_is_trying_again() {
    let mut r = rig_with(status(OFF), false, |a| a.settings.trip());
    r.open();
    r.click(LOWER_DELAY);
    assert!(r.settings().engine.read_ahead && !r.settings().read_ahead_tripped);
}

// --- the safety net ------------------------------------------------------------------------------

fn set_overloads(r: &mut Rig, n: u64) {
    r.h.state()
        .worker
        .shared
        .lock()
        .unwrap()
        .status
        .driver_overloads = vec![("v1".into(), n)];
    r.h.step();
}

#[test]
fn growing_driver_overloads_turn_lower_delay_off_for_good_and_say_so() {
    let mut r = rig_with(status(ON), false, |a| a.settings.engine.read_ahead = true);
    settle(&mut r.h);
    set_overloads(&mut r, 0);
    set_overloads(&mut r, 0);
    assert!(r.settings().engine.read_ahead, "flat counters trip nothing");
    assert!(!r.file().exists());
    set_overloads(&mut r, 3);
    settle(&mut r.h);

    assert!(!r.settings().engine.read_ahead && r.settings().read_ahead_tripped);
    assert!(!r.last_options().read_ahead, "the engine was told");
    let saved = r.saved();
    assert!(
        !saved.engine.read_ahead && saved.read_ahead_tripped,
        "and the file"
    );
    r.h.get_by_label("Lower delay turned itself off after 3 driver dropouts.");

    // The banner's button leads to the sheet, which explains and offers to try again.
    r.click(health::SETTINGS);
    assert!(r.h.state().settings_sheet.open);
    r.h.get_by_label(TRIPPED);
    r.click(TRY_AGAIN);
    assert!(r.settings().engine.read_ahead);
    assert!(
        !has(
            &r.h,
            "Lower delay turned itself off after 3 driver dropouts."
        ),
        "trying again takes the notice away"
    );
}

#[test]
fn the_notice_ranks_below_a_silent_capture() {
    let mut r = rig_with(
        EngineStatus {
            tap_silent: vec!["v1".into()],
            ..status(ON)
        },
        false,
        |a| {
            a.settings.engine.read_ahead = true;
            a.trip_notice = Some(2);
        },
    );
    settle(&mut r.h);
    r.h.get_by_label("1 more notice");
    assert!(!has(
        &r.h,
        "Lower delay turned itself off after 2 driver dropouts."
    ));
    r.click("Dismiss notice");
    r.h.get_by_label("Lower delay turned itself off after 2 driver dropouts.");
}

#[test]
fn a_persisted_trip_keeps_lower_delay_off_at_the_next_launch_until_try_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("settings.json");
    // Even a hand-edited file that asks for both starts with it off.
    std::fs::write(
        &file,
        r#"{"read_ahead_tripped":true,"engine":{"read_ahead":true}}"#,
    )
    .unwrap();
    let (loaded, msg) = settings::load(&file);
    assert!(msg.is_none());
    let calls = launched(loaded.clone());
    assert_eq!(
        calls.first(),
        Some(&Call::Options(EngineOptions::default()))
    );
    assert!(loaded.read_ahead_tripped && !loaded.engine.read_ahead);
}

// --- start up ------------------------------------------------------------------------------------

/// Start the app the way `main` does, with `settings`, and return what the engine was asked.
fn launched(settings: Settings) -> Vec<Call> {
    let calls = Calls::default();
    let spy = Spy(FakeBackend::frozen(1.5), calls.clone());
    let mut h = Harness::builder()
        .with_size(vec2(1180.0, 700.0))
        .with_theme(Theme::Dark)
        .with_context_setup(crate::theme::install_fonts)
        .build_eframe(move |cc| {
            let w = Worker::spawn_inline(Box::new(spy), cc.egui_ctx.clone(), Box::new(|_| None));
            App::new(demo_config(), None, w, None, false, false).with_settings(settings, None)
        });
    super::tests::wait_published(&mut h);
    calls.lock().unwrap().clone()
}

#[test]
fn the_engine_gets_the_saved_options_before_its_first_config() {
    let saved = Settings {
        engine: EngineOptions {
            read_ahead: true,
            tap_split: false,
            tap_cushion_blocks: 2,
        },
        ..Settings::default()
    };
    let calls = launched(saved.clone());
    assert_eq!(calls.first(), Some(&Call::Options(saved.engine)));
    assert!(calls.contains(&Call::Apply));
}

// --- Capture -------------------------------------------------------------------------------------

#[test]
fn split_capture_toggle_sends_the_option_and_warns_audio_pauses() {
    let mut r = opened(status(OFF));
    r.h.get_by_label(SPLIT_NOTE);
    assert!(r.settings().engine.tap_split);
    r.click(SPLIT);
    assert!(!r.last_options().tap_split && !r.saved().engine.tap_split);
    r.click(SPLIT);
    assert!(r.last_options().tap_split);
}

#[test]
fn the_safety_margin_is_one_or_two_buffers_never_zero() {
    let mut r = opened(status(OFF));
    r.h.get_by_label(MARGIN_1);
    r.click(MARGIN_2);
    assert_eq!(r.last_options().tap_cushion_blocks, 2);
    assert_eq!(r.saved().engine.tap_cushion_blocks, 2);
    r.click(MARGIN_1);
    assert_eq!(r.last_options().tap_cushion_blocks, 1);
    assert!(
        r.options()
            .iter()
            .all(|o| (1..=2).contains(&o.tap_cushion_blocks))
    );
}

#[test]
fn the_capture_section_is_for_a_real_engine_only() {
    let mut r = rig_with(demo_status(), true, |_| {});
    r.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    settle(&mut r.h);
    r.h.get_by_label(LOWER_DELAY);
    assert!(!has(&r.h, SPLIT) && !has(&r.h, MARGIN_1));
    r.click(DONE);
    let mut r = rig(EngineStatus {
        driver_version: None,
        ..status(OFF)
    });
    // No driver: the setup sheet is up, and the shortcut leaves it alone.
    r.h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Comma);
    settle(&mut r.h);
    assert!(!r.h.state().settings_sheet.open);
}

// --- Advanced ------------------------------------------------------------------------------------

#[test]
fn copy_diagnostics_copies_the_support_text_and_the_settings() {
    let mut r = opened(status(OFF));
    r.h.get_by_label(COPY_DIAGNOSTICS).click();
    r.h.step();
    let copied =
        r.h.output()
            .platform_output
            .commands
            .iter()
            .find_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.clone()),
                _ => None,
            })
            .expect("something was copied");
    assert!(
        copied.contains("driver plug-in: found, protocol version 1"),
        "{copied}"
    );
    assert!(copied.contains("lower delay (read-ahead): off"), "{copied}");
    assert!(
        copied.contains("engine options: lower delay off, split app capture on"),
        "{copied}"
    );
    settle(&mut r.h);
    r.h.get_by_label("Copied to clipboard");
}

#[test]
fn show_log_opens_the_logs_folder() {
    let mut r = opened(status(OFF));
    r.click(SHOW_LOG);
    let opened = r.m.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].ends_with("Logs/SoundRouter"), "{opened:?}");
}

fn changed(a: &mut App) {
    a.settings = Settings {
        show_in_menu_bar: false,
        keep_running_on_close: false,
        engine: EngineOptions {
            read_ahead: true,
            tap_split: false,
            tap_cushion_blocks: 2,
        },
        ..Settings::default()
    };
    a.win.close_quits = true;
}

#[test]
fn restore_asks_first_and_cancel_changes_nothing() {
    let mut r = rig_with(status(OFF), false, changed);
    r.open();
    r.click(RESTORE);
    r.h.get_by_label(RESTORE_ASK);
    r.click("Cancel");
    assert!(!has(&r.h, RESTORE_ASK) && has(&r.h, RESTORE));
    assert!(!r.settings().keep_running_on_close && r.options().is_empty());
}

#[test]
fn esc_in_the_restore_question_cancels_it_and_keeps_the_sheet() {
    let mut r = rig_with(status(OFF), false, changed);
    r.open();
    r.click(RESTORE);
    r.h.key_press(egui::Key::Escape);
    settle(&mut r.h);
    assert!(!has(&r.h, RESTORE_ASK) && r.h.state().settings_sheet.open);
    assert!(!r.settings().keep_running_on_close);
    r.h.key_press(egui::Key::Escape);
    settle(&mut r.h);
    assert!(!r.h.state().settings_sheet.open);
}

#[test]
fn restore_resets_settings_only_and_tells_the_engine() {
    let mut r = rig_with(status(OFF), false, changed);
    *r.m.login.lock().unwrap() = true;
    r.open();
    let (cfg, presets) = (r.h.state().cfg.clone(), r.h.state().presets.names());
    r.click(RESTORE);
    r.click("Restore");
    assert_eq!(r.settings(), &Settings::default());
    assert_eq!(r.saved(), Settings::default());
    assert_eq!(r.last_options(), EngineOptions::default());
    assert!(!r.h.state().win.close_quits);
    assert_eq!(*r.m.trays.lock().unwrap(), 1, "the menu-bar item came back");
    assert_eq!(r.h.state().cfg, cfg, "routing is not settings");
    assert_eq!(r.h.state().presets.names(), presets);
    assert!(
        *r.m.login.lock().unwrap(),
        "Start at login lives in the LaunchAgent"
    );
    assert!(!has(&r.h, RESTORE_ASK), "the question went away");
}

// --- pointer targets -----------------------------------------------------------------------------

#[test]
fn every_switch_and_choice_has_a_full_size_target_that_takes_a_click_anywhere() {
    use crate::widgets::MIN_HIT;
    let mut r = opened(status(OFF));
    for name in [LOGIN, MENU_BAR, KEEP_RUNNING, LOWER_DELAY, SPLIT] {
        let rect = r.h.get_by_label(name).rect();
        assert!(
            rect.height() >= MIN_HIT && rect.width() >= 200.0,
            "{name}: {rect:?}"
        );
    }
    for name in [delay_option(64), MARGIN_1.to_owned(), MARGIN_2.to_owned()] {
        let rect = r.h.get_by_label(&name).rect();
        assert!(rect.height() >= MIN_HIT, "{name}: {rect:?}");
    }
    // The far end of the row, on its note rather than its switch, still toggles.
    let row = r.h.get_by_label(KEEP_RUNNING).rect();
    r.h.hover_at(row.left_bottom() + vec2(4.0, -4.0));
    r.h.step();
    r.h.drag_at(row.left_bottom() + vec2(4.0, -4.0));
    r.h.step();
    r.h.drop_at(row.left_bottom() + vec2(4.0, -4.0));
    settle(&mut r.h);
    assert!(!r.settings().keep_running_on_close);
}

/// `cargo test -p soundrouter-app --features snapshots` (`UPDATE_SNAPSHOTS=1` rewrites the baselines).
#[cfg(feature = "snapshots")]
mod snapshots {
    use super::*;

    /// A window that shows the whole sheet.
    const TALL: egui::Vec2 = egui::vec2(1180.0, 1320.0);
    /// The default window: the sheet is taller than it and scrolls.
    const MAIN: egui::Vec2 = egui::vec2(1180.0, 700.0);
    /// The smallest window the app allows, a short one.
    const SHORT: egui::Vec2 = egui::vec2(1000.0, 520.0);

    fn shot(
        name: &str,
        size: egui::Vec2,
        status: EngineStatus,
        sheet: bool,
        before: impl Fn(&mut App) + Clone + 'static,
    ) {
        shot_after(name, size, status, sheet, before, |_| {});
    }

    /// Scroll the sheet to its end.
    fn to_the_end(h: &mut Harness<'static, App>) {
        let at = h.ctx.content_rect().center();
        h.hover_at(at);
        h.step();
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: vec2(0.0, -4000.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(10);
    }

    fn shot_after(
        name: &str,
        size: egui::Vec2,
        status: EngineStatus,
        sheet: bool,
        before: impl Fn(&mut App) + Clone + 'static,
        after: fn(&mut Harness<'static, App>),
    ) {
        let mut results = egui_kittest::SnapshotResults::new();
        for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let (status, before) = (status.clone(), before.clone());
            let mut h = Harness::builder()
                .with_size(size)
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(crate::theme::install_fonts)
                .wgpu()
                .build_eframe(move |cc| {
                    let mut b = FakeBackend::frozen(1.5);
                    b.status = status;
                    let w =
                        Worker::spawn_inline(Box::new(b), cc.egui_ctx.clone(), Box::new(|_| None));
                    let mut app = App::new(demo_config(), None, w, None, false, false);
                    // Not `Setup::system()`: whether its sheet opens depends on this machine's marker file.
                    let no_admin: crate::setup::Runner =
                        Arc::new(|_| Err(crate::setup::SetupError::Failed("no admin".into())));
                    app.setup =
                        Setup::new(no_admin, Some("/App/Resources/SoundRouter.driver".into()))
                            .with_driver_check(|_| Ok("ab12".into()));
                    before(&mut app);
                    app
                });
            super::super::tests::wait_published(&mut h);
            settle(&mut h);
            machine(&mut h);
            h.state_mut().os.login_is_on = Arc::new(|| true);
            if sheet {
                h.state_mut().settings_sheet.open();
            }
            h.run_steps(60); // the sheet needs a few dozen frames to find its place
            after(&mut h);
            h.snapshot(format!("settings_{name}_{suffix}"));
            results.extend_harness(&mut h);
        }
    }

    #[test]
    fn sheet_default() {
        shot("default", TALL, status(OFF), true, |_| {});
    }

    #[test]
    fn sheet_in_the_default_window_scrolls() {
        shot("window", MAIN, status(OFF), true, |_| {});
    }

    #[test]
    fn sheet_lower_delay_needs_the_new_driver() {
        shot(
            "unsupported",
            TALL,
            status(ReadAheadState::Unsupported),
            true,
            |_| {},
        );
    }

    #[test]
    fn sheet_lower_delay_on_with_dropouts() {
        shot("on", TALL, status(ON), true, |a| {
            a.settings.engine.read_ahead = true;
            a.guard = health::ReadAheadGuard::with_overloads(3);
        });
    }

    #[test]
    fn sheet_lower_delay_tripped() {
        shot("tripped", TALL, status(OFF), true, |a| a.settings.trip());
    }

    #[test]
    fn sheet_split_off_and_keep_running_off() {
        shot("split_off", TALL, status(OFF), true, |a| {
            a.settings.engine.tap_split = false;
            a.settings.engine.tap_cushion_blocks = 2;
            a.settings.keep_running_on_close = false;
        });
    }

    #[test]
    fn sheet_in_a_short_window_scrolls() {
        shot("short", SHORT, status(OFF), true, |_| {});
    }

    #[test]
    fn sheet_in_a_short_window_scrolled_to_its_end() {
        shot_after("short_end", SHORT, status(OFF), true, |_| {}, to_the_end);
    }

    #[test]
    fn sheet_restore_question() {
        shot("restore", TALL, status(OFF), false, |a| {
            a.settings_sheet.ask_restore();
        });
    }

    #[test]
    fn banner_after_lower_delay_turned_itself_off() {
        shot("banner", MAIN, status(OFF), false, |a| {
            a.settings.trip();
            a.trip_notice = Some(3);
        });
    }
}
