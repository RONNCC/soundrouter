//! Whole-window tests: the real `App` and worker driven through the accessibility tree, so every
//! control is found by the name a screen reader would announce, then clicked/dragged/typed at.
use super::*;
use crate::fake::{FakeBackend, demo_config, demo_status};
use crate::model::bare_config;
use crate::setup::{Runner, SetupError};
use crate::worker::SelfTestState;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use sr_core::backend::{
    AppInfo, Backend, DeviceState, EngineStatus, HwDevice, MeterReading, SelfTest, TccStatus,
};
use sr_core::config::SourceKind;
use std::sync::{Arc, Mutex};

/// Fake backend whose `apply` always fails, like a missing driver.
struct Failing(FakeBackend, &'static str);

impl Backend for Failing {
    fn apply(&mut self, _: &Config) -> anyhow::Result<()> {
        anyhow::bail!(self.1)
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
}

/// Frames `until` may run: the 10 s driver deadline is 200 frames of 0.05 s.
const MAX_FRAMES: u32 = 1_000;

/// Step the window until `done` holds. The worker and the setup runner answer inside the call that
/// asks them (see `Worker::spawn_inline`), so what is awaited here arrives in frames, never in
/// wall time: no thread to race, no timer, the same result on a loaded machine.
fn until(h: &mut Harness<App>, what: &str, done: impl Fn(&Harness<App>) -> bool) {
    for _ in 0..MAX_FRAMES {
        if done(h) {
            return;
        }
        h.step();
    }
    panic!("not within {MAX_FRAMES} frames: {what}");
}

/// Block until the worker thread has published what the window needs: its first refresh (status,
/// hardware, apps) and, unless the setup sheet holds the first apply back, either meters for every
/// enabled device or the apply error.
pub(super) fn wait_published(h: &mut Harness<App>) {
    until(h, "worker publishing", |h| {
        let app = h.state();
        let s = app.worker.shared.lock().unwrap();
        let held = s.status.driver_version.is_none() || s.status.mic == TccStatus::Unknown;
        s.status_ready
            && !s.hardware.is_empty()
            && (held
                || s.apply_error.is_some()
                || app
                    .cfg
                    .devices
                    .iter()
                    .filter(|d| d.enabled)
                    .all(|d| s.meters.contains_key(&d.id)))
    });
}

pub(super) fn window(
    cfg: Config,
    backend: Box<dyn Backend + Send>,
    toast: Option<&str>,
    theme: Theme,
    demo: bool,
) -> Harness<'static, App> {
    window_with(cfg, backend, toast, theme, demo, never_run())
}

/// A runner for tests that must not reach the administrator prompt.
fn never_run() -> Runner {
    Arc::new(|_| Err(SetupError::Failed("no admin in tests".into())))
}

fn window_with(
    cfg: Config,
    backend: Box<dyn Backend + Send>,
    toast: Option<&str>,
    theme: Theme,
    demo: bool,
    runner: Runner,
) -> Harness<'static, App> {
    let worker =
        move |ctx: &egui::Context| Worker::spawn_inline(backend, ctx.clone(), Box::new(|_| None));
    window_on(cfg, worker, toast, theme, demo, runner)
}

/// `window_with` for a worker the test builds itself.
fn window_on(
    cfg: Config,
    worker: impl FnOnce(&egui::Context) -> Worker + 'static,
    toast: Option<&str>,
    theme: Theme,
    demo: bool,
    runner: Runner,
) -> Harness<'static, App> {
    let toast = toast.map(str::to_string);
    let mut h = Harness::builder()
        .with_size(vec2(1180.0, 700.0))
        .with_step_dt(0.05) // double-click window (0.3 s) spans six frames
        .with_theme(theme)
        .with_context_setup(crate::theme::install_fonts)
        .build_eframe(move |cc| {
            let w = worker(&cc.egui_ctx);
            let mut app = App::new(cfg, None, w, toast, false, demo);
            app.setup = Setup::new(runner, Some("/App/Resources/SoundRouter.driver".into()))
                .with_driver_check(|_| Ok("ab12".into()));
            app
        });
    wait_published(&mut h);
    settle(&mut h);
    h
}

pub(super) fn open(cfg: Config) -> Harness<'static, App> {
    window(
        cfg,
        Box::new(FakeBackend::frozen(1.5)),
        None,
        Theme::Dark,
        false,
    )
}

/// Process queued input, let the UI react, and require every control to have an accessible name.
pub(super) fn settle(h: &mut Harness<App>) {
    h.step();
    h.run_steps(3);
    h.check_accessibility();
}

pub(super) fn click(h: &mut Harness<App>, label: &str) {
    h.get_by_label(label).click();
    settle(h);
}

/// The footer button wears the same label as a row's button when one permission is open.
fn click_footer(h: &mut Harness<App>, label: &str) {
    h.get_all_by_label(label).last().unwrap().click();
    settle(h);
}

/// Uninstall sits in the footer's "More actions" menu.
fn uninstall(h: &mut Harness<App>) {
    click(h, "More actions");
    click(h, "Uninstall…");
}

fn click_row(h: &mut Harness<App>, label: &str) {
    h.get_all_by_label(label).next().unwrap().click();
    settle(h);
}

fn drag(h: &mut Harness<App>, from: egui::Pos2, to: egui::Pos2) {
    h.hover_at(from);
    h.step();
    h.drag_at(from);
    h.step();
    h.hover_at(from + (to - from) * 0.5);
    h.step();
    h.hover_at(to);
    h.step();
    h.drop_at(to);
    settle(h);
}

fn center(h: &Harness<App>, label: &str) -> egui::Pos2 {
    h.get_by_label(label).rect().center()
}

fn double_click(h: &mut Harness<App>, label: &str) {
    let n = h.get_by_label(label);
    n.click();
    n.click();
    settle(h);
}

pub(super) fn select_all(h: &mut Harness<App>) {
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
    settle(h);
}

/// A device with one Safari source wired channel for channel, plus the demo hardware list.
fn one_source() -> Config {
    let mut cfg = bare_config();
    apply(
        &mut cfg,
        Edit::AddSource(
            0,
            SourceKind::App {
                bundle_id: "com.apple.Safari".into(),
            },
            2,
        ),
    );
    cfg
}

#[test]
fn every_control_of_the_demo_window_has_an_accessible_name() {
    let h = open(demo_config());
    // `window` already ran the check; spot-check the names the other tests rely on.
    for label in [
        NEW_DEVICE,
        "Remove Loopback Audio",
        "Select Loopback Audio",
        "Rename Loopback Audio",
        "Enable Loopback Audio",
        "Mute Loopback Audio",
        "Loopback Audio volume",
        "Device name",
        "Add source",
        "Output channel options",
        "Music settings",
        "Music volume",
        "Mute Music",
        "EQ on Music",
        "Music channel 1 input",
        "Music channel 2 output",
        "Add monitor",
        "Safari enabled",
        "Safari options",
        "Safari channel 1 output",
    ] {
        h.get_by_label(label);
    }
    // The row's x is the one way to remove a device: the footer has no such button.
    assert!(h.query_by_label("Remove device").is_none());
}

#[test]
fn new_device_is_added_and_selected() {
    let mut h = open(demo_config());
    click(&mut h, NEW_DEVICE);
    assert_eq!(h.state().cfg.devices.len(), 4);
    assert_eq!(h.state().selected, 3);
    assert_eq!(
        h.get_by_label("Device name").value().as_deref(),
        Some("Loopback Audio 4")
    );
    h.get_by_label("Select Loopback Audio 4");
}

#[test]
fn deleting_the_last_device_selects_the_new_last_one_and_empties_cleanly() {
    let mut h = open(demo_config());
    click(&mut h, "Select Zoom Out");
    assert_eq!(h.state().selected, 2);
    click(&mut h, "Remove Zoom Out");
    assert_eq!(h.state().cfg.devices.len(), 2);
    assert_eq!(h.state().selected, 1);
    assert_eq!(
        h.get_by_label("Device name").value().as_deref(),
        Some("Stream Mix")
    );
    click(&mut h, "Remove Stream Mix");
    click(&mut h, "Remove Loopback Audio");
    assert!(h.state().cfg.devices.is_empty());
    assert_eq!(h.state().selected, 0);
    assert!(h.query_by_label("Device name").is_none());
    // the footer has no remove button to press with nothing selected
    assert!(h.query_by_label("Remove device").is_none());
    // and the user can start over
    click(&mut h, NEW_DEVICE);
    assert_eq!(h.state().cfg.devices.len(), 1);
    assert_eq!(h.state().selected, 0);
}

#[test]
fn rename_then_escape_changes_nothing() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    double_click(&mut h, "Rename Loopback Audio");
    assert!(h.state().renaming.is_some());
    select_all(&mut h);
    h.get_by_label("Rename device").type_text("Podcast");
    settle(&mut h);
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert!(h.state().renaming.is_none());
    assert_eq!(h.state().cfg, before);
    h.get_by_label("Select Loopback Audio");
}

#[test]
fn rename_with_enter_commits_the_trimmed_name() {
    let mut h = open(demo_config());
    double_click(&mut h, "Rename Loopback Audio");
    select_all(&mut h);
    h.get_by_label("Rename device").type_text("  Podcast ");
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[0].name, "Podcast");
    h.get_by_label("Select Podcast");
    assert_eq!(
        h.get_by_label("Device name").value().as_deref(),
        Some("Podcast")
    );
}

#[test]
fn renaming_to_blank_or_whitespace_is_refused_with_a_message() {
    for typed in [None, Some("   ")] {
        let mut h = open(demo_config());
        let before = h.state().cfg.clone();
        double_click(&mut h, "Rename Loopback Audio");
        select_all(&mut h);
        match typed {
            None => h.key_press(egui::Key::Backspace),
            Some(t) => h.get_by_label("Rename device").type_text(t),
        }
        settle(&mut h);
        h.key_press(egui::Key::Enter);
        settle(&mut h);
        assert_eq!(h.state().cfg, before, "typed {typed:?}");
        assert!(h.state().renaming.is_none());
        h.get_by_label(BLANK_NAME);
        // a later valid edit clears the message
        click(&mut h, "Mute Loopback Audio");
        assert!(h.query_by_label(BLANK_NAME).is_none());
    }
}

#[test]
fn clearing_the_title_field_keeps_the_old_name() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    h.get_by_label("Device name").click();
    settle(&mut h);
    select_all(&mut h);
    h.key_press(egui::Key::Backspace);
    settle(&mut h);
    assert_eq!(h.state().cfg, before);
    h.get_by_label(BLANK_NAME);
    assert_eq!(
        h.get_by_label("Device name").value().as_deref(),
        Some("Loopback Audio")
    );
}

#[test]
fn typing_a_new_title_renames_the_device() {
    let mut h = open(demo_config());
    h.get_by_label("Device name").click();
    settle(&mut h);
    select_all(&mut h);
    h.get_by_label("Device name").type_text("Podcast");
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[0].name, "Podcast");
    h.get_by_label("Select Podcast");
}

#[test]
fn toggling_a_device_switch_enables_and_disables_it() {
    let mut h = open(demo_config());
    assert!(h.state().cfg.devices[0].enabled);
    click(&mut h, "Enable Loopback Audio");
    assert!(!h.state().cfg.devices[0].enabled);
    click(&mut h, "Enable Loopback Audio");
    assert!(h.state().cfg.devices[0].enabled);
    // the disabled demo device turns on without touching the others
    click(&mut h, "Enable Zoom Out");
    let on: Vec<_> = h.state().cfg.devices.iter().map(|d| d.enabled).collect();
    assert_eq!(on, [true, true, true]);
}

#[test]
fn mute_button_toggles_the_device() {
    let mut h = open(demo_config());
    click(&mut h, "Mute Stream Mix");
    let muted: Vec<_> = h.state().cfg.devices.iter().map(|d| d.muted).collect();
    assert_eq!(muted, [false, true, false]);
}

#[test]
fn sliders_move_with_the_keyboard() {
    let mut h = open(demo_config());
    let db = h.state().cfg.devices[0].volume_db;
    h.get_by_label("Loopback Audio volume").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    assert!(h.state().cfg.devices[0].volume_db < db);

    click(&mut h, "Safari options");
    let db = h.state().cfg.devices[0].sources[0].volume_db;
    h.get_by_label("Safari volume").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    assert!(h.state().cfg.devices[0].sources[0].volume_db < db);
}

#[test]
fn dragging_from_a_source_port_to_an_output_port_makes_a_wire() {
    let mut h = open(one_source());
    let id = h.state().cfg.devices[0].sources[0].id.clone();
    // free output channel 1, then wire it again by hand
    click(&mut h, "Output channel 1 input");
    assert_eq!(h.state().cfg.devices[0].wires.len(), 1);
    let from = center(&h, "Safari channel 1 output");
    let to = center(&h, "Output channel 1 input");
    drag(&mut h, from, to);
    let wires = &h.state().cfg.devices[0].wires;
    assert_eq!(wires.len(), 2);
    assert!(
        wires
            .iter()
            .any(|w| w.source == id && w.src_ch == 0 && w.out_ch == 0),
        "{wires:?}"
    );
}

#[test]
fn dragging_onto_empty_space_changes_nothing() {
    let mut h = open(one_source());
    let before = h.state().cfg.clone();
    let from = center(&h, "Safari channel 1 output");
    drag(&mut h, from, egui::pos2(700.0, 600.0));
    assert_eq!(h.state().cfg, before);
}

#[test]
fn source_on_switch_mutes_and_unmutes_it() {
    let mut h = open(demo_config());
    assert!(!h.state().cfg.devices[0].sources[0].muted);
    click(&mut h, "Safari enabled");
    assert!(h.state().cfg.devices[0].sources[0].muted);
    assert!(!h.state().cfg.devices[0].sources[1].muted, "only Safari");
    click(&mut h, "Safari enabled");
    assert!(!h.state().cfg.devices[0].sources[0].muted);
}

#[test]
fn options_disclosure_opens_closes_and_removes_the_source() {
    let mut h = open(demo_config());
    assert!(h.query_by_label("Safari volume").is_none());
    click(&mut h, "Safari options");
    h.get_by_label("Safari volume");
    assert!(
        h.query_by_label("Spotify volume").is_none(),
        "one card only"
    );
    click(&mut h, "Safari options");
    assert!(h.query_by_label("Safari volume").is_none());
    click(&mut h, "Safari options");
    click(&mut h, "Remove Source Safari");
    let names: Vec<_> = h.state().cfg.devices[0]
        .sources
        .iter()
        .map(|s| s.kind.clone())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(h.query_by_label("Safari enabled").is_none());
}

#[test]
fn adding_an_app_source_makes_a_stereo_source() {
    let mut h = open(demo_config());
    click(&mut h, "Add source");
    click(&mut h, "Zoom");
    let src = h.state().cfg.devices[0].sources.last().unwrap().clone();
    assert_eq!(
        src.kind,
        SourceKind::App {
            bundle_id: "us.zoom.xos".into()
        }
    );
    // a tap is always a stereo mixdown, whatever the app
    assert_eq!(src.channels, 2);
}

#[test]
fn choose_app_wires_an_installed_app_that_is_not_running() {
    let mut h = open(demo_config());
    h.state_mut()
        .graph
        .installed
        .set(vec![AppInfo {
            bundle_id: "com.example.sketch".into(),
            name: "Sketchbook".into(),
            running_output: false,
            regular: true,
        }])
        .unwrap();
    click(&mut h, "Add source");
    click(&mut h, "Choose App…");
    click(&mut h, "Sketchbook");
    let src = h.state().cfg.devices[0].sources.last().unwrap().clone();
    assert_eq!(
        src.kind,
        SourceKind::App {
            bundle_id: "com.example.sketch".into()
        }
    );
    assert!(!src.mute_original);
    // The card is named after the installed app and the config applies without an error toast.
    h.get_by_label("Sketchbook enabled");
    settle(&mut h);
    assert!(h.state().toast.is_none());
}

#[test]
fn mute_in_original_output_is_an_app_option() {
    let mut h = open(demo_config());
    click(&mut h, "Safari options");
    let name = "Safari mute in original output";
    assert!(!h.state().cfg.devices[0].sources[0].mute_original);
    click(&mut h, name);
    assert!(h.state().cfg.devices[0].sources[0].mute_original);
    click(&mut h, name);
    assert!(!h.state().cfg.devices[0].sources[0].mute_original);
}

#[test]
fn adding_an_input_device_keeps_its_own_channel_count() {
    let mut h = open(demo_config());
    click(&mut h, "Add source");
    click(&mut h, "USB Headset");
    let src = h.state().cfg.devices[0].sources.last().unwrap().clone();
    assert_eq!(
        src.kind,
        SourceKind::Device {
            uid: "USBHeadset".into()
        }
    );
    assert_eq!(src.channels, 2);
}

