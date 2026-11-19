//! Whole-window tests: the real `App` and worker driven through the accessibility tree, so every
//! control is found by the name a screen reader would announce, then clicked/dragged/typed at.
use super::*;
use crate::fake::{FakeBackend, demo_config, demo_status};
use crate::model::bare_config;
use crate::setup::{Runner, SetupError};
use crate::worker::SelfTestState;
use egui_kittest::{Harness, kittest::Queryable};
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
fn wait_published(h: &mut Harness<App>) {
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
    let toast = toast.map(str::to_string);
    let mut h = Harness::builder()
        .with_size(vec2(1180.0, 700.0))
        .with_step_dt(0.05) // double-click window (0.3 s) spans six frames
        .with_theme(theme)
        .with_context_setup(crate::theme::install_fonts)
        .build_eframe(move |cc| {
            let w = Worker::spawn_inline(backend, cc.egui_ctx.clone(), Box::new(|_| None));
            let mut app = App::new(cfg, None, w, toast, false, demo);
            app.setup = Setup::new(runner, Some("/App/Resources/SoundRouter.driver".into()));
            app
        });
    wait_published(&mut h);
    settle(&mut h);
    h
}

fn open(cfg: Config) -> Harness<'static, App> {
    window(
        cfg,
        Box::new(FakeBackend::frozen(1.5)),
        None,
        Theme::Dark,
        false,
    )
}

/// Process queued input, let the UI react, and require every control to have an accessible name.
fn settle(h: &mut Harness<App>) {
    h.step();
    h.run_steps(3);
    h.check_accessibility();
}

fn click(h: &mut Harness<App>, label: &str) {
    h.get_by_label(label).click();
    settle(h);
}

/// The footer button wears the same label as a row's button when one permission is open.
fn click_footer(h: &mut Harness<App>, label: &str) {
    h.get_all_by_label(label).last().unwrap().click();
    settle(h);
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

fn select_all(h: &mut Harness<App>) {
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
        "New Virtual Device",
        "Remove Loopback Audio",
        "Delete",
        "Select Loopback Audio",
        "Rename Loopback Audio",
        "Enable Loopback Audio",
        "Mute Loopback Audio",
        "Loopback Audio volume",
        "Device name",
        "Add source",
        "Add channel pair",
        "Add monitor",
        "Safari enabled",
        "Safari options",
        "Safari channel 1 output",
    ] {
        h.get_by_label(label);
    }
}

