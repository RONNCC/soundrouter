//! Whole-window tests: the real `App` and worker driven through the accessibility tree, so every
//! control is found by the name a screen reader would announce, then clicked/dragged/typed at.
use super::*;
use crate::fake::{FakeBackend, demo_config};
use crate::model::default_config;
use crate::worker::Snapshot;
use egui_kittest::{Harness, kittest::Queryable};
use sr_core::backend::{AppInfo, Backend, HwDevice, MeterReading};
use sr_core::config::SourceKind;

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
}

/// Block until the worker thread has published what the window needs, without timers: its first
/// refresh (hardware/apps) and either meters for every enabled device or the apply error.
fn wait_published(app: &App) {
    let ready = |s: &Snapshot| {
        !s.hardware.is_empty()
            && (s.apply_error.is_some()
                || app
                    .cfg
                    .devices
                    .iter()
                    .filter(|d| d.enabled)
                    .all(|d| s.meters.contains_key(&d.id)))
    };
    for _ in 0..50_000_000u32 {
        if ready(&app.worker.shared.lock().unwrap()) {
            return;
        }
        std::thread::yield_now();
    }
    panic!("worker never published");
}

pub(super) fn window(
    cfg: Config,
    backend: Box<dyn Backend + Send>,
    toast: Option<&str>,
    theme: Theme,
    demo: bool,
) -> Harness<'static, App> {
    let toast = toast.map(str::to_string);
    let mut h = Harness::builder()
        .with_size(vec2(1180.0, 700.0))
        .with_step_dt(0.05) // double-click window (0.3 s) spans six frames
        .with_theme(theme)
        .with_context_setup(crate::theme::install_fonts)
        .build_eframe(move |cc| {
            let w = Worker::spawn(backend, cc.egui_ctx.clone());
            App::new(cfg, None, w, toast, false, demo)
        });
    wait_published(h.state());
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
    let mut cfg = default_config();
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
        "Delete selected device",
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
    click(&mut h, "Delete selected device");
    click(&mut h, "Delete selected device");
    assert!(h.state().cfg.devices.is_empty());
    assert_eq!(h.state().selected, 0);
    assert!(h.query_by_label("Device name").is_none());
    // nothing left to delete: both buttons are inert instead of panicking
    click(&mut h, "Delete");
    click(&mut h, "Delete selected device");
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
    // a missing driver gets an actionable message instead of the raw error
    let h = window(
        demo_config(),
        Box::new(Failing(FakeBackend::frozen(1.5), "driver not installed")),
        None,
        Theme::Dark,
        false,
    );
    h.get_by_label_contains("Driver not installed. Run");
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
                let w = Worker::spawn(backend, cc.egui_ctx.clone());
                App::new(cfg, None, w, toast.map(str::to_string), false, false)
            });
        wait_published(h.state());
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
    fn error_status() {
        both(
            "error",
            demo_config,
            Some("Could not save config: Permission denied (os error 13)"),
            |_| {},
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
                    let w = Worker::spawn(backend, cc.egui_ctx.clone());
                    App::new(demo_config(), None, w, None, false, true)
                });
            wait_published(h.state());
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