#[test]
fn growing_and_shrinking_channel_pairs() {
    let mut h = open(demo_config());
    assert!(h.query_by_label("Remove channel pair").is_none());
    click(&mut h, "Output channel options");
    click(&mut h, "Add channel pair");
    assert_eq!(h.state().cfg.devices[0].channels, 4);
    click(&mut h, "Output channel options");
    click(&mut h, "Remove channel pair");
    assert_eq!(h.state().cfg.devices[0].channels, 2);
}

#[test]
fn a_failed_apply_shows_in_the_status_bar() {
    let h = window(
        demo_config(),
        Box::new(Failing(FakeBackend::frozen(1.5), "boom: no such device")),
        None,
        Theme::Dark,
        false,
    );
    h.get_by_label("boom: no such device");
}

#[test]
fn a_toast_shows_and_the_next_good_edit_clears_it() {
    let mut h = window(
        demo_config(),
        Box::new(FakeBackend::frozen(1.5)),
        Some("Could not read config"),
        Theme::Dark,
        false,
    );
    h.get_by_label("Could not read config");
    click(&mut h, "Mute Loopback Audio");
    assert!(h.query_by_label("Could not read config").is_none());
}

#[test]
fn demo_mode_says_so_and_nothing_else_by_default() {
    let h = window(
        demo_config(),
        Box::new(FakeBackend::frozen(1.5)),
        None,
        Theme::Light,
        true,
    );
    h.get_by_label("Demo mode: the audio is synthetic and nothing is saved.");
    let h = open(demo_config());
    assert!(h.query_by_label_contains("Demo mode").is_none());
}

#[test]
fn demo_mode_marks_the_title_and_the_chip() {
    let h = window(
        demo_config(),
        Box::new(FakeBackend::frozen(1.5)),
        None,
        Theme::Light,
        true,
    );
    h.get_by_label("Demo mode"); // the DEMO pill beside the title
    h.get_by_label("Device state: Demo");
    assert!(h.query_by_label("Device state: Not connected").is_none());
    let h = open(demo_config());
    assert!(h.query_by_label("Demo mode").is_none());
    assert!(h.query_by_label("Device state: Demo").is_none());
    h.get_by_label("Device state: Not connected");
}

/// Fake backend with many apps and output devices, to overflow the Sources and Monitors popups.
struct Crowded(FakeBackend);

impl Backend for Crowded {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()> {
        self.0.apply(cfg)
    }
    fn hardware(&self) -> Vec<HwDevice> {
        let outputs = (0..40).map(|i| HwDevice {
            uid: format!("out{i}"),
            name: format!("Output {i:02}"),
            in_ch: 0,
            out_ch: 2,
        });
        self.0.hardware().into_iter().chain(outputs).collect()
    }
    fn apps(&self) -> Vec<AppInfo> {
        (0..60)
            .map(|i| AppInfo {
                bundle_id: format!("com.example.app{i}"),
                name: format!("Example {i:02}"),
                running_output: false,
                regular: true,
            })
            .collect()
    }
    fn meters(&self, id: &str) -> Option<MeterReading> {
        self.0.meters(id)
    }
    fn status(&self) -> EngineStatus {
        self.0.status()
    }
}

fn top(h: &Harness<App>, label: &str) -> f32 {
    h.get_by_label(label).rect().top()
}

#[test]
fn the_source_menu_lists_inputs_first_then_playing_apps_then_gui_apps() {
    let mut h = open(demo_config());
    click(&mut h, "Add source");
    let order = [
        "MacBook Pro Microphone",
        "USB Headset",
        "Safari  (playing)",
        "Spotify  (playing)",
        "Finder",
        "Zoom",
        "Pass-Thru",
    ];
    let ys: Vec<f32> = order.iter().map(|l| top(&h, l)).collect();
    assert!(ys.windows(2).all(|w| w[0] < w[1]), "{order:?} at {ys:?}");
    // The speakers have no input, so they are no source.
    assert!(h.query_by_label("MacBook Pro Speakers").is_none());
}

#[test]
fn the_source_menu_hides_background_processes_until_asked() {
    let mut h = open(demo_config());
    click(&mut h, "Add source");
    for daemon in ["CoreSpeech", "PowerChime"] {
        assert!(h.query_by_label(daemon).is_none(), "{daemon} listed");
    }
    click(&mut h, "Show background processes");
    // the menu stays open and lists them after the regular apps
    assert!(top(&h, "CoreSpeech") > top(&h, "Zoom"));
    assert!(top(&h, "PowerChime") > top(&h, "CoreSpeech"));
    click(&mut h, "CoreSpeech");
    assert_eq!(
        h.state().cfg.devices[0].sources.last().unwrap().kind,
        SourceKind::App {
            bundle_id: "com.apple.CoreSpeech".into()
        }
    );
}

#[test]
fn the_source_menu_search_filters_inputs_and_apps() {
    let mut h = open(demo_config());
    click(&mut h, "Add source");
    h.get_by_label("Search sources").type_text("USB");
    settle(&mut h);
    h.get_by_label("USB Headset");
    for gone in ["Safari  (playing)", "Zoom", "MacBook Pro Microphone"] {
        assert!(h.query_by_label(gone).is_none(), "{gone} still listed");
    }
    h.get_by_label("Search sources").type_text("zzz");
    settle(&mut h);
    assert!(h.query_by_label("USB Headset").is_none());
    // the search starts empty again on the next open
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    click(&mut h, "Add source");
    h.get_by_label("Safari  (playing)");
}

#[test]
fn choosing_the_microphone_adds_a_mono_input_wired_to_channel_one() {
    let mut h = open(demo_config());
    click(&mut h, "Add source");
    click(&mut h, "MacBook Pro Microphone");
    let dev = &h.state().cfg.devices[0];
    let src = dev.sources.last().unwrap();
    assert_eq!(
        src.kind,
        SourceKind::Device {
            uid: "BuiltInMicrophoneDevice".into()
        }
    );
    assert_eq!(src.channels, 1);
    assert!(
        dev.wires
            .iter()
            .any(|w| w.source == src.id && w.src_ch == 0 && w.out_ch == 0)
    );
    // listed again, it is taken: greyed out, so it cannot be added twice
    click(&mut h, "Add source");
    let before = h.state().cfg.devices[0].sources.len();
    h.get_by_label("MacBook Pro Microphone").click();
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[0].sources.len(), before);
}

#[test]
fn an_unplugged_input_stays_a_source_and_says_not_connected() {
    let mut cfg = bare_config();
    apply(
        &mut cfg,
        Edit::AddSource(
            0,
            SourceKind::Device {
                uid: "GoneMic".into(),
            },
            1,
        ),
    );
    let h = open(cfg);
    h.get_by_label("GoneMic: Not connected");
    let granted = open(demo_config());
    assert!(
        granted
            .query_by_label("MacBook Pro Microphone: Not connected")
            .is_none()
    );
}

#[test]
fn a_denied_microphone_is_flagged_on_the_input_card() {
    let status = EngineStatus {
        mic: TccStatus::Denied,
        ..installed()
    };
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    let h = window(stream_mix(), Box::new(b), None, Theme::Dark, false);
    h.get_by_label("MacBook Pro Microphone: Microphone denied");
}

/// The demo's "Stream Mix" device (a microphone source) selected.
fn stream_mix() -> Config {
    let mut cfg = demo_config();
    cfg.devices.swap(0, 1);
    cfg
}

#[test]
fn adding_a_microphone_before_macos_was_asked_holds_the_apply_for_the_sheet() {
    for (mic, held) in [(TccStatus::Unknown, true), (TccStatus::Granted, false)] {
        let status = EngineStatus { mic, ..installed() };
        let mut h = with_runner(status, never_run());
        // `applied` stands for "the sheet has let the first apply through"
        h.state_mut().applied = true;
        h.state_mut().commit(vec![Edit::AddSource(
            0,
            SourceKind::Device {
                uid: "BuiltInMicrophoneDevice".into(),
            },
            1,
        )]);
        assert_eq!(!h.state().applied, held, "{mic:?}");
    }
}

#[test]
fn the_source_menu_stays_inside_the_window_and_scrolls() {
    let mut h = window(
        demo_config(),
        Box::new(Crowded(FakeBackend::frozen(1.5))),
        None,
        Theme::Dark,
        false,
    );
    click(&mut h, "Add source");
    let win = h.ctx.content_rect();
    for label in ["Search sources", "Pass-Thru", "Show background processes"] {
        let r = h.get_by_label(label).rect();
        assert!(win.contains_rect(r), "{label} at {r:?} leaves {win:?}");
    }
    // A scrolling list, not 60 rows: the last app is scrolled out of view.
    assert!(
        h.query_by_label("Example 59")
            .is_none_or(|n| !win.contains_rect(n.rect()))
    );
    assert!(h.get_by_label("Search sources").rect().width() <= 300.0);
}

#[test]
fn the_monitor_menu_stays_inside_the_window_and_scrolls() {
    let mut h = window(
        demo_config(),
        Box::new(Crowded(FakeBackend::frozen(1.5))),
        None,
        Theme::Dark,
        false,
    );
    click(&mut h, "Add monitor");
    let win = h.ctx.content_rect();
    let first = h.get_by_label("Output 00").rect();
    assert!(win.contains_rect(first), "{first:?} leaves {win:?}");
    // The last device is out of view, and reachable by scrolling the menu, not the window.
    assert!(!win.contains_rect(h.get_by_label("Output 39").rect()));
    h.get_by_label("Output 39").scroll_to_me();
    settle(&mut h);
    let last = h.get_by_label("Output 39").rect();
    assert!(win.contains_rect(last), "{last:?} leaves {win:?}");
}

#[test]
fn a_hidden_background_launch_applies_the_config_without_drawing() {
    let ctx = egui::Context::default();
    let backend = Box::new(FakeBackend::frozen(1.5));
    let w = Worker::spawn_inline(backend, ctx.clone(), Box::new(|_| None));
    let mut app = App::new(demo_config(), None, w, None, false, false).background(
        None,
        tray::WinState::default(),
        true,
    );
    // While the window is hidden eframe calls only `logic`, never `ui`.
    let mut frame = eframe::Frame::_new_kittest();
    for _ in 0..3 {
        eframe::App::logic(&mut app, &ctx, &mut frame);
    }
    assert!(app.applied);
    let s = app.worker.shared.lock().unwrap();
    let enabled = app.cfg.devices.iter().filter(|d| d.enabled);
    assert!(enabled.clone().count() > 0);
    for d in enabled {
        assert!(s.meters.contains_key(&d.id), "{} not running", d.id);
    }
}

#[test]
fn names_stop_at_the_driver_limit_instead_of_failing() {
    let mut h = open(demo_config());
    h.get_by_label("Device name").click();
    settle(&mut h);
    select_all(&mut h);
    h.get_by_label("Device name").type_text(&"x".repeat(80));
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[0].name, "x".repeat(MAX_NAME_CHARS));
    assert!(h.state().toast.is_none(), "{:?}", h.state().toast);

    double_click(&mut h, "Rename Stream Mix");
    select_all(&mut h);
    h.get_by_label("Rename device").type_text(&"y".repeat(80));
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[1].name, "y".repeat(MAX_NAME_CHARS));
    assert!(h.state().toast.is_none(), "{:?}", h.state().toast);
}

#[test]
fn an_unplugged_monitor_is_named_from_the_config_and_flagged() {
    let mut cfg = demo_config();
    cfg.devices[0].monitors[0].device_uid = "GoneSpeakers".into();
    cfg.devices[0].monitors[0].name = "Desk Speakers".into();
    let h = open(cfg);
    h.get_by_label("Desk Speakers: Not connected");
    // a present device's name is recorded in the config, for when it is unplugged later
    assert_eq!(h.state().cfg.devices[0].sources[0].name, "Safari");
}

/// The chip on the middle of the cable from `src` channel `ch` into output channel `out`.
fn chip(src: &str, ch: u16, out: u16) -> String {
    let side = |c: u16| if c.is_multiple_of(2) { "L" } else { "R" };
    format!(
        "Cable trim, {src}, channel {} ({}) to Channel {} ({})",
        ch + 1,
        side(ch),
        out + 1,
        side(out)
    )
}

macro_rules! chip_node {
    ($h:expr, $src:expr, $ch:expr, $out:expr) => {
        $h.get_by_label_contains(&chip($src, $ch, $out))
    };
}

/// Safari and Spotify into the two channels: two cables meet in each, no Pass-Thru.
fn two_sources() -> Config {
    let mut cfg = demo_config();
    apply(&mut cfg, Edit::RemoveSource(0, 2));
    cfg
}

/// GPU snapshots: `cargo test -p soundrouter-app --features snapshots`
/// (`UPDATE_SNAPSHOTS=1` to rewrite the baselines in `app/tests/snapshots`).
#[cfg(feature = "snapshots")]
mod snapshots {
    use super::*;