#[test]
fn new_device_is_added_and_selected() {
    let mut h = open(demo_config());
    click(&mut h, "New Virtual Device");
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
    click(&mut h, "Delete");
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
    // nothing left to delete: the button is inert instead of panicking
    click(&mut h, "Delete");
    assert!(h.state().cfg.devices.is_empty());
    // and the user can start over
    click(&mut h, "New Virtual Device");
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
    click(&mut h, "Add channel pair");
    assert_eq!(h.state().cfg.devices[0].channels, 4);
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
    h.get_by_label("Demo mode: synthetic audio, nothing is saved");
    let h = open(demo_config());
    assert!(h.query_by_label_contains("Demo mode").is_none());
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
                let backend = Box::new(FakeBackend::frozen(1.5));
                let w = Worker::spawn_inline(backend, cc.egui_ctx.clone(), Box::new(|_| None));
                App::new(cfg, None, w, toast.map(str::to_string), false, false)
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

    #[test]
    fn main_view() {
        both("main", demo_config, None, |_| {});
    }

    #[test]
    fn empty_state() {
        both("empty", Config::default, None, |_| {});
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
    fn sidebar_only_window_with_row_x() {
        let mut results = egui_kittest::SnapshotResults::new();
        for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let mut h = Harness::builder()
                .with_size(vec2(243.0, 330.0))
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(crate::theme::install_fonts)
                .wgpu()
                .build_eframe(move |cc| {
                    let backend = Box::new(FakeBackend::frozen(1.5));
                    let w = Worker::spawn_inline(backend, cc.egui_ctx.clone(), Box::new(|_| None));
                    App::new(demo_config(), None, w, None, false, false)
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
            Some("Could not save config: Permission denied (os error 13)"),
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
                        Setup::new(runner, Some("/App/Resources/SoundRouter.driver".into()));
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
    fn sheet_allow_access_granted() {
        sheet(
            "allow_granted",
            WIDE,
            installed(),
            flowing(),
            never_run(),
            |h| {
                click(h, "Test Setup…");
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
                click(h, "Test Setup…");
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
                click(h, "Test Setup…");
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
                click(h, "Test Setup…");
                click(h, "Continue");
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
                click(h, "Test Setup…");
                click(h, "Continue");
                until(h, "self-test", |h| {
                    h.query_by_label_contains("Audio flows").is_some()
                });
            },
        );
    }

    #[test]
    fn sheet_self_test_fail() {
        sheet("test_fail", WIDE, installed(), None, never_run(), |h| {
            click(h, "Test Setup…");
            click(h, "Continue");
            until(h, "self-test", |h| has(h, "Copy diagnostics"));
        });
    }

    #[test]
    fn sheet_all_done() {
        sheet("done", WIDE, installed(), flowing(), never_run(), |h| {
            click(h, "Test Setup…");
            click(h, "Continue");
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
            click(h, "Install Driver…");
            until(h, "runner result", |h| has(h, "Driver not installed"));
        });
    }

    #[test]
    fn sheet_uninstall_confirm() {
        sheet("uninstall", WIDE, installed(), None, never_run(), |h| {
            click(h, "Uninstall…");
        });
    }

    #[test]
    fn sheet_uninstall_failed() {
        let runner = recording(Err(SetupError::Failed("Operation not permitted".into()))).0;
        sheet("uninstall_failed", WIDE, installed(), None, runner, |h| {
            click(h, "Uninstall…");
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
                click(h, "Install Driver…");
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
    h.get_by_label("Open Settings");
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

fn has(h: &Harness<App>, label: &str) -> bool {
    h.query_by_label(label).is_some()
}

fn no_driver() -> EngineStatus {
    EngineStatus {
        driver_version: None,
        ..demo_status()
    }
}

#[test]
fn without_a_driver_the_sheet_installs_it_and_a_cancel_says_not_installed() {
    let (runner, calls) = recording(Err(SetupError::Cancelled));
    let mut h = with_runner(no_driver(), runner);
    h.get_by_label("Try Demo");
    assert!(calls.lock().unwrap().is_empty());
    click(&mut h, "Install Driver…");
    until(&mut h, "runner result", |h| has(h, "Driver not installed"));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains("/Library/Audio/Plug-Ins/HAL"));
    assert!(calls[0].contains("'/App/Resources/SoundRouter.driver'"));
    h.get_by_label("Retry");
    h.get_by_label("Copy diagnostics");
}

#[test]
fn a_driver_that_never_shows_up_after_a_good_install_times_out() {
    let (runner, _) = recording(Ok(()));
    let mut h = with_runner(no_driver(), runner);
    click(&mut h, "Install Driver…");
    until(&mut h, "waiting for the driver", |h| {
        has(h, "Starting the driver…")
    });
    until(&mut h, "deadline", |h| has(h, "Driver not installed"));
    h.get_by_label_contains("has not loaded it yet");
}

#[test]
fn a_driver_present_hides_the_sheet_and_the_footer_offers_test_and_uninstall() {
    let h = open(demo_config()); // the demo driver: nothing to test or uninstall
    assert!(!has(&h, "Install Driver…") && !has(&h, "Test Setup…") && !has(&h, "Uninstall…"));
    let h = with_runner(installed(), never_run());
    assert!(!has(&h, "Install Driver…"));
    h.get_by_label("Test Setup…");
    h.get_by_label("Uninstall…");
}

fn setup_run(status: EngineStatus, test: Option<SelfTest>) -> Harness<'static, App> {
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    b.self_test = test;
    let mut h = window(demo_config(), Box::new(b), None, Theme::Dark, false);
    // Something missing opens the sheet by itself; otherwise the footer button does.
    if !h.state().setup.shown() {
        click(&mut h, "Test Setup…");
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
    click(&mut h, "Continue");
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
    click(&mut h, "Continue");
    until(&mut h, "self-test", |h| has(h, "Copy diagnostics"));
    h.get_by_label_contains("no audio came back");
    click(&mut h, "Copy diagnostics");
    h.get_by_label("Copied to clipboard");
    h.get_by_label("Retry");
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
    h.get_by_label("Open Settings");
    h.get_by_label("System Audio Recording: Denied");
    h.get_by_label_contains("System Audio Recording Only");
    assert!(!has(&h, "Allow System Audio Recording"));
    let h = setup_run(installed(), flowing());
    assert!(!has(&h, "Open Settings") && !has(&h, "Allow System Audio Recording"));
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
        click(&mut h, "Test Setup…");
        h.get_by_label("System Audio Recording: Needs permission");
        assert!(!has(&h, "Open Settings"), "only after Denied");
        click_row(&mut h, "Allow System Audio Recording");
        assert_eq!(*calls.lock().unwrap(), 1);
        h.get_by_label(&format!("System Audio Recording: {word}"));
        assert_eq!(has(&h, "Open Settings"), settings, "{answer:?}");
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
    click(&mut h, "Test Setup…");
    h.get_by_label("Microphone: Needs permission");
    assert_eq!(
        h.get_all_by_label("Allow Microphone").count(),
        2,
        "row + footer"
    );
    click_row(&mut h, "Allow Microphone");
    h.get_by_label("Microphone: Asking…");
    assert!(!has(&h, "Allow Microphone"));
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
    h.get_by_label("Open Settings");
    h.get_by_label("Continue");
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
    click(&mut h, "Test Setup…");
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
    h.get_by_label("Allow All");
    click(&mut h, "Allow All");
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
    click(&mut h, "Test Setup…");
    assert!(h.state().setup.shown(), "Test Setup… always opens it");
}

#[test]
fn the_microphone_question_is_explained_before_the_first_apply() {
    let status = EngineStatus {
        mic: TccStatus::Unknown,
        ..installed()
    };
    let mut h = with_runner(status, never_run());
    assert_eq!(h.get_all_by_label("Allow Microphone").count(), 2);
    assert!(!has(&h, "Close"), "cannot skip the explanation");
    assert!(h.state().worker.shared.lock().unwrap().meters.is_empty());
    click_footer(&mut h, "Allow Microphone");
    until(&mut h, "apply and self-test", |h| {
        let s = h.state().worker.shared.lock().unwrap();
        !s.meters.is_empty() && matches!(s.self_test, SelfTestState::Done(_))
    });
}

#[test]
fn try_demo_swaps_to_the_synthetic_engine() {
    let mut h = with_runner(no_driver(), never_run());
    click(&mut h, "Try Demo");
    until(&mut h, "demo", |h| {
        has(h, "Demo mode: synthetic audio, nothing is saved")
    });
    assert!(!has(&h, "Install Driver…"));
    until(&mut h, "demo meters", |h| {
        !h.state().worker.shared.lock().unwrap().meters.is_empty()
    });
}

#[test]
fn uninstall_asks_first_and_deletes_settings_only_when_ticked() {
    let (runner, calls) = recording(Err(SetupError::Cancelled));
    let mut h = with_runner(installed(), runner);
    click(&mut h, "Uninstall…");
    assert!(calls.lock().unwrap().is_empty());
    click(&mut h, "Cancel");
    assert!(!has(&h, "Uninstall"));
    click(&mut h, "Uninstall…");
    click(&mut h, "Uninstall");
    until(&mut h, "runner", |h| has(h, "Not uninstalled: Cancelled."));
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
        click(&mut h, "Uninstall…");
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
fn deleting_a_device_can_be_undone_from_both_delete_buttons() {
    for button in ["Delete", "Remove Stream Mix"] {
        let mut h = open(demo_config());
        click(&mut h, "Select Stream Mix");
        let before = h.state().cfg.clone();
        click(&mut h, button);
        assert_eq!(h.state().cfg.devices.len(), before.devices.len() - 1);
        h.get_by_label("Removed Stream Mix");
        click(&mut h, "Undo");
        assert_eq!(h.state().cfg, before, "{button}");
        assert_eq!(h.state().selected, 1);
        h.get_by_label("Select Stream Mix");
    }
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
fn a_failed_engine_start_shows_silence_and_a_standing_banner() {
    let mut h = window(
        demo_config(),
        Box::new(FakeBackend::silent()),
        None,
        Theme::Dark,
        false,
    );
    h.state_mut().engine_error = Some("no CoreAudio".into());
    settle(&mut h);
    let banner = "Audio engine unavailable: no CoreAudio. Routing is not running.";
    h.get_by_label(banner);
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
    h.get_by_label(banner);
    assert!(!has(&h, "Demo mode: synthetic audio, nothing is saved"));
}

#[test]
fn a_hung_self_test_can_be_cancelled_or_times_out() {
    let hang = |h: &mut Harness<App>| {
        click(h, "Continue");
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
        has(h, "The test did not finish")
    });
    assert!(h.state().now >= 30.0);
    h.get_by_label("Copy diagnostics");
    click(&mut h, "Retry");
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
