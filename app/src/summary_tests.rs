//! The device summary card through the whole window: its controls are the sidebar row's controls.
use super::tests::{click, has, open, settle, window};
use super::*;
use crate::fake::{FakeBackend, demo_config, demo_status};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};

fn with(cfg: Config, status: EngineStatus) -> Harness<'static, App> {
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    window(cfg, Box::new(b), None, Theme::Dark, false)
}

fn state_of(state: DeviceState) -> EngineStatus {
    EngineStatus {
        per_device: vec![("v1".into(), state)],
        ..demo_status()
    }
}

fn volume_of(h: &Harness<App>, label: &str) -> f64 {
    h.get_by_label(label)
        .accesskit_node()
        .numeric_value()
        .expect("a slider has a value")
}

fn is_muted(h: &Harness<App>, label: &str) -> bool {
    h.get_by_label(label).accesskit_node().toggled() == Some(egui::accesskit::Toggled::True)
}

#[test]
fn the_card_names_its_controls() {
    let h = open(demo_config());
    for label in [
        "Master volume",
        "Mute device",
        "Collapse device summary",
        "Output level, channels 1 & 2",
    ] {
        h.get_by_label(label);
    }
}

#[test]
fn the_master_fader_and_the_sidebar_slider_are_one_control() {
    let mut h = open(demo_config());
    let before = h.state().cfg.devices[0].volume_db;
    h.get_by_label("Master volume").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    let after = h.state().cfg.devices[0].volume_db;
    assert!(after < before, "{after} < {before}");
    assert_eq!(
        volume_of(&h, "Master volume"),
        volume_of(&h, "Loopback Audio volume")
    );
    // and the other way round
    h.get_by_label("Loopback Audio volume").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    assert!(h.state().cfg.devices[0].volume_db < after);
    assert_eq!(
        volume_of(&h, "Master volume"),
        volume_of(&h, "Loopback Audio volume")
    );
    // the other devices keep their level
    assert_eq!(
        h.state().cfg.devices[1].volume_db,
        demo_config().devices[1].volume_db
    );
}

#[test]
fn mute_device_and_the_sidebar_speaker_are_one_control() {
    let mut h = open(demo_config());
    assert!(!h.state().cfg.devices[0].muted);
    click(&mut h, "Mute device");
    assert!(h.state().cfg.devices[0].muted);
    assert!(is_muted(&h, "Mute device") && is_muted(&h, "Mute Loopback Audio"));
    click(&mut h, "Mute Loopback Audio");
    assert!(!h.state().cfg.devices[0].muted);
    assert!(!is_muted(&h, "Mute device") && !is_muted(&h, "Mute Loopback Audio"));
    // only the selected device
    assert!(h.state().cfg.devices.iter().all(|d| !d.muted));
}

#[test]
fn the_chip_says_what_the_engine_says() {
    for (state, text) in [
        (DeviceState::Running, "Running"),
        (DeviceState::Starting, "Starting"),
        (DeviceState::WaitingForPermission, "Waiting for permission"),
        (DeviceState::Failed("boom".into()), "Error"),
        (DeviceState::Idle, "Not connected"),
    ] {
        let h = with(demo_config(), state_of(state));
        h.get_by_label(&format!("Device state: {text}"));
    }
    let mut cfg = demo_config();
    cfg.devices[0].enabled = false;
    let h = with(cfg, state_of(DeviceState::Running));
    h.get_by_label("Device state: Off");
}

#[test]
fn an_error_is_spelled_out_on_the_second_row() {
    let h = with(
        demo_config(),
        state_of(DeviceState::Failed("the driver is not installed".into())),
    );
    h.get_by_label("Device state: Error");
    h.get_by_label("the driver is not installed");
}