    fn shot(
        name: &str,
        theme: Theme,
        cfg: Config,
        toast: Option<&str>,
        before: impl FnOnce(&mut Harness<'static, App>),
        results: &mut egui_kittest::SnapshotResults,
    ) {
        shot_with(name, theme, cfg, toast, false, false, before, results);
    }

    #[allow(clippy::too_many_arguments)]
    fn shot_with(
        name: &str,
        theme: Theme,
        cfg: Config,
        toast: Option<&str>,
        low_power_mode: bool,
        demo: bool,
        before: impl FnOnce(&mut Harness<'static, App>),
        results: &mut egui_kittest::SnapshotResults,
    ) {
        let suffix = if theme == Theme::Dark {
            "dark"
        } else {
            "light"
        };
        let mut h = Harness::builder()
            .with_size(vec2(1180.0, 700.0))
            .with_step_dt(0.05)
            .with_theme(theme)
            .with_context_setup(crate::theme::install_fonts)
            .wgpu()
            .build_eframe(move |cc| {
                let mut backend = FakeBackend::frozen(1.5);
                backend.status.low_power_mode = low_power_mode;
                let backend = Box::new(backend);
                let w = Worker::spawn_inline(backend, cc.egui_ctx.clone(), Box::new(|_| None));
                let mut app = App::new(cfg, None, w, toast.map(str::to_string), false, demo);
                app.setup.skip_intro(); // the real marker may not exist on this machine
                app
            });
        wait_published(&mut h);
        settle(&mut h);
        before(&mut h);
        // meters and peak markers have settled on the frozen readings
        h.run_steps(5);
        h.snapshot(format!("{name}_{suffix}"));
        results.extend_harness(&mut h);
    }

    fn both(name: &str, cfg: impl Fn() -> Config, toast: Option<&str>, f: fn(&mut Harness<App>)) {
        let mut results = egui_kittest::SnapshotResults::new();
        for theme in [Theme::Dark, Theme::Light] {
            shot(name, theme, cfg(), toast, f, &mut results);
        }
    }

    /// `name` at the default window and at its smallest (`name_1000`), dark and light.
    fn sized(
        name: &str,
        cfg: impl Fn() -> Config,
        f: impl Fn(&mut Harness<App>),
        results: &mut egui_kittest::SnapshotResults,
    ) {
        for theme in [Theme::Dark, Theme::Light] {
            shot(name, theme, cfg(), None, &f, results);
            let small = |h: &mut Harness<App>| {
                minimum(h);
                f(h);
            };
            shot(&format!("{name}_1000"), theme, cfg(), None, small, results);
        }
    }

    #[test]
    fn one_mixer() {
        let mut results = egui_kittest::SnapshotResults::new();
        sized("mixers_1", demo_config, |_| {}, &mut results);
    }

    /// The demo's Music mixer, and one behind Safari and one behind Pass-Thru.
    #[test]
    fn three_mixers() {
        let cfg = || {
            let mut cfg = demo_config();
            apply(
                &mut cfg,
                Edit::AddNode(0, crate::model::NewNode::ForSource(0)),
            );
            apply(
                &mut cfg,
                Edit::AddNode(0, crate::model::NewNode::ForSource(2)),
            );
            assert_eq!(cfg.devices[0].nodes.len(), 3);
            cfg
        };
        let mut results = egui_kittest::SnapshotResults::new();
        sized("mixers_3", cfg, |_| {}, &mut results);
    }

    /// The Music mixer with only `fx`, its chip clicked open.
    fn effect(name: &str, fx: sr_core::config::Fx, results: &mut egui_kittest::SnapshotResults) {
        let cfg = || {
            let mut cfg = demo_config();
            cfg.devices[0].nodes[0].fx = vec![fx.clone()];
            cfg
        };
        let open = |h: &mut Harness<App>| {
            h.get_by_label_contains(" on Music").click();
            settle(h);
        };
        sized(name, cfg, open, results);
    }

    #[test]
    fn effect_popovers() {
        use sr_core::config::Fx;
        let mut results = egui_kittest::SnapshotResults::new();
        effect("mixer_fx_mono", Fx::Mono, &mut results);
        let eq = Fx::Eq {
            low_hz: 120.0,
            low_db: 3.0,
            high_hz: 8000.0,
            high_db: -2.5,
        };
        effect("mixer_fx_eq", eq, &mut results);
        let limiter = Fx::Limiter {
            ceiling_db: -1.0,
            knee_db: 6.0,
        };
        effect("mixer_fx_limiter", limiter, &mut results);
        effect("mixer_fx_delay", Fx::Delay { ms: 20.0 }, &mut results);
    }

    #[test]
    fn main_view() {
        both("main", demo_config, None, |_| {});
    }

    #[test]
    fn demo_main_view() {
        let mut results = egui_kittest::SnapshotResults::new();
        for theme in [Theme::Dark, Theme::Light] {
            shot_with(
                "demo_main",
                theme,
                demo_config(),
                None,
                false,
                true,
                |_| {},
                &mut results,
            );
        }
    }

    #[test]
    fn empty_state() {
        both("empty", Config::default, None, |_| {});
    }

    /// Safari and Pass-Thru into the stereo pair and Spotify into the Music mixer, with trims of
    /// every kind on show: one of them on a cable into the mixer.
    fn trimmed() -> Config {
        use crate::model::{Link, Port};
        let mut cfg = demo_config();
        let l = |src, ch, out| Link {
            from: Port::Src { src, ch },
            to: Port::OutIn { ch: out },
        };
        let music = Link {
            from: Port::Src { src: 1, ch: 1 },
            to: Port::NodeIn { node: 0, ch: 1 },
        };
        apply(&mut cfg, Edit::WireGain(0, l(0, 0, 0), -6.0));
        apply(&mut cfg, Edit::WireGain(0, music, -3.5));
        apply(&mut cfg, Edit::WireInvert(0, music, true));
        apply(&mut cfg, Edit::WireInvert(0, l(2, 0, 0), true));
        apply(&mut cfg, Edit::WireGain(0, l(2, 1, 1), 2.5));
        // Every edit landed on a cable that exists.
        let trims = cfg.devices[0].wires.iter();
        assert_eq!(trims.filter(|w| w.gain_db != 0.0 || w.invert).count(), 4);
        cfg
    }

    /// The window at its smallest, where the gutters between the cards are narrowest.
    fn minimum(h: &mut Harness<App>) {
        h.set_size(vec2(1000.0, 520.0));
        settle(h);
    }

    fn aim_at_a_cable(h: &mut Harness<App>) {
        let at = chip_node!(h, "Safari", 1, 1).rect().center();
        h.hover_at(at);
        settle(h);
    }

    fn open_fan_in(h: &mut Harness<App>) {
        chip_node!(h, "Safari", 0, 0).click();
        settle(h);
    }

    /// Open the trim popover of the microphone's left cable on Stream Mix: a mono input wired to
    /// both sides of the pair.
    fn open_pair(h: &mut Harness<App>) {
        click(h, "Select Stream Mix");
        let name = h
            .query_all_by_label_contains("Cable trim, ")
            .filter_map(|n| n.accesskit_node().label())
            .find(|l| l.contains(", channel 1 to Channel 1 (L)"))
            .unwrap();
        h.get_by_label_contains(&name).click();
        settle(h);
    }

    /// Mic panned right and its left cable flipped, on top of the demo's Stream Mix.
    fn panned_pair() -> Config {
        let mut cfg = demo_config();
        let l = crate::model::Link {
            from: crate::model::Port::Src { src: 1, ch: 0 },
            to: crate::model::Port::OutIn { ch: 0 },
        };
        apply(&mut cfg, Edit::WirePan(1, l, 0.4));
        apply(&mut cfg, Edit::WireInvert(1, l, true));
        cfg
    }

    #[test]
    fn cable_trim_badges_and_the_hovered_cable() {
        both("cable_trim_badges", trimmed, None, aim_at_a_cable);
    }

    #[test]
    fn cable_trim_badges_and_the_hovered_cable_at_the_minimum_window() {
        both("cable_trim_badges_1000", trimmed, None, |h| {
            minimum(h);
            aim_at_a_cable(h);
        });
    }

    #[test]
    fn cable_trim_popover_with_the_fan_in_hint() {
        both("cable_trim_popover", two_sources, None, open_fan_in);
    }

    #[test]
    fn cable_trim_popover_with_the_fan_in_hint_at_the_minimum_window() {
        both("cable_trim_popover_1000", two_sources, None, |h| {
            minimum(h);
            open_fan_in(h);
        });
    }

    #[test]
    fn cable_trim_popover_with_pan_on_a_trimmed_inverted_cable() {
        both("cable_trim_pan", panned_pair, None, open_pair);
    }

    #[test]
    fn cable_trim_popover_with_pan_at_the_minimum_window() {
        both("cable_trim_pan_1000", panned_pair, None, |h| {
            minimum(h);
            open_pair(h);
        });
    }

    #[test]
    fn expanded_options() {
        both("options", demo_config, None, |h| {
            click(h, "Safari options");
            click(h, "MacBook Pro Speakers options");
        });
    }

    #[test]
    fn icons_and_source_menu() {
        let mut results = egui_kittest::SnapshotResults::new();
        // A flat teal square stands in for the machine-specific real Safari icon.
        let icon = egui::ColorImage::filled([8, 8], egui::Color32::from_rgb(0x30, 0xB0, 0xC7));
        let publish = |h: &mut Harness<'static, App>| {
            let shared = h.state().worker.shared.clone();
            let img = std::sync::Arc::new(icon.clone());
            shared
                .lock()
                .unwrap()
                .icons
                .insert("com.apple.Safari".into(), img);
            settle(h);
        };
        let sources = |h: &mut Harness<'static, App>| {
            publish(h);
            click(h, "Add source");
        };
        let monitors = |h: &mut Harness<'static, App>| {
            publish(h);
            click(h, "Add monitor");
        };
        let cfg = demo_config;
        shot("icons", Theme::Dark, cfg(), None, sources, &mut results);
        shot(
            "icons_monitors",
            Theme::Dark,
            cfg(),
            None,
            monitors,
            &mut results,
        );
    }

    #[test]
    fn choose_app_list_loading_and_with_icons() {
        const NAMES: [&str; 16] = [
            "Chrome Remote Desktop Host Uninstaller",
            "Clock",
            "ColorSync Utility",
            "Comet",
            "Console",
            "Contacts",
            "Cursor",
            "Dictionary",
            "Digital Color Meter",
            "Disk Utility",
            "Docker",
            "FaceTime",
            "Finder",
            "Firefox",
            "Font Book",
            "Freeform",
        ];
        let open_list = |h: &mut Harness<'static, App>| {
            let apps = NAMES
                .iter()
                .enumerate()
                .map(|(i, n)| AppInfo {
                    // the uninstaller is a bundle without an icon
                    bundle_id: if i == 0 {
                        "com.example.uninstaller.noicon".into()
                    } else {
                        format!("com.example.{n}")
                    },
                    name: n.to_string(),
                    running_output: false,
                    regular: true,
                })
                .collect();
            h.state_mut().graph.installed.set(apps).unwrap();
            h.state_mut().graph.picker.manual(Box::new(fake_icon));
            settle(h);
            click(h, "Add source");
            click(h, "Choose App…");
        };
        let mut results = egui_kittest::SnapshotResults::new();
        for theme in [Theme::Dark, Theme::Light] {
            shot(
                "choose_app_loading",
                theme,
                demo_config(),
                None,
                open_list,
                &mut results,
            );
            shot(
                "choose_app_icons",
                theme,
                demo_config(),
                None,
                |h| {
                    open_list(h);
                    answer_icons(h);
                },
                &mut results,
            );
        }
    }

    #[test]
    fn sidebar_only_window_with_row_x() {
        let mut results = egui_kittest::SnapshotResults::new();
        for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let mut h = Harness::builder()
                .with_size(vec2(243.0, 345.0))
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(crate::theme::install_fonts)
                .wgpu()
                .build_eframe(move |cc| {
                    let backend = Box::new(FakeBackend::frozen(1.5));
                    let w = Worker::spawn_inline(backend, cc.egui_ctx.clone(), Box::new(|_| None));
                    let mut app = App::new(demo_config(), None, w, None, false, false);
                    app.setup.skip_intro(); // the real marker may not exist on this machine
                    app
                });
            wait_published(&mut h);
            settle(&mut h);
            h.run_steps(5);
            h.snapshot(format!("sidebar_narrow_{suffix}"));
            results.extend_harness(&mut h);
        }
    }

    #[test]
    fn removed_card_offers_undo() {
        both("removed", demo_config, None, |h| click(h, "Remove Safari"));
    }

    #[test]
    fn source_menu_open() {
        both("source_menu", demo_config, None, |h| click(h, "Add source"));
    }

    #[test]
    fn delay_menu_open() {
        both("delay_menu", demo_config, None, |h| {
            let label = delay_label(h.state().cfg.buffer_frames);
            click(h, &label);
        });
    }

    #[test]
    fn delay_menu_low_power_mode() {
        let mut results = egui_kittest::SnapshotResults::new();
        for theme in [Theme::Dark, Theme::Light] {
            let mut cfg = demo_config();
            cfg.buffer_frames = 128;
            shot_with(
                "delay_menu_low_power",
                theme,
                cfg,
                None,
                true,
                false,
                |h| click(h, &delay_label(128)),
                &mut results,
            );
        }
    }

    /// Three presets of the window's current routing.
    fn seed_presets(h: &mut Harness<App>) {
        let cfg = h.state().cfg.clone();
        for name in ["Streaming", "Podcast", "Game night"] {
            h.state_mut().presets.seed(name, &cfg);
        }
    }

    #[test]
    fn presets_menu_open() {
        both("presets_menu", demo_config, None, |h| {
            seed_presets(h);
            click(h, presets::BUTTON);
        });
    }

    #[test]
    fn presets_save_sheet() {
        both("presets_save", demo_config, None, |h| {
            click(h, presets::BUTTON);
            click(h, presets::SAVE_AS);
            h.get_by_label("Preset name").type_text("Streaming");
            settle(h);
        });
    }

    #[test]
    fn presets_manage_sheet() {
        both("presets_manage", demo_config, None, |h| {
            seed_presets(h);
            click(h, presets::BUTTON);
            click(h, presets::MANAGE);
        });
    }

    #[test]
    fn presets_confirm_sheet() {
        both("presets_confirm", demo_config, None, |h| {
            seed_presets(h);
            h.state_mut().cfg.devices.remove(1);
            click(h, presets::BUTTON);
            click(h, "Streaming");
        });
    }

    #[test]
    fn presets_applied_offers_undo() {
        both("presets_applied", demo_config, None, |h| {
            seed_presets(h);
            click(h, presets::BUTTON);
            click(h, "Podcast");
        });
    }

    #[test]
    fn measure_delay_sheet() {
        both("measure_delay", demo_config, None, |h| {
            click(h, &delay_label(256));
            click(h, "Measure delay…");
        });
    }

    #[test]
    fn source_menu_with_background_processes() {
        let mut results = egui_kittest::SnapshotResults::new();
        let open = |h: &mut Harness<'static, App>| {
            click(h, "Add source");
            click(h, "Show background processes");
        };
        shot(
            "source_menu_background",
            Theme::Dark,
            demo_config(),
            None,
            open,
            &mut results,
        );
    }

    #[test]
    fn error_status() {
        both(
            "error",
            demo_config,
            Some(
                "Could not save your settings: Permission denied (os error 13). Changes last until you quit.",
            ),
            |_| {},
        );
    }

    /// An unplugged microphone source and monitor, named from the config.
    fn unplugged() -> Config {
        let mut cfg = demo_config();
        cfg.devices.swap(0, 1); // Stream Mix: a microphone source
        let d = &mut cfg.devices[0];
        for s in &mut d.sources {
            if let SourceKind::Device { uid } = &mut s.kind {
                *uid = "GoneMic".into();
                s.name = "Podcast Mic".into();
            }
        }
        apply(&mut cfg, Edit::AddMonitor(0, "GoneSpeakers".into(), 2));
        cfg.devices[0].monitors.last_mut().unwrap().name = "Desk Speakers".into();
        cfg
    }

    #[test]
    fn unplugged_devices() {
        both("unplugged", unplugged, None, |_| {});
    }

    /// A window showing the setup flow, driven by `before`; both themes.
    fn sheet(
        name: &str,
        size: egui::Vec2,
        status: EngineStatus,
        test: Option<SelfTest>,
        runner: Runner,
        before: fn(&mut Harness<'static, App>),
    ) {
        let mut results = egui_kittest::SnapshotResults::new();
        for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let (status, runner, test) = (status.clone(), runner.clone(), test.clone());
            let mut h = Harness::builder()
                .with_size(size)
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(crate::theme::install_fonts)
                .wgpu()
                .build_eframe(move |cc| {
                    let mut b = FakeBackend::frozen(1.5);
                    b.status = status;
                    b.self_test = test;
                    let w =
                        Worker::spawn_inline(Box::new(b), cc.egui_ctx.clone(), Box::new(|_| None));
                    let mut app = App::new(demo_config(), None, w, None, false, false);
                    app.setup =
                        Setup::new(runner, Some("/App/Resources/SoundRouter.driver".into()))
                            .with_driver_check(|_| Ok("ab12".into()));
                    app
                });
            wait_published(&mut h);
            settle(&mut h);
            before(&mut h);
            h.run_steps(5);
            h.snapshot(format!("sheet_{name}_{suffix}"));
            results.extend_harness(&mut h);
        }
    }

    const WIDE: egui::Vec2 = egui::vec2(900.0, 640.0);

    fn mic_unasked() -> EngineStatus {
        EngineStatus {
            mic: TccStatus::Unknown,
            tcc: TccStatus::Unknown,
            ..installed()
        }
    }

    #[test]
    fn sheet_allow_access_not_asked() {
        sheet(
            "allow_unasked",
            WIDE,
            mic_unasked(),
            flowing(),
            never_run(),
            |_| {},
        );
    }

    #[test]
    fn sheet_welcome() {
        sheet("welcome", WIDE, no_driver(), None, never_run(), |h| {
            h.state_mut().setup = Setup::new(never_run(), None).with_intro();
            settle(h);
        });
    }

    #[test]
    fn sheet_footer_more_menu() {
        sheet("more_menu", WIDE, installed(), None, never_run(), |h| {
            click(h, "More actions");
        });
    }

    #[test]
    fn sheet_allow_access_granted() {
        sheet(
            "allow_granted",
            WIDE,
            installed(),
            flowing(),
            never_run(),
            |h| {
                click(h, "Test setup");
            },
        );
    }

    #[test]
    fn sheet_allow_access_denied() {
        let denied = EngineStatus {
            mic: TccStatus::Denied,
            tcc: TccStatus::Denied,
            ..installed()
        };
        // denied with sources that need it: the sheet opens by itself
        sheet("allow_denied", WIDE, denied, flowing(), never_run(), |_| {});
    }

    #[test]
    fn sheet_allow_access_mic_only() {
        let status = EngineStatus {
            mic: TccStatus::Unknown,
            ..installed()
        };
        sheet(
            "allow_mic_only",
            WIDE,
            status,
            flowing(),
            never_run(),
            |_| {},
        );
    }

    #[test]
    fn sheet_allow_access_capture_only() {
        let status = EngineStatus {
            tcc: TccStatus::Unknown,
            ..installed()
        };
        sheet(
            "allow_capture_only",
            WIDE,
            status,
            flowing(),
            never_run(),
            |h| {
                click(h, "Test setup");
            },
        );
    }

    /// The Verify step with System Audio Recording still open: its own Allow button.
    #[test]
    fn sheet_verify_capture_not_asked() {
        let status = EngineStatus {
            tcc: TccStatus::Unknown,
            ..installed()
        };
        sheet(
            "verify_capture_unasked",
            WIDE,
            status,
            flowing(),
            never_run(),
            |h| {
                click(h, "Test setup");
                click_footer(h, "Allow System Audio Recording");
                until(h, "self-test", |h| {
                    h.query_by_label_contains("Audio flows").is_some()
                });
            },
        );
    }

    #[test]
    fn sheet_self_test_running() {
        sheet(
            "test_running",
            WIDE,
            installed(),
            flowing(),
            never_run(),
            |h| {
                click(h, "Test setup");
                click(h, "Run test");
                h.state().worker.shared.lock().unwrap().self_test = SelfTestState::Running;
                h.run_steps(3);
            },
        );
    }

    #[test]
    fn sheet_self_test_pass() {
        sheet(
            "test_pass",
            WIDE,
            installed(),
            flowing(),
            never_run(),
            |h| {
                click(h, "Test setup");
                click(h, "Run test");
                until(h, "self-test", |h| {
                    h.query_by_label_contains("Audio flows").is_some()
                });
            },
        );
    }

    #[test]
    fn sheet_self_test_fail() {
        sheet("test_fail", WIDE, installed(), None, never_run(), |h| {
            click(h, "Test setup");
            click(h, "Run test");
            until(h, "self-test", |h| has(h, "Copy diagnostics"));
        });
    }

    #[test]
    fn sheet_all_done() {
        sheet("done", WIDE, installed(), flowing(), never_run(), |h| {
            click(h, "Test setup");
            click(h, "Run test");
            until(h, "self-test", |h| {
                h.query_by_label_contains("Audio flows").is_some()
            });
            click(h, "Continue");
        });
    }

    #[test]
    fn sheet_driver_missing() {
        sheet(
            "driver_missing",
            WIDE,
            no_driver(),
            None,
            never_run(),
            |_| {},
        );
    }

    #[test]
    fn sheet_driver_install_failed() {
        let runner = recording(Err(SetupError::Cancelled)).0;
        sheet("driver_failed", WIDE, no_driver(), None, runner, |h| {
            click(h, "Install driver");
            until(h, "runner result", |h| has(h, "Driver not installed"));
        });
    }

    #[test]
    fn sheet_uninstall_confirm() {
        sheet("uninstall", WIDE, installed(), None, never_run(), |h| {
            uninstall(h);
        });
    }

    #[test]
    fn sheet_uninstall_failed() {
        let runner = recording(Err(SetupError::Failed("Operation not permitted".into()))).0;
        sheet("uninstall_failed", WIDE, installed(), None, runner, |h| {
            uninstall(h);
            click(h, "Uninstall");
            until(h, "runner result", |h| {
                h.query_by_label_contains("Not uninstalled").is_some()
            });
        });
    }

    /// A long failure in a small window: the body scrolls, the buttons stay inside the card.
    #[test]
    fn sheet_long_content_in_a_small_window() {
        let long = "The installer stopped: ".to_string()
            + &"ditto: /Library/Audio/Plug-Ins/HAL/SoundRouter.driver: Operation not permitted. "
                .repeat(30);
        let runner = recording(Err(SetupError::Failed(long))).0;
        sheet(
            "overflow",
            egui::vec2(520.0, 340.0),
            no_driver(),
            None,
            runner,
            |h| {
                click(h, "Install driver");
                until(h, "runner result", |h| has(h, "Driver not installed"));
            },
        );
    }

    /// Regenerates the README screenshots:
    /// `cargo test -p soundrouter-app --features snapshots -- --ignored docs_screenshots`
    #[test]
    #[ignore = "writes docs/screenshots"]
    fn docs_screenshots() {
        for (theme, name) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let mut h = Harness::builder()
                .with_size(vec2(1180.0, 700.0))
                .with_pixels_per_point(2.0)
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(crate::theme::install_fonts)
                .wgpu()
                .build_eframe(|cc| {
                    let backend = Box::new(FakeBackend::frozen(1.5));
                    let w = Worker::spawn_inline(backend, cc.egui_ctx.clone(), Box::new(|_| None));
                    App::new(demo_config(), None, w, None, false, true)
                });
            wait_published(&mut h);
            settle(&mut h);
            h.run_steps(5);
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/screenshots");
            h.render()
                .unwrap()
                .save(format!("{path}/soundrouter-{name}.png"))
                .unwrap();
        }
    }
}

fn with_status(status: EngineStatus) -> Harness<'static, App> {
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    window(demo_config(), Box::new(b), None, Theme::Dark, false)
}

/// Status of an installed (non-demo) driver with every permission granted.
fn installed() -> EngineStatus {
    EngineStatus {
        driver_version: Some("1".into()),
        ..demo_status()
    }
}

fn one(state: DeviceState) -> EngineStatus {
    EngineStatus {
        per_device: vec![("v1".into(), state)],
        ..demo_status()
    }
}

#[test]
fn device_states_show_as_named_dots_and_permission_banner() {
    for (state, text) in [
        (DeviceState::Running, "Loopback Audio status: Running"),
        (DeviceState::Starting, "Loopback Audio status: Starting"),
        (
            DeviceState::Failed("boom".into()),
            "Loopback Audio status: Failed: boom",
        ),
    ] {
        let h = with_status(one(state));
        h.get_by_label(text);
        assert!(h.query_by_label(PERMISSION_BANNER).is_none());
    }
    let h = with_status(one(DeviceState::WaitingForPermission));
    h.get_by_label("Loopback Audio status: Waiting for permission");
    h.get_by_label(PERMISSION_BANNER);
    h.get_by_label(OPEN_SETTINGS);
    let h = with_status(one(DeviceState::Idle));
    assert!(h.query_by_label_contains("status:").is_none());
}

#[test]
fn engine_warnings_and_background_error_show_in_the_status_bar() {
    let h = with_status(EngineStatus {
        warnings: vec!["wire A".into(), "wire B".into()],
        ..demo_status()
    });
    h.get_by_label("wire A; wire B");
    let h = with_status(EngineStatus {
        background_error: Some("device vanished".into()),
        warnings: vec!["wire A".into()],
        ..demo_status()
    });
    h.get_by_label("device vanished");
}

#[test]
fn app_source_shows_its_icon_once_the_worker_publishes_one() {
    let mut h = open(one_source());
    assert!(
        h.query_by_label("Safari icon").is_none(),
        "tile fallback has no icon node"
    );
    let red = egui::ColorImage::filled([2, 2], egui::Color32::RED);
    h.state()
        .worker
        .shared
        .lock()
        .unwrap()
        .icons
        .insert("com.apple.Safari".into(), std::sync::Arc::new(red));
    settle(&mut h);
    h.get_by_label("Safari icon");
    // The source menu lists apps (with icon rows) and stays fully named.
    click(&mut h, "Add source");
    h.get_by_label("Pass-Thru");
}

/// A runner that records every script and answers with `result`.
fn recording(result: Result<(), SetupError>) -> (Runner, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c = calls.clone();
    let runner: Runner = Arc::new(move |script| {
        c.lock().unwrap().push(script.to_string());
        result.clone()
    });
    (runner, calls)
}

fn with_runner(status: EngineStatus, runner: Runner) -> Harness<'static, App> {
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    window_with(demo_config(), Box::new(b), None, Theme::Dark, false, runner)
}

pub(super) fn has(h: &Harness<App>, label: &str) -> bool {
    h.query_by_label(label).is_some()
}

fn no_driver() -> EngineStatus {
    EngineStatus {
        driver_version: None,
        ..demo_status()
    }
}

#[test]
fn a_brand_new_install_opens_with_a_welcome_before_the_driver_step() {
    let mut h = with_runner(no_driver(), never_run());
    h.state_mut().setup = Setup::new(never_run(), None).with_intro();
    settle(&mut h);
    h.get_by_label("Welcome to SoundRouter");
    assert!(!has(&h, "Install driver") && !has(&h, "Try the demo"));
    click(&mut h, "Get started");
    assert!(!has(&h, "Get started") && !has(&h, "Welcome to SoundRouter"));
    h.get_by_label("Install driver");
}

#[test]
fn without_a_driver_the_sheet_installs_it_and_a_cancel_says_not_installed() {
    let (runner, calls) = recording(Err(SetupError::Cancelled));
    let mut h = with_runner(no_driver(), runner);
    h.get_by_label("Try the demo");
    assert!(calls.lock().unwrap().is_empty());
    click(&mut h, "Install driver");
    until(&mut h, "runner result", |h| has(h, "Driver not installed"));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("/Library/Audio/Plug-Ins/HAL"));
    assert!(calls[0].contains("'/App/Resources/SoundRouter.driver'"));
    h.get_by_label("Install driver");
    h.get_by_label("Copy diagnostics");
}

#[test]
fn a_driver_signed_by_someone_else_is_refused_before_the_admin_prompt() {
    let (runner, calls) = recording(Ok(()));
    let mut h = with_runner(no_driver(), runner.clone());
    h.state_mut().setup = Setup::new(runner, Some("/App/Resources/SoundRouter.driver".into()))
        .with_driver_check(|_| Err(crate::setup::DRIVER_MISMATCH.into()));
    settle(&mut h);
    click(&mut h, "Install driver");
    until(&mut h, "check result", |h| has(h, "Driver not installed"));
    h.get_by_label_contains("does not match this app");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn a_driver_that_never_shows_up_after_a_good_install_times_out() {
    let (runner, _) = recording(Ok(()));
    let mut h = with_runner(no_driver(), runner);
    click(&mut h, "Install driver");
    until(&mut h, "waiting for the driver", |h| {
        has(h, "Starting the driver…")
    });
    until(&mut h, "deadline", |h| has(h, "Driver not installed"));
    h.get_by_label_contains("has not loaded it yet");
}

#[test]
fn a_driver_present_hides_the_sheet_and_the_footer_offers_test_and_uninstall() {
    let h = open(demo_config()); // the demo driver: nothing to test or uninstall
    assert!(!has(&h, "Install driver") && !has(&h, "Test setup") && !has(&h, "More actions"));
    let h = with_runner(installed(), never_run());
    assert!(!has(&h, "Install driver"));
    h.get_by_label("Test setup");
    h.get_by_label("More actions");
}

fn setup_run(status: EngineStatus, test: Option<SelfTest>) -> Harness<'static, App> {
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    b.self_test = test;
    let mut h = window(demo_config(), Box::new(b), None, Theme::Dark, false);
    // Something missing opens the sheet by itself; otherwise the footer button does.
    if !h.state().setup.shown() {
        click(&mut h, "Test setup");
    }
    h
}

fn flowing() -> Option<SelfTest> {
    Some(SelfTest {
        rms_dbfs: -6.0,
        dropouts: 0,
        rate: 48_000.0,
        passed: true,
    })
}

#[test]
fn test_setup_walks_permissions_verify_done_on_a_passing_test() {
    let mut h = setup_run(installed(), flowing());
    h.get_by_label("Microphone: Allowed");
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| {
        h.query_by_label_contains("Audio flows").is_some()
    });
    h.get_by_label("Audio flows (-6 dBFS, 0 dropouts)");
    click(&mut h, "Continue");
    h.get_by_label_contains("choose “Loopback Audio”");
    click(&mut h, "Done");
    assert!(!has(&h, "Continue") && !has(&h, "Done"));
}

#[test]
fn a_failed_self_test_offers_diagnostics() {
    let mut h = setup_run(installed(), None);
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| has(h, "Copy diagnostics"));
    h.get_by_label_contains("no audio came back");
    click(&mut h, "Copy diagnostics");
    h.get_by_label("Copied to clipboard");
    h.get_by_label("Test again");
}

#[test]
fn denied_audio_capture_offers_open_settings() {
    let h = setup_run(
        EngineStatus {
            tcc: TccStatus::Denied,
            ..installed()
        },
        flowing(),
    );
    h.get_by_label(OPEN_SETTINGS);
    h.get_by_label("System Audio Recording: Denied");
    h.get_by_label_contains("System Audio Recording Only");
    assert!(!has(&h, "Allow System Audio Recording"));
    let h = setup_run(installed(), flowing());
    assert!(!has(&h, OPEN_SETTINGS) && !has(&h, "Allow System Audio Recording"));
}

/// An asker that counts its calls and answers `with`.
fn asker(with: TccStatus) -> (crate::setup::Asker, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0));
    let c = calls.clone();
    (
        Arc::new(move || {
            *c.lock().unwrap() += 1;
            with
        }),
        calls,
    )
}

fn with_asker(status: EngineStatus, a: crate::setup::Asker) -> Harness<'static, App> {
    let mut h = with_runner(status, never_run());
    h.state_mut().setup = Setup::new(never_run(), None).with_asker(a);
    settle(&mut h);
    h
}

#[test]
fn the_capture_row_asks_macos_and_shows_the_answer() {
    for (answer, word, settings) in [
        (TccStatus::Granted, "Allowed", false),
        (TccStatus::Denied, "Denied", true),
        // no prompt came (or no answer in time): still open, the button stays
        (TccStatus::Unknown, "Needs permission", false),
    ] {
        let unasked = EngineStatus {
            tcc: TccStatus::Unknown,
            ..installed()
        };
        let (a, calls) = asker(answer);
        let mut h = with_asker(unasked, a);
        click(&mut h, "Test setup");
        h.get_by_label("System Audio Recording: Needs permission");
        assert!(!has(&h, OPEN_SETTINGS), "only after Denied");
        click_row(&mut h, "Allow System Audio Recording");
        assert_eq!(*calls.lock().unwrap(), 1);
        h.get_by_label(&format!("System Audio Recording: {word}"));
        assert_eq!(has(&h, OPEN_SETTINGS), settings, "{answer:?}");
        assert_eq!(
            h.query_all_by_label("Allow System Audio Recording").count() > 0,
            answer == TccStatus::Unknown
        );
    }
}

#[test]
fn the_microphone_row_has_its_own_button_and_footer_follows_what_is_missing() {
    let mut h = with_asker(
        EngineStatus {
            mic: TccStatus::Unknown,
            ..installed()
        },
        asker(TccStatus::Granted).0,
    );
    click(&mut h, "Test setup");
    h.get_by_label("Microphone: Needs permission");
    assert_eq!(
        h.get_all_by_label("Allow microphone").count(),
        2,
        "row + footer"
    );
    click_row(&mut h, "Allow microphone");
    h.get_by_label("Microphone: Asking…");
    assert!(!has(&h, "Allow microphone"));
    // answered: the row loses its button
    h.state().worker.shared.lock().unwrap().status.mic = TccStatus::Granted;
    settle(&mut h);
    h.get_by_label("Microphone: Allowed");

    let denied = EngineStatus {
        mic: TccStatus::Denied,
        ..installed()
    };
    let h = setup_run(denied, flowing());
    h.get_by_label("Microphone: Denied");
    h.get_by_label(OPEN_SETTINGS);
    h.get_by_label("Run test");
    assert!(!has(&h, "Open Microphone Settings"));
}

#[test]
fn continue_asks_for_the_microphone_then_system_audio() {
    // Microphone already answered: Continue asks for System Audio Recording right away.
    let (a, calls) = asker(TccStatus::Granted);
    let mut h = with_asker(
        EngineStatus {
            tcc: TccStatus::Unknown,
            ..installed()
        },
        a,
    );
    click(&mut h, "Test setup");
    click_footer(&mut h, "Allow System Audio Recording");
    assert_eq!(*calls.lock().unwrap(), 1);
    h.get_by_label("System Audio Recording: Allowed");

    // Both open: the Microphone prompt (self-test) first, System Audio Recording once it is answered.
    let (a, calls) = asker(TccStatus::Granted);
    let mut h = with_asker(
        EngineStatus {
            mic: TccStatus::Unknown,
            tcc: TccStatus::Unknown,
            ..installed()
        },
        a,
    );
    h.get_by_label("Allow both");
    click(&mut h, "Allow both");
    h.run_steps(3);
    assert_eq!(
        *calls.lock().unwrap(),
        0,
        "not while the Microphone is open"
    );
    until(&mut h, "System Audio Recording asked", |h| {
        h.state().worker.shared.lock().unwrap().status.mic = TccStatus::Granted;
        *calls.lock().unwrap() == 1
    });
    settle(&mut h);
    h.get_by_label("System Audio Recording: Allowed");
}

#[test]
fn a_dismissed_sheet_stays_dismissed() {
    let status = EngineStatus {
        mic: TccStatus::Denied,
        ..installed()
    };
    let mut h = with_runner(status, never_run());
    assert!(
        h.state().setup.shown(),
        "missing: the sheet opens by itself"
    );
    click(&mut h, "Close");
    h.run_steps(10);
    assert!(!h.state().setup.shown() && h.state().setup.seen());
    click(&mut h, "Test setup");
    assert!(h.state().setup.shown(), "Test Setup… always opens it");
}

#[test]
fn the_microphone_question_is_explained_before_the_first_apply() {
    let status = EngineStatus {
        mic: TccStatus::Unknown,
        ..installed()
    };
    let mut h = with_runner(status, never_run());
    assert_eq!(h.get_all_by_label("Allow microphone").count(), 2);
    assert!(!has(&h, "Close"), "cannot skip the explanation");
    assert!(h.state().worker.shared.lock().unwrap().meters.is_empty());
    click_footer(&mut h, "Allow microphone");
    until(&mut h, "apply and self-test", |h| {
        let s = h.state().worker.shared.lock().unwrap();
        !s.meters.is_empty() && matches!(s.self_test, SelfTestState::Done(_))
    });
}