#[test]
fn the_second_row_carries_the_delay_and_the_counts() {
    let h = open(demo_config());
    h.get_by_label("3 sources, 2 outputs, 1 monitor");
    // the frozen engine reports 512 frames at 48 kHz and 1384 for captured apps
    h.get_by_label("Delay 10.7 ms  Apps 29 ms");
    // a device without a reading says so instead of showing a stale number
    let h = with(demo_config(), demo_status());
    let mut h = h;
    click(&mut h, "Select Zoom Out");
    h.get_by_label("No delay reading yet");
}

#[test]
fn the_chevron_folds_the_card_into_a_strip_and_the_choice_stays_for_the_session() {
    let mut h = open(demo_config());
    let full = h.get_by_label("Mute device").rect().center().y;
    click(&mut h, "Collapse device summary");
    assert!(h.state().summary.collapsed);
    assert!(!has(&h, "3 sources, 2 outputs, 1 monitor"));
    assert!(has(&h, "Expand device summary"));
    for label in [
        "Master volume",
        "Mute device",
        "Device state: Not connected",
    ] {
        h.get_by_label(label);
    }
    assert!(
        h.get_by_label("Mute device").rect().center().y < full,
        "the strip is one line"
    );
    // another device, same state
    click(&mut h, "Select Stream Mix");
    assert!(has(&h, "Expand device summary"));
    click(&mut h, "Expand device summary");
    assert!(!h.state().summary.collapsed);
    h.get_by_label("2 sources, 2 outputs, 0 monitors");
}

#[test]
fn a_wide_device_shows_four_pairs_and_counts_the_rest() {
    let mut cfg = demo_config();
    for _ in 0..4 {
        apply(&mut cfg, Edit::GrowChannels(0));
    }
    assert_eq!(cfg.devices[0].channels, 10);
    let h = open(cfg);
    for p in 1..=4 {
        h.get_by_label(&format!("Output level, channels {} & {}", p * 2 - 1, p * 2));
    }
    assert!(!has(&h, "Output level, channels 9 & 10"));
    h.get_by_label("1 more output channel pair");
}

#[test]
fn a_dropout_dot_shows_next_to_the_chip_while_dropouts_grow() {
    let mut b = FakeBackend::frozen(1.5);
    b.status.xruns = vec![("v1".into(), 12)];
    let mut h = window(demo_config(), Box::new(b), None, Theme::Dark, false);
    assert!(!has(&h, "This device: 12 dropouts since start"));
    let now = h.state().now;
    h.state_mut().dropouts.observe(&[("v1".into(), 8)], now);
    h.run_steps(2);
    h.get_by_label("This device: 12 dropouts since start");
    // the sidebar row's dot is still its own
    h.get_by_label("Loopback Audio: 12 dropouts since start");
}

#[test]
fn a_long_device_name_leaves_the_card_intact() {
    let mut cfg = demo_config();
    cfg.devices[0].name = "A very long device name that goes on and on and on and on ok".into();
    let h = open(cfg);
    for label in [
        "Master volume",
        "Mute device",
        "Device state: Not connected",
    ] {
        h.get_by_label(label);
    }
}

/// `cargo test -p soundrouter-app --features snapshots` (`UPDATE_SNAPSHOTS=1` rewrites the baselines).
#[cfg(feature = "snapshots")]
mod snapshots {
    use super::*;

    const MAIN: egui::Vec2 = egui::vec2(1180.0, 700.0);
    /// The smallest window the app allows.
    const MIN: egui::Vec2 = egui::vec2(1000.0, 620.0);