#[test]
fn try_demo_swaps_to_the_synthetic_engine() {
    let mut h = with_runner(no_driver(), never_run());
    click(&mut h, "Try the demo");
    until(&mut h, "demo", |h| {
        has(h, "Demo mode: the audio is synthetic and nothing is saved.")
    });
    assert!(!has(&h, "Install driver"));
    until(&mut h, "demo meters", |h| {
        !h.state().worker.shared.lock().unwrap().meters.is_empty()
    });
}

#[test]
fn uninstall_asks_first_and_deletes_settings_only_when_ticked() {
    let (runner, calls) = recording(Err(SetupError::Cancelled));
    let mut h = with_runner(installed(), runner);
    uninstall(&mut h);
    assert!(calls.lock().unwrap().is_empty());
    click(&mut h, "Cancel");
    assert!(!has(&h, "Uninstall"));
    uninstall(&mut h);
    click(&mut h, "Uninstall");
    until(&mut h, "runner", |h| {
        has(h, "Not uninstalled. You cancelled the password dialog.")
    });
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("rm -rf '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'"));
        assert!(!calls[0].contains("Application Support"));
    }
    click(&mut h, "Also delete my SoundRouter settings");
    click(&mut h, "Uninstall");
    until(&mut h, "second run", |_| calls.lock().unwrap().len() == 2);
    assert!(calls.lock().unwrap()[1].contains("Application Support/SoundRouter"));
}

#[test]
fn after_deleting_the_settings_edits_are_not_written_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("SoundRouter/config.json");
    for (tick, kept) in [(false, true), (true, false)] {
        let (runner, _) = recording(Ok(()));
        let mut h = with_runner(installed(), runner);
        h.state_mut().path = Some(path.clone());
        uninstall(&mut h);
        if tick {
            click(&mut h, "Also delete my SoundRouter settings");
        }
        click(&mut h, "Uninstall");
        until(&mut h, "uninstalled", |h| !has(h, "Uninstall"));
        if tick {
            std::fs::remove_dir_all(dir.path().join("SoundRouter")).ok(); // what the script did
        }
        click(&mut h, "Mute Loopback Audio");
        assert_eq!(path.exists(), kept, "ticked {tick}");
    }
}

#[test]
fn the_x_removes_a_source_with_its_wires_and_the_toast_undoes_it() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    let id = before.devices[0].sources[0].id.clone();
    assert!(before.devices[0].wires.iter().any(|w| w.source == id));
    click(&mut h, "Remove Safari");
    let dev = &h.state().cfg.devices[0];
    assert_eq!(dev.sources.len(), before.devices[0].sources.len() - 1);
    assert!(dev.wires.iter().all(|w| w.source != id));
    assert!(h.query_by_label("Safari enabled").is_none());
    h.get_by_label("Removed Safari");
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before, "same source, same wires, same order");
    h.get_by_label("Safari enabled");
    assert!(!has(&h, "Undo") && !has(&h, "Removed Safari"));
}

/// Stream Mix with an unplugged microphone source named "Podcast Mic".
fn gone_mic() -> Config {
    let mut cfg = demo_config();
    cfg.devices.swap(0, 1);
    for s in &mut cfg.devices[0].sources {
        if let SourceKind::Device { uid } = &mut s.kind {
            *uid = "GoneMic".into();
            s.name = "Podcast Mic".into();
        }
    }
    cfg
}

#[test]
fn the_x_removes_the_monitor_pass_thru_and_input_device_cards() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    click(&mut h, "Remove MacBook Pro Speakers");
    assert!(h.state().cfg.devices[0].monitors.is_empty());
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    click(&mut h, "Remove Pass-Thru");
    assert_eq!(h.state().cfg.devices[0].sources.len(), 2);
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    let mut h = open(gone_mic());
    let before = h.state().cfg.clone();
    click(&mut h, "Remove Podcast Mic");
    assert_eq!(
        h.state().cfg.devices[0].sources.len(),
        before.devices[0].sources.len() - 1
    );
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
}

#[test]
fn every_removable_card_has_the_x_but_output_cards_do_not() {
    let h = open(demo_config());
    for name in ["Safari", "Spotify", "Pass-Thru", "MacBook Pro Speakers"] {
        h.get_by_label(&format!("Remove {name}"));
    }
    assert!(h.query_by_label_contains("Remove Channels").is_none());
}

#[test]
fn the_undo_toast_expires_and_the_next_edit_replaces_it() {
    let mut h = open(demo_config());
    click(&mut h, "Remove Safari");
    until(&mut h, "undo to expire", |h| !has(h, "Undo"));
    assert!(h.state().now > UNDO_SECS);
    click(&mut h, "Remove Spotify");
    h.get_by_label("Removed Spotify");
    click(&mut h, "Mute Loopback Audio");
    assert!(!has(&h, "Undo"), "a later edit makes the old undo unsafe");
}

#[test]
fn deleting_a_device_can_be_undone() {
    let mut h = open(demo_config());
    click(&mut h, "Select Stream Mix");
    let before = h.state().cfg.clone();
    assert!(
        h.query_by_label("Remove device").is_none(),
        "no footer button"
    );
    click(&mut h, "Remove Stream Mix");
    assert_eq!(h.state().cfg.devices.len(), before.devices.len() - 1);
    h.get_by_label("Removed Stream Mix");
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    assert_eq!(h.state().selected, 1);
    h.get_by_label("Select Stream Mix");
}

#[test]
fn row_x_removes_that_device_keeps_the_selection_on_its_device_and_undoes() {
    let mut h = open(demo_config());
    click(&mut h, "Select Zoom Out");
    let before = h.state().cfg.clone();
    click(&mut h, "Remove Loopback Audio");
    let names: Vec<_> = h
        .state()
        .cfg
        .devices
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert_eq!(names, ["Stream Mix", "Zoom Out"]);
    assert_eq!(h.state().selected, 1, "still Zoom Out");
    h.get_by_label("Removed Loopback Audio");
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    assert_eq!(h.state().selected, 2);
    // removing a device below the selection leaves the selection alone
    click(&mut h, "Select Loopback Audio");
    click(&mut h, "Remove Zoom Out");
    assert_eq!(h.state().cfg.devices.len(), 2);
    assert_eq!(h.state().selected, 0);
}

#[test]
fn a_failed_engine_start_shows_silence_and_a_standing_banner_until_the_engine_starts() {
    let now = Arc::new(Mutex::new(std::time::Instant::now()));
    let tries = Arc::new(Mutex::new(0u32));
    let (clock, count) = (now.clone(), tries.clone());
    let mut h = window_on(
        demo_config(),
        move |ctx| {
            // Fails twice, as coreaudiod does for the first seconds of a login.
            let start: crate::worker::EngineStart = Box::new(move || {
                let mut n = count.lock().unwrap();
                *n += 1;
                anyhow::ensure!(*n > 2, "no CoreAudio");
                Ok(Box::new(FakeBackend::frozen(1.5)) as Box<dyn Backend + Send>)
            });
            let clock = Arc::new(move || *clock.lock().unwrap());
            Worker::spawn_inline_retrying("no CoreAudio".into(), start, ctx.clone(), clock)
        },
        None,
        Theme::Dark,
        false,
        never_run(),
    );
    let banner = engine_banner("no CoreAudio");
    assert_eq!(
        banner,
        "Audio engine not ready yet. Retrying (no CoreAudio). Nothing is routed until it starts."
    );
    h.get_by_label(&banner);
    assert!(!has(&h, "Quit and reopen SoundRouter to try again."));
    {
        let s = h.state().worker.shared.lock().unwrap();
        assert!(!s.meters.is_empty());
        for m in s.meters.values() {
            assert!(m.channels.iter().all(|c| *c == 0.0));
            assert!(m.sources.iter().all(|(_, l)| l.iter().all(|v| *v == 0.0)));
        }
    }
    click(&mut h, "Mute Loopback Audio");
    h.run_steps(300); // far longer than any toast
    h.get_by_label(&banner);
    assert!(!has(
        &h,
        "Demo mode: the audio is synthetic and nothing is saved."
    ));
    assert_eq!(
        *tries.lock().unwrap(),
        0,
        "the first retry is due after 1 s"
    );

    // Retries at 1 s, 3 s (2 s later) and 7 s (4 s later); the third finds the engine.
    let wake = |h: &mut Harness<App>, secs: u64| {
        *now.lock().unwrap() += std::time::Duration::from_secs(secs);
        h.state().worker.wake();
        settle(h);
    };
    for (secs, attempts) in [(1, 1), (1, 1), (1, 2), (4, 3)] {
        wake(&mut h, secs);
        assert_eq!(*tries.lock().unwrap(), attempts);
        if attempts < 3 {
            h.get_by_label(&banner);
        }
    }
    assert!(!has(&h, &banner));
    assert!(
        h.query_by_label_contains("Audio engine not ready")
            .is_none()
    );
    let s = h.state().worker.shared.lock().unwrap();
    assert!(s.engine_error.is_none() && s.apply_error.is_none());
    let plays = s
        .meters
        .values()
        .any(|m| m.channels.iter().any(|c| *c > 0.0));
    assert!(plays, "the new engine got the config and runs it");
}

#[test]
fn a_hung_self_test_can_be_cancelled_or_times_out() {
    let hang = |h: &mut Harness<App>| {
        click(h, "Run test");
        until(h, "self-test", |h| {
            has(h, "Audio flows (-6 dBFS, 0 dropouts)")
        });
        h.state().worker.shared.lock().unwrap().self_test = SelfTestState::Running;
        settle(h);
    };
    let mut h = setup_run(installed(), flowing());
    hang(&mut h);
    click(&mut h, "Cancel");
    assert!(!has(&h, "Cancel") && !has(&h, "Loopback test"));

    let mut h = setup_run(installed(), flowing());
    hang(&mut h);
    until(&mut h, "30 s timeout", |h| {
        has(h, "The test did not finish in 30 seconds.")
    });
    assert!(h.state().now >= 30.0);
    h.get_by_label("Copy diagnostics");
    click(&mut h, "Test again");
    until(&mut h, "retry result", |h| {
        has(h, "Audio flows (-6 dBFS, 0 dropouts)")
    });
    assert!(!has(&h, "The test did not finish"));
}

/// The UI used to ask for a frame every 33 ms on its own (about 70 fps next to the worker's
/// 30 Hz); now only the worker (changed meters), animations and timers ask.
#[test]
fn a_settled_window_asks_for_no_more_frames() {
    let mut h = open(demo_config());
    h.run_steps(60);
    assert!(
        !h.ctx.has_requested_repaint(),
        "{:?}",
        h.ctx.repaint_causes()
    );
}

#[test]
fn a_row_rename_that_loses_focus_to_another_rows_x_renames_its_own_device() {
    let mut h = open(demo_config());
    double_click(&mut h, "Rename Zoom Out");
    select_all(&mut h);
    h.get_by_label("Rename device").type_text("Podcast");
    settle(&mut h);
    // The X sits on a row above: the removal used to shift the rename onto a missing index.
    click(&mut h, "Remove Loopback Audio");
    let names: Vec<_> = h.state().cfg.devices.iter().map(|d| &d.name).collect();
    assert_eq!(names, ["Stream Mix", "Podcast"]);
}

#[test]
fn undo_clicked_mid_rename_restores_the_config_and_renames_nothing_else() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    click(&mut h, "Remove Loopback Audio");
    double_click(&mut h, "Rename Stream Mix");
    select_all(&mut h);
    h.get_by_label("Rename device").type_text("Podcast");
    settle(&mut h);
    // The rename loses focus to the Undo click; its index belongs to the config Undo replaces,
    // where it used to rename the restored Loopback Audio.
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    assert_eq!(h.state().selected, 0);
}

#[test]
fn try_demo_resets_the_selection_and_the_undo_of_the_replaced_config() {
    let mut h = with_runner(no_driver(), never_run());
    {
        let app = h.state_mut();
        apply(&mut app.cfg, Edit::NewDevice);
        apply(&mut app.cfg, Edit::NewDevice);
        app.selected = 4;
        app.undo = Some(Undo {
            verb: "Removed",
            name: "Old".into(),
            prev: app.cfg.clone(),
            selected: 4,
            until: 1e6,
        });
    }
    click(&mut h, "Try the demo");
    until(&mut h, "demo", |h| h.state().demo);
    let app = h.state();
    assert!(app.selected < app.cfg.devices.len(), "{}", app.selected);
    assert!(app.undo.is_none());
    h.get_by_label("Device name");
}

#[test]
fn quitting_mid_slider_drag_still_saves_the_change() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let mut h = open(demo_config());
    h.state_mut().path = Some(path.clone());
    let knob = center(&h, "Loopback Audio volume");
    h.hover_at(knob);
    h.step();
    h.drag_at(knob);
    h.step();
    h.hover_at(knob - vec2(60.0, 0.0));
    h.run_steps(2);
    assert!(h.state().cfg.devices[0].volume_db < 0.0);
    assert!(!path.exists(), "the save waits for the button to come up");
    eframe::App::on_exit(h.state_mut());
    // The file says the version it needs (2: the demo has a mixer); the rest is the same.
    let mut want = h.state().cfg.clone();
    want.version = want.written_version();
    assert_eq!(store::load(&path).0, want);
}

/// Remembers the last config the worker applied.
struct Recording(FakeBackend, Arc<Mutex<Option<Config>>>);

impl Backend for Recording {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()> {
        *self.1.lock().unwrap() = Some(cfg.clone());
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
}

mod model_based {
    use super::*;
    use crate::model::tests::prop::edit;
    use proptest::prelude::*;

    /// One frame: an edit, maybe with a row rename that lost focus to it (in either order), or
    /// a click on Undo (`undo`).
    type Frame = (Edit, Option<(usize, String)>, bool, bool);

    fn frame() -> impl Strategy<Value = Frame> {
        (
            edit(),
            proptest::option::of((0usize..4, "[ -~]{0,6}")),
            any::<bool>(),
            proptest::bool::weighted(0.15),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        /// The real `App::commit`/`undo_removal` and inline worker under random frames: the kept
        /// config validates, the engine always has exactly it, the selection follows its device,
        /// a removal can be undone to the exact config before it, and a rename lands on the
        /// device it was typed on.
        #[test]
        fn random_frames_keep_config_engine_selection_and_undo_consistent(
            frames in proptest::collection::vec(frame(), 0..50)
        ) {
            let engine = Arc::new(Mutex::new(None));
            let backend = Recording(FakeBackend::frozen(1.5), engine.clone());
            let w = Worker::spawn_inline(Box::new(backend), egui::Context::default(), Box::new(|_| None));
            let mut app = App::new(demo_config(), None, w, None, false, false);
            app.first_apply();
            prop_assert!(app.applied);
            for (e, rename, rename_first, undo) in frames {
                let before = app.cfg.clone();
                let sel = app.selected;
                let sel_id = before.devices.get(sel).map(|d| d.id.clone());
                if undo {
                    let want = app.undo.as_ref().map(|u| (u.prev.clone(), u.selected));
                    app.undo_removal();
                    match want {
                        Some((prev, s)) => {
                            prop_assert_eq!(&app.cfg, &prev);
                            prop_assert_eq!(app.selected, s);
                        }
                        None => prop_assert_eq!(&app.cfg, &before),
                    }
                } else {
                    let new = e == Edit::NewDevice;
                    let removes = matches!(e, Edit::RemoveDevice(d) if d < before.devices.len());
                    let other_rename = matches!(e, Edit::Rename(..));
                    let mut edits = vec![e];
                    if let Some((i, n)) = rename.clone() {
                        let r = Edit::Rename(i, n);
                        if rename_first { edits.insert(0, r) } else { edits.push(r) }
                    }
                    app.commit(edits);
                    let accepted = app.cfg != before;
                    if removes {
                        prop_assert!(app.undo.as_ref().is_some_and(|u| u.prev == before));
                    }
                    if new && accepted {
                        prop_assert_eq!(app.selected, app.cfg.devices.len() - 1);
                    } else if let Some(id) = &sel_id {
                        match app.cfg.devices.iter().position(|d| &d.id == id) {
                            Some(p) => prop_assert_eq!(app.selected, p, "selection left its device"),
                            None => prop_assert_eq!(app.selected, sel.min(app.cfg.devices.len().saturating_sub(1))),
                        }
                    }
                    if let Some((i, n)) = rename
                        && accepted
                        && !other_rename
                        && !n.trim().is_empty()
                        && let Some(id) = before.devices.get(i).map(|d| &d.id)
                        && let Some(d) = app.cfg.devices.iter().find(|d| &d.id == id)
                    {
                        prop_assert_eq!(&d.name, &n, "rename landed elsewhere");
                    }
                    if accepted {
                        prop_assert!(app.save_pending);
                    }
                }
                prop_assert!(app.cfg.validate().is_ok());
                prop_assert!(app.selected <= app.cfg.devices.len().saturating_sub(1));
                let applied = engine.lock().unwrap().clone();
                prop_assert_eq!(applied.as_ref(), Some(&app.cfg));
            }
        }
    }
}

/// Device names, in order.
fn names(h: &Harness<App>) -> Vec<String> {
    h.state()
        .cfg
        .devices
        .iter()
        .map(|d| d.name.clone())
        .collect()
}

/// Start a sidebar rename of `name` and type `to` without committing it.
fn start_rename(h: &mut Harness<App>, name: &str, to: &str) {
    double_click(h, &format!("Rename {name}"));
    select_all(h);
    h.get_by_label("Rename device").type_text(to);
    settle(h);
}

#[test]
fn return_in_the_permissions_sheet_answers_the_footer_not_a_row_button() {
    let (a, calls) = asker(TccStatus::Granted);
    let mut h = with_asker(
        EngineStatus {
            mic: TccStatus::Unknown,
            tcc: TccStatus::Unknown,
            ..installed()
        },
        a,
    );
    h.get_by_label("Allow both");
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    // "Allow both" asks for System Audio Recording once the Microphone is answered; the
    // Microphone row's own button would not.
    until(&mut h, "System Audio Recording asked", |h| {
        h.state().worker.shared.lock().unwrap().status.mic = TccStatus::Granted;
        *calls.lock().unwrap() == 1
    });
}

#[test]
fn a_driver_that_blinks_while_test_setup_is_open_returns_to_the_permissions() {
    let mut h = setup_run(installed(), flowing());
    h.get_by_label("Run test");
    let set = |h: &mut Harness<App>, v: Option<&str>| {
        h.state()
            .worker
            .shared
            .lock()
            .unwrap()
            .status
            .driver_version = v.map(Into::into);
        settle(h);
    };
    set(&mut h, None);
    h.get_by_label("Install driver");
    set(&mut h, Some("1"));
    assert!(!has(&h, "Install driver"), "a stale 'not installed' sheet");
    h.get_by_label("Run test");
}

#[test]
fn typing_never_reaches_the_window_behind_a_sheet() {
    // The title field has the keyboard when the driver goes away and the sheet comes up.
    let mut h = with_runner(installed(), never_run());
    h.get_by_label("Device name").click();
    settle(&mut h);
    h.event(egui::Event::Text("Q".into()));
    settle(&mut h);
    let typed = h.state().cfg.devices[0].name.clone();
    assert!(typed.contains('Q'), "the field has the keyboard: {typed}");
    h.state()
        .worker
        .shared
        .lock()
        .unwrap()
        .status
        .driver_version = None;
    settle(&mut h);
    h.get_by_label("Install driver");
    h.event(egui::Event::Text("xyz".into()));
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[0].name, typed);

    // Same for a sidebar rename, which holds on to the keyboard while it lasts.
    let mut h = with_runner(installed(), never_run());
    start_rename(&mut h, "Stream Mix", "Pod");
    h.get_by_label("Rename device");
    h.state()
        .worker
        .shared
        .lock()
        .unwrap()
        .status
        .driver_version = None;
    settle(&mut h);
    h.event(egui::Event::Text("xyz".into()));
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    assert!(
        names(&h).iter().all(|n| !n.contains("xyz")),
        "{:?}",
        names(&h)
    );
}

#[test]
fn a_new_device_in_a_long_list_is_scrolled_into_view() {
    let mut cfg = demo_config();
    while cfg.devices.len() < 15 {
        crate::model::new_device(&mut cfg);
    }
    let mut h = open(cfg);
    h.set_size(vec2(1000.0, 520.0)); // the window's minimum size
    settle(&mut h);
    click(&mut h, NEW_DEVICE);
    assert_eq!(h.state().selected, 15);
    let row = h.get_by_label("Select Loopback Audio 16").rect();
    let new = h.get_by_label(NEW_DEVICE).rect();
    assert!(
        row.bottom() <= new.top() && row.top() >= TITLEBAR_H,
        "{row:?} hidden"
    );
}

#[test]
fn the_delay_menu_stays_inside_the_window_and_picks_a_size() {
    let mut h = open(demo_config());
    let label = delay_label(h.state().cfg.buffer_frames);
    click(&mut h, &label);
    h.get_by_label("Audio delay");
    let win = h.ctx.content_rect();
    let other = *BUFFER_FRAMES
        .iter()
        .find(|n| **n != h.state().cfg.buffer_frames)
        .unwrap();
    for n in BUFFER_FRAMES {
        let r = h.get_all_by_label(&delay_option(n)).last().unwrap().rect();
        assert!(win.contains_rect(r), "{n}: {r:?} leaves {win:?}");
    }
    h.get_all_by_label(&delay_option(other))
        .last()
        .unwrap()
        .click();
    settle(&mut h);
    assert_eq!(h.state().cfg.buffer_frames, other);
    assert_eq!(
        h.get_all_by_label_contains("Audio delay:").count(),
        1,
        "closed"
    );
    assert!(!has(&h, "Audio delay"), "closed");
}

#[test]
fn device_rows_show_the_live_delay() {
    let h = open(demo_config());
    // Two buffers of 256 frames at 48 kHz.
    h.get_by_label("Loopback Audio Delay 10.7 ms, Apps 29 ms"); // apps are captured into it
    h.get_by_label("Stream Mix Delay 10.7 ms");
    assert!(
        !has(&h, "Zoom Out Delay 10.7 ms"),
        "a device that is off reports no delay"
    );
}

#[test]
fn the_delay_menu_warns_about_low_power_mode_only_for_small_delays() {
    let low_power = |frames: u32| {
        let mut cfg = demo_config();
        cfg.buffer_frames = frames;
        let mut b = FakeBackend::frozen(1.5);
        b.status = EngineStatus {
            low_power_mode: true,
            ..demo_status()
        };
        let mut h = window(cfg, Box::new(b), None, Theme::Dark, false);
        click(&mut h, &delay_label(frames));
        h
    };
    assert!(has(&low_power(64), crate::model::LOW_POWER_HINT));
    assert!(has(&low_power(128), crate::model::LOW_POWER_HINT));
    assert!(!has(&low_power(256), crate::model::LOW_POWER_HINT));
    let mut h = open(demo_config());
    click(&mut h, &delay_label(256));
    assert!(
        !has(&h, crate::model::LOW_POWER_HINT),
        "Low Power Mode is off"
    );
}

#[test]
fn measure_delay_runs_from_the_menu_and_shows_the_result() {
    let mut h = open(demo_config());
    click(&mut h, &delay_label(256));
    click(&mut h, "Measure delay…");
    h.get_by_label("Measure delay");
    h.get_by_label("Measured delay: 5.4 ms");
    h.get_by_label_contains("Through Loopback Audio");
    click(&mut h, "Measure again");
    h.get_by_label("Measured delay: 5.4 ms");
    click(&mut h, "Done");
    assert!(!has(&h, "Measured delay: 5.4 ms"), "closed");
}

#[test]
fn a_failed_measurement_says_why() {
    let mut b = FakeBackend::frozen(1.5);
    b.measured_ms = None;
    let mut h = window(demo_config(), Box::new(b), None, Theme::Dark, false);
    click(&mut h, &delay_label(256));
    click(&mut h, "Measure delay…");
    h.get_by_label("Measured delay: Failed");
    h.get_by_label_contains("The measurement failed");
    click(&mut h, "Done");
}

#[test]
fn measure_delay_needs_an_enabled_device() {
    let mut cfg = demo_config();
    cfg.devices.iter_mut().for_each(|d| d.enabled = false);
    let mut h = open(cfg);
    click(&mut h, &delay_label(256));
    assert!(
        h.get_by_label("Measure delay…")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn switches_and_buttons_work_from_the_keyboard() {
    let mut h = open(demo_config());
    for (label, key) in [
        ("Enable Loopback Audio", egui::Key::Space),
        ("Mute Loopback Audio", egui::Key::Enter),
    ] {
        let before = h.state().cfg.clone();
        h.get_by_label(label).focus();
        settle(&mut h);
        h.key_press(key);
        settle(&mut h);
        assert_ne!(h.state().cfg, before, "{label}");
    }
    // The Output channels menu opens from the keyboard and its rows answer it too.
    h.get_by_label("Output channel options").focus();
    settle(&mut h);
    h.key_press(egui::Key::Space);
    settle(&mut h);
    h.get_by_label("Add channel pair").focus();
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    assert_eq!(h.state().cfg.devices[0].channels, 4);
}

/// The fake backend, recording which device each self-test ran on.
struct Tested(FakeBackend, Arc<Mutex<Vec<String>>>);

impl Backend for Tested {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()> {
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
    fn self_test(&mut self, id: &str) -> anyhow::Result<SelfTest> {
        self.1.lock().unwrap().push(id.to_string());
        self.0.self_test(id)
    }
}

#[test]
fn test_setup_tests_an_enabled_device_not_a_disabled_selection() {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let mut b = FakeBackend::frozen(1.5);
    b.status = installed();
    b.self_test = flowing();
    let mut h = window(
        demo_config(),
        Box::new(Tested(b, ids.clone())),
        None,
        Theme::Dark,
        false,
    );
    click(&mut h, "Enable Loopback Audio"); // the selected device, now off
    click(&mut h, "Test setup");
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| {
        has(h, "Audio flows (-6 dBFS, 0 dropouts)")
    });
    assert_eq!(*ids.lock().unwrap(), ["v2"]);
    click(&mut h, "Continue");
    h.get_by_label_contains("choose “Stream Mix”");

    // Nothing enabled (Zoom Out starts off): nothing to test, and the sheet says so instead of
    // testing a dead device.
    click(&mut h, "Done");
    click(&mut h, "Enable Stream Mix");
    assert!(h.state().cfg.devices.iter().all(|d| !d.enabled));
    click(&mut h, "Test setup");
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| has(h, "Copy diagnostics"));
    h.get_by_label_contains("no enabled device");
    assert_eq!(*ids.lock().unwrap(), ["v2"], "no second test ran");
}

#[test]
fn a_standing_warning_never_hides_the_undo() {
    let mut h = with_status(EngineStatus {
        warnings: vec!["wire A".into()],
        ..demo_status()
    });
    click(&mut h, "Remove Safari");
    h.get_by_label("Removed Safari");
    click(&mut h, "Undo");
    h.get_by_label("Safari enabled");
    h.get_by_label("wire A");
}

#[test]
fn a_monitor_wire_past_the_hardware_channels_stays_visible_and_removable() {
    // Bluetooth headphones drop to one output channel in a call; the second wire is not lost.
    let mut cfg = demo_config();
    let m = &mut cfg.devices[0].monitors[0];
    assert_eq!(m.device_uid, "BuiltInSpeakerDevice");
    m.wires.push(sr_core::config::MonitorWire {
        out_ch: 0,
        dst_ch: 3,
    });
    let mut h = open(cfg);
    click(&mut h, "MacBook Pro Speakers channel 4 input");
    let m = &h.state().cfg.devices[0].monitors[0];
    assert!(m.wires.iter().all(|w| w.dst_ch < 2), "{:?}", m.wires);
}

#[test]
fn choose_app_stays_inside_a_minimum_size_window() {
    let mut h = open(demo_config());
    h.set_size(vec2(1000.0, 520.0));
    let apps = (0..80)
        .map(|i| AppInfo {
            bundle_id: format!("com.example.{i:02}"),
            name: format!("App {i:02}"),
            running_output: false,
            regular: true,
        })
        .collect();
    h.state_mut().graph.installed.set(apps).unwrap();
    settle(&mut h);
    click(&mut h, "Add source");
    click(&mut h, "Choose App…");
    let win = h.ctx.content_rect();
    let first = h.get_by_label("App 00").rect();
    assert!(win.contains_rect(first), "{first:?} leaves {win:?}");
    h.get_by_label("App 79").scroll_to_me();
    settle(&mut h);
    let last = h.get_by_label("App 79").rect();
    assert!(win.contains_rect(last), "{last:?} leaves {win:?}");
    click(&mut h, "App 79");
    assert_eq!(
        h.state().cfg.devices[0].sources.last().unwrap().kind,
        SourceKind::App {
            bundle_id: "com.example.79".into()
        }
    );
    assert!(!has(&h, "Choose App…"), "both menus closed");
}

/// Deterministic stand-in for AppKit: a rounded square tinted by the key, nothing for `*.noicon`.
fn fake_icon(key: &str) -> Option<egui::ColorImage> {
    if key.ends_with(".noicon") {
        return None;
    }
    let h = key
        .bytes()
        .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b)));
    let tint = egui::Color32::from_rgb(
        (h >> 16) as u8 | 0x40,
        (h >> 8) as u8 | 0x40,
        h as u8 | 0x40,
    );
    let mut img = egui::ColorImage::filled([64, 64], tint);
    for y in 0..64usize {
        for x in 0..64usize {
            // Transparent corners (radius 14) and a light disc, like a glyph.
            let (dx, dy) = (x.min(63 - x) as f32, y.min(63 - y) as f32);
            let out =
                dx < 14.0 && dy < 14.0 && ((14.0 - dx).powi(2) + (14.0 - dy).powi(2)).sqrt() > 14.0;
            let glyph = ((x as f32 - 32.0).powi(2) + (y as f32 - 32.0).powi(2)).sqrt() < 12.0;
            if out {
                img.pixels[y * 64 + x] = egui::Color32::TRANSPARENT;
            } else if glyph {
                img.pixels[y * 64 + x] = egui::Color32::from_white_alpha(200);
            }
        }
    }
    Some(img)
}

/// `n` installed apps `App 000…`, with `com.example.007` and `com.example.noicon`-style gaps made by the caller.
fn installed_apps(n: usize) -> Vec<AppInfo> {
    (0..n)
        .map(|i| AppInfo {
            bundle_id: if i == 7 {
                "com.example.007.noicon".into()
            } else {
                format!("com.example.{i:03}")
            },
            name: format!("App {i:03}"),
            running_output: false,
            regular: true,
        })
        .collect()
}

/// Window with `n` installed apps whose icon lookups wait for `answer_icons`; returns what was looked up.
fn choose_app_window(n: usize) -> (Harness<'static, App>, Arc<Mutex<Vec<String>>>) {
    let mut h = open(demo_config());
    h.state_mut()
        .graph
        .installed
        .set(installed_apps(n))
        .unwrap();
    let looked_up = Arc::new(Mutex::new(Vec::new()));
    let log = looked_up.clone();
    h.state_mut().graph.picker.manual(Box::new(move |k| {
        log.lock().unwrap().push(k.to_string());
        fake_icon(k)
    }));
    settle(&mut h);
    click(&mut h, "Add source");
    click(&mut h, "Choose App…");
    (h, looked_up)
}

fn answer_icons(h: &mut Harness<App>) {
    let ctx = h.ctx.clone();
    let waiting = !h.state().graph.picker.queued().is_empty();
    h.state_mut().graph.picker.answer(&ctx);
    h.step();
    // The first frame after an answer exists because the loader asked for it.
    assert!(
        !waiting
            || h.ctx
                .repaint_causes()
                .iter()
                .any(|c| c.file.contains("app_icons")),
        "an arriving icon must wake the UI: {:?}",
        h.ctx.repaint_causes()
    );
    settle(h);
}

/// Row number of `App 012`-style keys and labels.
fn row_of(key: &str) -> usize {
    key.trim_start_matches("com.example.")[..3].parse().unwrap()
}

#[test]
fn choose_app_shows_placeholders_first_then_icons_when_the_loader_answers() {
    let (mut h, looked_up) = choose_app_window(300);
    let p = &h.state().graph.picker;
    let asked = p.queued();
    assert!(
        !asked.is_empty() && asked.len() <= 24,
        "{} lookups",
        asked.len()
    );
    assert!(
        asked.iter().all(|k| row_of(k) < 24),
        "only the top of the list: {asked:?}"
    );
    assert_eq!(
        (p.textures(), p.missing()),
        (0, 0),
        "placeholders until the loader answers"
    );
    h.get_by_label("App 000"); // a placeholder row is still a named button
    assert!(
        looked_up.lock().unwrap().is_empty(),
        "nothing resolves on the UI thread"
    );

    answer_icons(&mut h);
    let p = &h.state().graph.picker;
    assert_eq!(p.textures() + p.missing(), asked.len());
    assert_eq!(
        p.missing(),
        1,
        "the app without an icon is remembered, not retried"
    );
    assert!(p.queued().iter().all(|k| !asked.contains(k)), "no repeats");
    h.get_by_label("App 000");
    h.get_by_label("App 007");

    // Settled with the list open: no frames are asked for.
    answer_icons(&mut h);
    h.run_steps(60);
    assert!(
        !h.ctx.has_requested_repaint(),
        "{:?}",
        h.ctx.repaint_causes()
    );
}