    fn shot(
        name: &str,
        size: egui::Vec2,
        cfg: &Config,
        status: EngineStatus,
        before: impl Fn(&mut Harness<'static, App>),
        results: &mut egui_kittest::SnapshotResults,
    ) {
        for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let (status, cfg) = (status.clone(), cfg.clone());
            let mut h = Harness::builder()
                .with_size(size)
                .with_step_dt(0.05)
                .with_theme(theme)
                .with_context_setup(crate::theme::install_fonts)
                .wgpu()
                .build_eframe(move |cc| {
                    let mut b = FakeBackend::frozen(1.5);
                    // the frozen backend's delays, with the state the shot is about
                    let (delays, taps) = (b.status.delays.clone(), b.status.tap_delays.clone());
                    b.status = status;
                    b.status.delays = delays;
                    b.status.tap_delays = taps;
                    let w =
                        Worker::spawn_inline(Box::new(b), cc.egui_ctx.clone(), Box::new(|_| None));
                    let mut app = App::new(cfg, None, w, None, false, false);
                    app.setup.skip_intro(); // the real marker may not exist on this machine
                    app
                });
            super::super::tests::wait_published(&mut h);
            settle(&mut h);
            before(&mut h);
            h.run_steps(5);
            h.snapshot(format!("summary_{name}_{suffix}"));
            results.extend_harness(&mut h);
        }
    }

    #[test]
    fn running() {
        let (cfg, status) = (demo_config(), state_of(DeviceState::Running));
        let mut r = egui_kittest::SnapshotResults::new();
        shot("running", MAIN, &cfg, status.clone(), |_| {}, &mut r);
        shot("running_min", MIN, &cfg, status, |_| {}, &mut r);
    }

    #[test]
    fn muted_and_turned_down() {
        let mut cfg = demo_config();
        cfg.devices[0].muted = true;
        cfg.devices[0].volume_db = -12.0;
        shot(
            "muted",
            MAIN,
            &cfg,
            state_of(DeviceState::Running),
            |_| {},
            &mut egui_kittest::SnapshotResults::new(),
        );
    }

    #[test]
    fn not_connected() {
        shot(
            "not_connected",
            MAIN,
            &demo_config(),
            demo_status(),
            |_| {},
            &mut egui_kittest::SnapshotResults::new(),
        );
    }

    #[test]
    fn turned_off() {
        let mut r = egui_kittest::SnapshotResults::new();
        shot(
            "off",
            MAIN,
            &demo_config(),
            demo_status(),
            |h| click(h, "Select Zoom Out"),
            &mut r,
        );
    }

    #[test]
    fn error() {
        let status = state_of(DeviceState::Failed(
            "the audio driver did not answer in time, reinstall it from the permissions sheet"
                .into(),
        ));
        let mut r = egui_kittest::SnapshotResults::new();
        shot(
            "error",
            MAIN,
            &demo_config(),
            status.clone(),
            |_| {},
            &mut r,
        );
        shot("error_min", MIN, &demo_config(), status, |_| {}, &mut r);
    }

    #[test]
    fn collapsed() {
        let (cfg, status) = (demo_config(), state_of(DeviceState::Running));
        let mut r = egui_kittest::SnapshotResults::new();
        shot(
            "collapsed",
            MAIN,
            &cfg,
            status,
            |h| click(h, "Collapse device summary"),
            &mut r,
        );
    }

    #[test]
    fn waiting_for_permission_with_dropouts() {
        let mut status = state_of(DeviceState::WaitingForPermission);
        status.xruns = vec![("v1".into(), 12)];
        let mut r = egui_kittest::SnapshotResults::new();
        let grew = |h: &mut Harness<'static, App>| {
            let now = h.state().now;
            h.state_mut().dropouts.observe(&[("v1".into(), 8)], now);
        };
        shot("waiting", MIN, &demo_config(), status, grew, &mut r);
    }

    #[test]
    fn long_name_and_many_pairs() {
        let mut cfg = demo_config();
        cfg.devices[0].name = "A very long device name that goes on and on and on and on ok".into();
        for _ in 0..5 {
            apply(&mut cfg, Edit::GrowChannels(0));
        }
        let mut r = egui_kittest::SnapshotResults::new();
        shot(
            "long_name",
            MIN,
            &cfg,
            state_of(DeviceState::Running),
            |_| {},
            &mut r,
        );
    }
}