#[test]
fn choose_app_scrolling_requests_only_the_rows_in_view() {
    let (mut h, looked_up) = choose_app_window(300);
    answer_icons(&mut h);
    answer_icons(&mut h);
    h.get_by_label("App 250").scroll_to_me();
    settle(&mut h);
    let asked = h.state().graph.picker.queued();
    assert!(!asked.is_empty(), "the rows now in view are requested");
    assert!(
        asked.iter().all(|k| (230..300).contains(&row_of(k))),
        "only near the viewport: {asked:?}"
    );
    answer_icons(&mut h);
    answer_icons(&mut h);
    let all = looked_up.lock().unwrap().clone();
    assert!(all.len() < 100, "{} of 300 rows resolved", all.len());
    assert!(
        !all.iter().any(|k| (60..220).contains(&row_of(k))),
        "rows never in view stay unresolved"
    );
    let mut once = all.clone();
    once.sort();
    once.dedup();
    assert_eq!(once.len(), all.len(), "no icon is looked up twice");

    // Picking closes both menus and drops what the list held.
    click(&mut h, "App 250");
    h.run_steps(3);
    let p = &h.state().graph.picker;
    assert_eq!((p.textures(), p.in_flight(), p.queued().len()), (0, 0, 0));
    assert_eq!(p.missing(), 1, "failures last the session");
    assert_eq!(
        h.state().cfg.devices[0].sources.last().unwrap().kind,
        SourceKind::App {
            bundle_id: "com.example.250".into()
        }
    );
}

#[test]
fn choose_app_rows_keep_their_names_and_disable_apps_already_added() {
    let (mut h, _) = choose_app_window(12);
    answer_icons(&mut h);
    for i in 0..12 {
        h.get_by_label(&format!("App {i:03}"));
    }
    // The icon is only decoration: the accessible name is the app's name alone.
    assert!(!has(&h, "App 003 icon"));
    click(&mut h, "App 003");
    click(&mut h, "Add source");
    click(&mut h, "Choose App…");
    assert!(
        h.get_by_label("App 003").accesskit_node().is_disabled(),
        "already a source"
    );
    assert!(!h.get_by_label("App 004").accesskit_node().is_disabled());
}

/// Two quick clicks at one spot (a double-click), a frame apart, as a mouse delivers them.
fn two_clicks(h: &mut Harness<App>, label: &str) {
    let at = center(h, label);
    h.hover_at(at);
    h.step();
    for _ in 0..2 {
        h.drag_at(at);
        h.step();
        h.drop_at(at);
        h.step();
    }
    settle(h);
}

#[test]
fn a_double_click_on_a_remove_button_removes_one_thing_only() {
    // The second click would land on whatever moved up under the pointer, and only the last
    // removal can be undone: the first one would be lost for good.
    let mut h = open(demo_config());
    two_clicks(&mut h, "Remove Loopback Audio");
    assert_eq!(names(&h), ["Stream Mix", "Zoom Out"]);
    h.get_by_label("Removed Loopback Audio");
    let mut h = open(demo_config());
    let sources = h.state().cfg.devices[0].sources.len();
    two_clicks(&mut h, "Remove Safari");
    assert_eq!(h.state().cfg.devices[0].sources.len(), sources - 1);
    h.get_by_label("Spotify enabled");
    // Same for the Remove Source link of two open Options panes.
    let mut h = open(demo_config());
    click(&mut h, "Safari options");
    click(&mut h, "Spotify options");
    two_clicks(&mut h, "Remove Source Safari");
    assert_eq!(h.state().cfg.devices[0].sources.len(), sources - 1);
    h.get_by_label("Remove Source Spotify");
}

#[test]
fn an_idle_sheet_asks_for_no_more_frames() {
    let idle = |h: &mut Harness<App>, what: &str| {
        h.run_steps(60);
        assert!(
            !h.ctx.has_requested_repaint(),
            "{what}: {:?}",
            h.ctx.repaint_causes()
        );
    };
    let mut h = setup_run(installed(), flowing());
    idle(&mut h, "permissions");
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| {
        has(h, "Audio flows (-6 dBFS, 0 dropouts)")
    });
    idle(&mut h, "verify passed");
    click(&mut h, "Continue");
    idle(&mut h, "done");
    click(&mut h, "Done");
    uninstall(&mut h);
    idle(&mut h, "uninstall dialog");
    let mut h = with_runner(no_driver(), never_run());
    idle(&mut h, "welcome");
    click(&mut h, "Install driver");
    until(&mut h, "runner result", |h| has(h, "Driver not installed"));
    idle(&mut h, "install failed");
}

#[test]
fn a_click_that_closes_a_menu_only_closes_it() {
    // As on macOS: dismissing a menu by clicking elsewhere never also presses what is there.
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    click(&mut h, "Add monitor");
    h.get_by_label("Output Devices");
    click(&mut h, "Remove Spotify");
    assert!(!has(&h, "Output Devices"), "the menu closed");
    assert_eq!(h.state().cfg, before);
    // The same click once the menu is gone does remove it.
    click(&mut h, "Remove Spotify");
    assert_ne!(h.state().cfg, before);

    // A click on a wire, which removes it, while the latency menu is open.
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    let (a, b) = (
        center(&h, "Output channel 2 output"),
        center(&h, "MacBook Pro Speakers channel 2 input"),
    );
    let [p0, p1, p2, p3] = crate::layout::wire_points(a, b);
    let mid =
        ((p0.to_vec2() + 3.0 * p1.to_vec2() + 3.0 * p2.to_vec2() + p3.to_vec2()) / 8.0).to_pos2();
    let l = delay_label(before.buffer_frames);
    click(&mut h, &l);
    for _ in 0..2 {
        h.hover_at(mid);
        h.step();
        h.drag_at(mid);
        h.step();
        h.drop_at(mid);
        settle(&mut h);
        if h.state().cfg == before {
            assert_eq!(
                h.get_all_by_label_contains("Audio delay:").count(),
                1,
                "closed"
            );
        }
    }
    let wires = |c: &Config| c.devices[0].monitors[0].wires.len();
    assert_eq!(
        wires(&h.state().cfg),
        wires(&before) - 1,
        "second click only"
    );
}

fn esc(h: &mut Harness<App>) {
    h.key_press(egui::Key::Escape);
    settle(h);
}

#[test]
fn esc_leaves_the_welcome_and_the_driver_step_alone() {
    // Neither has anything to cancel, and Esc must not choose for the user.
    let (runner, calls) = recording(Ok(()));
    let mut h = with_runner(no_driver(), runner.clone());
    h.state_mut().setup = Setup::new(runner, None).with_intro();
    settle(&mut h);
    h.get_by_label("Welcome to SoundRouter");
    esc(&mut h);
    h.get_by_label("Welcome to SoundRouter");
    click(&mut h, "Get started");
    h.get_by_label("Install driver");
    esc(&mut h);
    h.get_by_label("Install driver");
    h.get_by_label("Try the demo");
    assert!(calls.lock().unwrap().is_empty(), "no install was started");
    assert!(!h.state().demo, "no demo was chosen");
}

#[test]
fn esc_closes_the_permissions_step_like_close_and_runs_no_test() {
    let status = EngineStatus {
        mic: TccStatus::Denied,
        ..installed()
    };
    let mut h = with_runner(status, never_run());
    assert!(h.state().setup.shown());
    esc(&mut h);
    assert!(!h.state().setup.shown() && h.state().setup.seen());
    assert_eq!(
        h.state().worker.shared.lock().unwrap().self_test,
        SelfTestState::Idle
    );
}

#[test]
fn esc_cancels_a_running_test_and_closes_a_finished_one_without_testing_again() {
    let mut h = setup_run(installed(), flowing());
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| {
        has(h, "Audio flows (-6 dBFS, 0 dropouts)")
    });
    // Passed: the only button is Continue, so Esc leaves the sheet where it is.
    esc(&mut h);
    h.get_by_label("Continue");
    // Running: Esc is the Cancel button.
    h.state().worker.shared.lock().unwrap().self_test = SelfTestState::Running;
    settle(&mut h);
    h.get_by_label("Cancel");
    esc(&mut h);
    assert!(!has(&h, "Cancel") && !has(&h, "Loopback test"));

    // Failed: Esc is Close, not "Test again".
    let mut h = setup_run(installed(), None);
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| has(h, "Copy diagnostics"));
    h.get_by_label("Test again");
    esc(&mut h);
    assert!(!has(&h, "Test again") && !has(&h, "Copy diagnostics"));
    assert!(matches!(
        h.state().worker.shared.lock().unwrap().self_test,
        SelfTestState::Done(Err(_))
    ));
}

#[test]
fn esc_closes_the_all_set_sheet() {
    let mut h = setup_run(installed(), flowing());
    click(&mut h, "Run test");
    until(&mut h, "self-test", |h| {
        has(h, "Audio flows (-6 dBFS, 0 dropouts)")
    });
    click(&mut h, "Continue");
    h.get_by_label("Done");
    esc(&mut h);
    assert!(!has(&h, "Done") && !has(&h, "Continue"));
}

#[test]
fn esc_closes_the_measure_delay_sheet_running_or_done_without_measuring_again() {
    let mut h = open(demo_config());
    click(&mut h, &delay_label(256));
    click(&mut h, "Measure delay…");
    h.get_by_label("Measured delay: 5.4 ms");
    esc(&mut h);
    assert!(!has(&h, "Measured delay: 5.4 ms") && !has(&h, "Measure again"));
    assert!(
        matches!(
            h.state().worker.shared.lock().unwrap().measure,
            crate::worker::MeasureState::Done(Ok(_))
        ),
        "not measured again"
    );

    click(&mut h, &delay_label(256));
    click(&mut h, "Measure delay…");
    h.state().worker.shared.lock().unwrap().measure = crate::worker::MeasureState::Running;
    settle(&mut h);
    h.get_by_label("Cancel");
    esc(&mut h);
    assert!(!has(&h, "Cancel") && !has(&h, "Measure again"));
}

#[test]
fn esc_cancels_the_uninstall_question() {
    let (runner, calls) = recording(Ok(()));
    let mut h = with_runner(installed(), runner);
    uninstall(&mut h);
    h.get_by_label("Uninstall the audio driver?");
    esc(&mut h);
    assert!(!has(&h, "Uninstall the audio driver?"));
    assert!(calls.lock().unwrap().is_empty(), "nothing was uninstalled");
}

#[test]
fn esc_closes_each_menu_and_does_nothing_else() {
    let mut h = with_runner(installed(), never_run());
    let before = h.state().cfg.clone();
    for (opener, item) in [
        (delay_label(256), "Measure delay…"),
        ("More actions".to_string(), "Uninstall…"),
        ("Add source".to_string(), "Search sources"),
    ] {
        click(&mut h, &opener);
        h.get_by_label(item);
        esc(&mut h);
        assert!(!has(&h, item), "{item} stays open");
    }
    assert_eq!(h.state().cfg, before, "no edit was made");
    assert!(!h.state().setup.shown(), "no sheet was opened");
}

#[test]
fn the_os_reduce_motion_setting_turns_the_switch_slide_and_sheet_fade_off() {
    let mut h = open(demo_config());
    let knob = egui::Id::unique("a switch knob");
    // A switch asks egui for the animated value of its state; right after a toggle that is the
    // start of the slide, or already the end of it.
    let after_toggle = |h: &Harness<App>| {
        h.ctx.animate_bool_responsive(knob, false);
        h.ctx.animate_bool_responsive(knob, true)
    };
    assert!(h.ctx.global_style().animation_time > 0.0);
    assert!(after_toggle(&h) < 1.0, "slides");

    h.state_mut().os.reduce_motion = Arc::new(|| true);
    settle(&mut h);
    assert_eq!(
        h.ctx.global_style().animation_time,
        0.0,
        "sheets do not fade"
    );
    let knob = egui::Id::unique("another switch knob");
    h.ctx.animate_bool_responsive(knob, false);
    assert_eq!(h.ctx.animate_bool_responsive(knob, true), 1.0, "jumps");

    h.state_mut().os.reduce_motion = Arc::new(|| false);
    settle(&mut h);
    assert!(h.ctx.global_style().animation_time > 0.0, "and back");
}

// ---- cable trim: the popover on a cable's middle -------------------------------------------

fn wire0(h: &Harness<App>, i: usize) -> sr_core::config::Wire {
    h.state().cfg.devices[0].wires[i].clone()
}

#[test]
fn a_cables_chip_opens_its_popover_and_esc_closes_it() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    assert!(!has(&h, "Cable gain, dB"));
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    h.get_by_label("Cable gain, dB");
    h.get_by_label("Cable gain value, dB");
    h.get_by_label("Flip polarity");
    // Two separate channels are no L/R pair: no pan.
    assert!(!has(&h, "Cable pan"));
    assert_eq!(h.state().cfg, before, "opening edits nothing");
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert!(!has(&h, "Cable gain, dB"));
    assert_eq!(h.state().cfg, before);
}

#[test]
fn the_chip_is_reachable_by_keyboard() {
    let mut h = open(demo_config());
    chip_node!(h, "Safari", 1, 1).focus();
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    h.get_by_label("Cable gain, dB");
    h.get_by_label("Flip polarity");
}

#[test]
fn the_gain_slider_and_field_trim_one_cable() {
    let mut h = open(demo_config());
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    // The slider steps with the arrow keys, in tenths of a dB.
    h.get_by_label("Cable gain, dB").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    let g = wire0(&h, 0).gain_db;
    assert!(
        (-1.0..0.0).contains(&g) && (g * 10.0).round() == g * 10.0,
        "{g}"
    );
    // The cable's own chip now says so, for screen readers and for the eye.
    h.get_by_label_contains(&format!("{}, -0.6 dB", chip("Safari", 0, 0)));
    // Typing a value takes it on Enter; only this cable changes.
    let field = h.get_by_label("Cable gain value, dB");
    field.focus();
    settle(&mut h);
    select_all(&mut h);
    h.get_by_label("Cable gain value, dB").type_text("-4.5");
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    assert_eq!(wire0(&h, 0).gain_db, -4.5);
    assert!(
        h.state().cfg.devices[0].wires[1..]
            .iter()
            .all(|w| w.gain_db == 0.0)
    );
    h.get_by_label_contains(&format!("{}, -4.5 dB", chip("Safari", 0, 0)));
    h.state().cfg.validate().unwrap();
}

#[test]
fn a_gain_out_of_range_is_held_to_the_range_and_junk_is_ignored() {
    let mut h = open(demo_config());
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    let type_in = |h: &mut Harness<App>, text: &str, key: egui::Key| {
        h.get_by_label("Cable gain value, dB").focus();
        settle(h);
        select_all(h);
        h.get_by_label("Cable gain value, dB").type_text(text);
        h.key_press(key);
        settle(h);
    };
    type_in(&mut h, "30", egui::Key::Enter);
    assert_eq!(wire0(&h, 0).gain_db, sr_core::config::MAX_DB);
    assert!(!has(&h, "That change was not applied"));
    type_in(&mut h, "-500", egui::Key::Enter);
    assert_eq!(wire0(&h, 0).gain_db, crate::model::MIN_DB);
    type_in(&mut h, "loud", egui::Key::Enter);
    assert_eq!(wire0(&h, 0).gain_db, crate::model::MIN_DB);
    // Escape cancels: the typed value is dropped and the field shows the real gain again.
    type_in(&mut h, "-3", egui::Key::Escape);
    assert_eq!(wire0(&h, 0).gain_db, crate::model::MIN_DB);
    h.state().cfg.validate().unwrap();
}

#[test]
fn flip_polarity_toggles_one_cable_and_the_chip_says_so() {
    let mut h = open(demo_config());
    assert!(!chip_label_has(&h, "inverted"));
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    click(&mut h, "Flip polarity");
    assert!(wire0(&h, 0).invert);
    assert!(!wire0(&h, 1).invert);
    h.get_by_label_contains(&format!("{}, inverted", chip("Safari", 0, 0)));
    click(&mut h, "Flip polarity");
    assert!(!wire0(&h, 0).invert);
}

fn chip_label_has(h: &Harness<App>, text: &str) -> bool {
    h.query_all_by_label_contains("Cable trim, ")
        .any(|n| n.accesskit_node().label().is_some_and(|l| l.contains(text)))
}

#[test]
fn a_mono_pair_has_a_pan_that_writes_both_cables() {
    let mut h = open(demo_config());
    click(&mut h, "Select Stream Mix");
    let mic = h.state().cfg.devices[1].sources[1].clone();
    assert_eq!((mic.channels, mic.id.as_str()), (1, "s2"));
    // The mono input's channel has no side label: it feeds both sides of the pair.
    let name = h
        .query_all_by_label_contains("Cable trim, ")
        .filter_map(|n| n.accesskit_node().label())
        .find(|l| l.contains(", channel 1 to Channel 1 (L)"))
        .unwrap();
    h.get_by_label_contains(&name).click();
    settle(&mut h);
    h.get_by_label("Cable pan");
    let w = |h: &Harness<App>| {
        let mut w = h.state().cfg.devices[1].wires.clone();
        w.retain(|w| w.source == "s2");
        w
    };
    assert_eq!(mic.channels, 1);
    assert_eq!(w(&h).len(), 2);
    h.get_by_label("Cable pan").focus();
    settle(&mut h);
    for _ in 0..25 {
        h.key_press(egui::Key::ArrowRight);
        settle(&mut h);
    }
    let (l, r) = (w(&h)[0].gain_db, w(&h)[1].gain_db);
    assert!(r > l, "panned right: {l} {r}");
    // Constant power: the pair carries what the two unity cables did (+3.01 dB).
    let p = |g: f32| 10f32.powf(g / 10.0);
    assert!((p(l) + p(r) - 2.0).abs() < 0.02, "{l} {r}");
    // Both cables are trimmed, so both chips say so.
    assert!(chip_label_has(&h, " dB"));
    h.state().cfg.validate().unwrap();
    // Dragging back to centre restores unity cables.
    for _ in 0..25 {
        h.key_press(egui::Key::ArrowLeft);
        settle(&mut h);
    }
    assert!(w(&h).iter().all(|w| w.gain_db.abs() < 0.05), "{:?}", w(&h));
}

#[test]
fn two_cables_into_a_channel_offer_to_balance_them_and_undo_takes_it_back() {
    let mut h = open(two_sources());
    let before = h.state().cfg.clone();
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    h.get_by_label("Two cables add up (+6 dB). Trim each by -6 dB.");
    click(&mut h, "Balance cables");
    let d = &h.state().cfg.devices[0];
    let gains: Vec<_> = d
        .wires
        .iter()
        .filter(|w| w.to.is_none())
        .map(|w| (w.out_ch, w.gain_db))
        .collect();
    // Only the cables into the channel that was open (Safari's and the Music mixer's); the
    // other channel still has its pair, and the cables into the mixer keep theirs.
    assert_eq!(gains, [(0, -6.0), (1, 0.0), (0, -6.0), (1, 0.0)]);
    assert!(d.wires.iter().all(|w| w.to.is_none() || w.gain_db == 0.0));
    // The popover closes; the toast offers the way back.
    assert!(!has(&h, "Balance cables"));
    h.get_by_label("Balanced cables into channel 1 (L)");
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    // Open again: now balanced cables say so and offer nothing.
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    click(&mut h, "Balance cables");
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    h.get_by_label_contains("are balanced, each trimmed by -6 dB");
    assert!(!has(&h, "Balance cables"));
}

#[test]
fn three_cables_say_how_much_they_add_up_to() {
    let mut h = open(demo_config());
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    h.get_by_label("Three cables add up (+9.5 dB). Trim each by -9.5 dB.");
    click(&mut h, "Balance cables");
    assert!(
        h.state().cfg.devices[0]
            .wires
            .iter()
            .filter(|w| w.to.is_none() && w.out_ch == 0)
            .all(|w| w.gain_db == -9.5)
    );
}

#[test]
fn a_lone_cable_offers_no_balance() {
    let mut cfg = demo_config();
    apply(&mut cfg, Edit::RemoveNode(0, 0));
    apply(&mut cfg, Edit::RemoveSource(0, 2));
    apply(&mut cfg, Edit::RemoveSource(0, 1));
    let mut h = open(cfg);
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    h.get_by_label("Cable gain, dB");
    assert!(!has(&h, "Balance cables"));
    assert!(h.query_by_label_contains("add up").is_none());
}

#[test]
fn the_x_beside_a_cable_chip_is_the_one_way_to_unplug_it() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    let unplug = "Unplug cable, Safari, channel 1 (L) to Channel 1 (L)";
    // Out of sight until the cable is aimed at.
    assert!(!has(&h, unplug));
    let at = chip_node!(h, "Safari", 0, 0).rect().center();
    h.hover_at(at);
    settle(&mut h);
    h.get_by_label(unplug);
    // Clicking the chip trims, it never unplugs.
    tap_at(&mut h, at);
    h.get_by_label("Cable gain, dB");
    assert_eq!(h.state().cfg, before);
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    // Keyboard focus shows the x too.
    chip_node!(h, "Safari", 0, 0).focus();
    settle(&mut h);
    click(&mut h, unplug);
    let d = &h.state().cfg.devices[0];
    assert_eq!(d.wires.len(), before.devices[0].wires.len() - 1);
    assert!(
        !d.wires
            .iter()
            .any(|w| w.source == "s1" && w.src_ch == 0 && w.out_ch == 0)
    );
}

fn tap_at(h: &mut Harness<App>, at: egui::Pos2) {
    h.hover_at(at);
    h.step();
    h.drag_at(at);
    h.step();
    h.drop_at(at);
    settle(h);
}

#[test]
fn an_idle_window_with_a_trimmed_cable_and_an_open_popover_asks_for_no_frames() {
    let mut cfg = demo_config();
    let l = crate::model::Link {
        from: crate::model::Port::Src { src: 0, ch: 0 },
        to: crate::model::Port::OutIn { ch: 0 },
    };
    apply(&mut cfg, Edit::WireGain(0, l, -6.0));
    let mut h = open(cfg);
    h.run_steps(60);
    assert!(
        !h.ctx.has_requested_repaint(),
        "{:?}",
        h.ctx.repaint_causes()
    );
    chip_node!(h, "Safari", 0, 0).click();
    settle(&mut h);
    h.run_steps(60);
    assert!(
        !h.ctx.has_requested_repaint(),
        "{:?}",
        h.ctx.repaint_causes()
    );
}

// ---- mixers: the strips between Sources and Output Channels --------------------------------

fn music(h: &Harness<App>) -> sr_core::config::Node {
    h.state().cfg.devices[0].nodes[0].clone()
}

fn links0(h: &Harness<App>) -> Vec<crate::model::Link> {
    crate::model::links(&h.state().cfg.devices[0])
}

fn type_into(h: &mut Harness<App>, label: &str, text: &str) {
    h.get_by_label(label).focus();
    settle(h);
    select_all(h);
    h.get_by_label(label).type_text(text);
    h.key_press(egui::Key::Enter);
    settle(h);
}

#[test]
fn the_plus_over_a_cable_inserts_a_mixer_on_it() {
    use crate::model::{Link, Port};
    let mut h = open(demo_config());
    let at = chip_node!(h, "Safari", 0, 0).rect().center();
    h.hover_at(at);
    settle(&mut h);
    let plus = "Insert mixer on cable, Safari, channel 1 (L) to Channel 1 (L)";
    let p = h.get_by_label(plus).rect().center();
    tap_at(&mut h, p);
    let d = &h.state().cfg.devices[0];
    assert_eq!(d.nodes.len(), 2);
    assert_eq!(d.nodes[1].name, "Mixer");
    // One cable in, one cable out, on the side the cable went to.
    let all = links0(&h);
    assert!(all.contains(&Link {
        from: Port::Src { src: 0, ch: 0 },
        to: Port::NodeIn { node: 1, ch: 0 },
    }));
    assert!(all.contains(&Link {
        from: Port::NodeOut { node: 1, ch: 0 },
        to: Port::OutIn { ch: 0 },
    }));
    h.get_by_label("Mixer settings");
    h.state().cfg.validate().unwrap();
}

#[test]
fn a_mixer_comes_from_the_output_menu_or_a_sources_options() {
    use crate::model::Port;
    let mut h = open(one_source());
    assert!(!has(&h, "Mixer settings"));
    click(&mut h, "Output channel options");
    click(&mut h, "Add mixer");
    assert_eq!(h.state().cfg.devices[0].nodes.len(), 1);
    h.get_by_label("Mixer settings");
    // An empty mixer, to wire by hand: nothing goes in or out yet.
    assert!(
        links0(&h)
            .iter()
            .all(|l| !matches!(l.to, Port::NodeIn { .. }))
    );
    click(&mut h, "Safari options");
    click(&mut h, "Add mixer for Safari");
    let d = &h.state().cfg.devices[0];
    assert_eq!(d.nodes[1].name, "Mixer 2");
    // Safari now reaches the outputs only through it.
    let all = links0(&h);
    assert!(
        all.iter()
            .filter(|l| matches!(l.from, Port::Src { .. }))
            .all(|l| l.to
                == Port::NodeIn {
                    node: 1,
                    ch: match l.from {
                        Port::Src { ch, .. } => ch,
                        _ => unreachable!(),
                    }
                })
    );
    h.get_by_label("Mixer 2 settings");
    h.state().cfg.validate().unwrap();
}

#[test]
fn cables_plug_into_and_out_of_a_mixer() {
    use crate::model::{Link, Port};
    let mut h = open(demo_config());
    let row = Port::NodeIn { node: 0, ch: 0 };
    let safari = Link {
        from: Port::Src { src: 0, ch: 0 },
        to: row,
    };
    let (a, b) = (
        center(&h, "Safari channel 1 output"),
        center(&h, "Music channel 1 input"),
    );
    drag(&mut h, a, b);
    assert!(links0(&h).contains(&safari));
    // Out of the mixer into another output channel pair.
    click(&mut h, "Output channel options");
    click(&mut h, "Add channel pair");
    let (a, b) = (
        center(&h, "Music channel 1 output"),
        center(&h, "Output channel 3 input"),
    );
    drag(&mut h, a, b);
    assert!(links0(&h).contains(&Link {
        from: Port::NodeOut { node: 0, ch: 0 },
        to: Port::OutIn { ch: 2 },
    }));
    // A click on the mixer's input jack unplugs what comes into that row, nothing else.
    let before = links0(&h).len();
    click(&mut h, "Music channel 1 input");
    let after = links0(&h);
    assert!(!after.iter().any(|l| l.to == row));
    assert_eq!(
        after.len(),
        before - 2,
        "Safari's and Spotify's cables into row 1"
    );
    h.state().cfg.validate().unwrap();
}

#[test]
fn a_strip_mutes_and_fades_from_the_keyboard() {
    let mut h = open(demo_config());
    h.get_by_label("Mute Music").focus();
    settle(&mut h);
    h.key_press(egui::Key::Space);
    settle(&mut h);
    assert!(music(&h).muted);
    // The state is in words, not only in colour.
    h.get_by_label("Mute Music");
    assert!(
        h.query_by_label("Mute Music")
            .unwrap()
            .accesskit_node()
            .toggled()
            .is_some()
    );
    h.get_by_label("Music volume").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    assert!(music(&h).gain_db < 0.0);
    h.state().cfg.validate().unwrap();
}

#[test]
fn the_inspector_works_from_the_keyboard_and_esc_closes_it() {
    use sr_core::config::Fx;
    let mut h = open(demo_config());
    assert!(!has(&h, "Mixer name"));
    h.get_by_label("Music settings").focus();
    settle(&mut h);
    h.key_press(egui::Key::Enter);
    settle(&mut h);
    h.get_by_label("Mixer name");
    // Arrow keys step a slider by 1 % of its range.
    h.get_by_label("Low shelf gain, dB").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowRight);
    settle(&mut h);
    let Fx::Eq { low_db, .. } = music(&h).fx[0] else {
        panic!("EQ first")
    };
    assert!((low_db - 2.4).abs() < 0.01, "{low_db}");
    // A typed value is held to the effect's range.
    type_into(&mut h, "Limiter ceiling value, dB", "-30");
    assert!(matches!(
        music(&h).fx[1],
        Fx::Limiter { ceiling_db, .. } if ceiling_db == -24.0
    ));
    type_into(&mut h, "Mixer name", "Tunes");
    assert_eq!(music(&h).name, "Tunes");
    h.get_by_label("Tunes settings");
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert!(!has(&h, "Mixer name"));
    h.state().cfg.validate().unwrap();
}

#[test]
fn effects_are_added_reordered_set_and_removed_in_the_inspector() {
    use sr_core::config::Fx;
    let mut h = open(demo_config());
    let kinds = |h: &Harness<App>| {
        music(h)
            .fx
            .iter()
            .map(|f| match f {
                Fx::Mono => "mono",
                Fx::Eq { .. } => "eq",
                Fx::Limiter { .. } => "limiter",
                Fx::Delay { .. } => "delay",
            })
            .collect::<Vec<_>>()
    };
    click(&mut h, "Music settings");
    click(&mut h, "Add Delay");
    assert_eq!(kinds(&h), ["eq", "limiter", "delay"]);
    // The strip shows it, and the inspector says what it costs.
    h.get_by_label("Delay 0 ms on Music");
    h.get_by_label("Adds 0 ms");
    click(&mut h, "Move Delay up");
    assert_eq!(kinds(&h), ["eq", "delay", "limiter"]);
    type_into(&mut h, "Delay time value, ms", "12");
    assert_eq!(music(&h).fx[1], Fx::Delay { ms: 12.0 });
    h.get_by_label("Adds 12 ms");
    h.get_by_label("Delay 12 ms on Music");
    click(&mut h, "Remove Limiter");
    click(&mut h, "Add Mono");
    assert_eq!(kinds(&h), ["eq", "delay", "mono"]);
    click(&mut h, "Move EQ down");
    assert_eq!(kinds(&h), ["delay", "eq", "mono"]);
    h.state().cfg.validate().unwrap();
}

/// A typed value commits as the field loses focus; the click that took the focus may remove the
/// effect above it in the same frame, and the remaining one must keep its own parameters.
#[test]
fn a_parameter_typed_as_its_effect_is_removed_does_not_land_on_the_next_one() {
    use sr_core::config::Fx;
    let lim = |ceiling_db| Fx::Limiter {
        ceiling_db,
        knee_db: 6.0,
    };
    let mut cfg = demo_config();
    cfg.devices[0].nodes[0].fx = vec![lim(-1.0), lim(-3.0)];
    let mut h = open(cfg);
    click(&mut h, "Music settings");
    h.get_all_by_label("Limiter ceiling value, dB")
        .next()
        .unwrap()
        .focus();
    settle(&mut h);
    select_all(&mut h);
    h.get_all_by_label("Limiter ceiling value, dB")
        .next()
        .unwrap()
        .type_text("-10");
    h.get_all_by_label("Remove Limiter").next().unwrap().click();
    settle(&mut h);
    assert_eq!(music(&h).fx, [lim(-3.0)]);
}

/// Escape gives the keyboard back without taking what was typed.
#[test]
fn escape_in_an_inspector_field_discards_the_typing() {
    use sr_core::config::Fx;
    let mut h = open(demo_config());
    click(&mut h, "Music settings");
    let before = music(&h);
    let label = "Limiter ceiling value, dB";
    h.get_by_label(label).focus();
    settle(&mut h);
    select_all(&mut h);
    h.get_by_label(label).type_text("-10");
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    assert_eq!(music(&h), before);
    assert!(matches!(music(&h).fx[1], Fx::Limiter { ceiling_db, .. } if ceiling_db == -1.0));
}

#[test]
fn removing_a_mixer_joins_its_cables_and_undo_brings_it_back() {
    use crate::model::{Link, Port};
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    click(&mut h, "Music settings");
    click(&mut h, "Remove Music");
    assert!(h.state().cfg.devices[0].nodes.is_empty());
    // Spotify goes straight to the outputs again.
    for ch in 0..2 {
        assert!(links0(&h).contains(&Link {
            from: Port::Src { src: 1, ch },
            to: Port::OutIn { ch },
        }));
    }
    h.get_by_label("Removed Music");
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, before);
    h.get_by_label("Music settings");
}

#[test]
fn an_idle_window_with_a_mixer_and_its_inspector_open_asks_for_no_frames() {
    let mut h = open(demo_config());
    click(&mut h, "Music settings");
    h.get_by_label("Mixer name");
    h.run_steps(60);
    assert!(
        !h.ctx.has_requested_repaint(),
        "{:?}",
        h.ctx.repaint_causes()
    );
}
