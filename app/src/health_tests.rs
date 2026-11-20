//! The health banner, the dropout dot and the Permissions sheet through the whole window.
use super::tests::{click, has, open, settle, window};
use super::*;
use crate::fake::{FakeBackend, demo_config, demo_status};
use crate::health;
use egui_kittest::{Harness, kittest::Queryable};
use sr_core::backend::HogInfo;
use std::sync::Mutex;

const SILENT: &str = "Audio from Safari and Spotify looks silent. If you expect sound, check that SoundRouter may capture audio.";
const HOGGED: &str =
    "USB DAC is in exclusive use by Logic Pro; apps playing to it cannot be routed.";
const HFP: &str =
    "AirPods switched to a phone-call mode (low quality) because a microphone is open on it.";

/// Status of an installed (non-demo) driver, so the footer's "More actions" exists.
fn installed() -> EngineStatus {
    EngineStatus {
        driver_version: Some("1".into()),
        ..demo_status()
    }
}

fn hog() -> HogInfo {
    HogInfo {
        device_name: "USB DAC".into(),
        pid: Some(41),
        process_name: Some("Logic Pro".into()),
    }
}

fn with(status: EngineStatus) -> Harness<'static, App> {
    let mut b = FakeBackend::frozen(1.5);
    b.status = status;
    window(demo_config(), Box::new(b), None, Theme::Dark, false)
}

/// The stand-in for the machine: Start at login lives in a cell and every opened link is recorded.
struct Machine {
    login: Arc<Mutex<bool>>,
    opened: Arc<Mutex<Vec<String>>>,
}

fn machine(h: &mut Harness<App>, login_on: bool) -> Machine {
    let m = Machine {
        login: Arc::new(Mutex::new(login_on)),
        opened: Arc::default(),
    };
    let (read, write, opened) = (m.login.clone(), m.login.clone(), m.opened.clone());
    h.state_mut().os = Os {
        login_is_on: Arc::new(move || *read.lock().unwrap()),
        set_login: Arc::new(move |on| {
            *write.lock().unwrap() = on;
            Ok(())
        }),
        open_url: Arc::new(move |u| opened.lock().unwrap().push(u.to_owned())),
    };
    m
}

#[test]
fn a_silent_tap_names_the_apps_and_offers_the_privacy_pane() {
    let mut h = with(EngineStatus {
        tap_silent: vec!["v1".into()],
        ..demo_status()
    });
    h.get_by_label(SILENT);
    let m = machine(&mut h, false);
    click(&mut h, health::PRIVACY);
    assert_eq!(*m.opened.lock().unwrap(), [setup::AUDIO_CAPTURE_SETTINGS]);
    assert!(has(&h, health::PRIVACY), "opening a pane does not dismiss");
}

#[test]
fn only_one_banner_shows_and_closing_it_brings_up_the_next_with_a_count() {
    let mut h = with(EngineStatus {
        tap_silent: vec!["v1".into()],
        hogged: vec![hog()],
        bluetooth_hfp: vec!["AirPods".into()],
        ..demo_status()
    });
    assert!(h.query_by_label_contains("looks silent").is_some());
    assert!(!has(&h, HOGGED) && !has(&h, HFP));
    h.get_by_label("2 more notices");
    click(&mut h, "Dismiss notice");
    h.get_by_label(HOGGED);
    assert!(h.query_by_label_contains("looks silent").is_none());
    assert!(
        !has(&h, health::PRIVACY),
        "a hog has no settings pane to open"
    );
    h.get_by_label("1 more notice");
    click(&mut h, "Dismiss notice");
    h.get_by_label(HFP);
    assert!(h.query_by_label_contains("more notice").is_none());
    click(&mut h, "Dismiss notice");
    assert!(!has(&h, HFP) && !has(&h, "Dismiss notice"));
}

#[test]
fn a_banner_never_blocks_the_window() {
    let mut h = with(EngineStatus {
        hogged: vec![hog()],
        ..demo_status()
    });
    h.get_by_label(HOGGED);
    click(&mut h, NEW_DEVICE);
    assert_eq!(
        h.state().cfg.devices.len(),
        4,
        "the banner left the window usable"
    );
    h.get_by_label(HOGGED);
}

#[test]
fn a_denied_capture_is_one_banner_instead_of_one_per_silent_tap() {
    let h = with(EngineStatus {
        tcc: TccStatus::Denied,
        tap_silent: vec!["v1".into()],
        ..demo_status()
    });
    h.get_by_label_contains("System Audio Recording is off for SoundRouter");
    assert!(h.query_by_label_contains("looks silent").is_none());
    h.get_by_label(health::PRIVACY);
}

#[test]
fn a_healthy_window_and_the_demo_show_no_banner() {
    let h = open(demo_config());
    assert!(!has(&h, "Dismiss notice"));
    let mut b = FakeBackend::frozen(1.5);
    b.status.hogged = vec![hog()];
    let h = window(demo_config(), Box::new(b), None, Theme::Dark, true);
    assert!(!has(&h, HOGGED), "demo audio is synthetic");
}

/// A frame later the engine reports `n` dropouts for device v1, after it reported fewer.
fn dropouts_grew_to(h: &mut Harness<App>, n: u64) {
    let now = h.state().now;
    h.state_mut().dropouts.observe(&[("v1".into(), n - 4)], now);
    h.run_steps(2);
}

#[test]
fn two_devices_with_dropouts_each_show_their_own_dot() {
    let cfg = demo_config();
    let (a, b) = (&cfg.devices[0], &cfg.devices[1]);
    let mut backend = FakeBackend::frozen(1.5);
    backend.status.xruns = vec![(a.id.clone(), 3), (b.id.clone(), 5)];
    let mut h = window(cfg.clone(), Box::new(backend), None, Theme::Dark, false);
    let now = h.state().now;
    // The engine's counters then grew past these: both rows draw a dot in the same frame, and with one
    // shared widget id the second would replace the first.
    h.state_mut()
        .dropouts
        .observe(&[(a.id.clone(), 1), (b.id.clone(), 2)], now);
    h.run_steps(2);
    h.get_by_label(&format!("{}: 3 dropouts since start", a.name));
    h.get_by_label(&format!("{}: 5 dropouts since start", b.name));
}

#[test]
fn a_device_row_shows_an_amber_dot_only_while_dropouts_grow() {
    let mut b = FakeBackend::frozen(1.5);
    b.status.xruns = vec![("v1".into(), 12)];
    let mut h = window(demo_config(), Box::new(b), None, Theme::Dark, false);
    assert!(
        h.query_by_label_contains("dropouts since start").is_none(),
        "dropouts from before the window looked are not news"
    );
    dropouts_grew_to(&mut h, 12);
    h.get_by_label("Loopback Audio: 12 dropouts since start");
    for _ in 0..200 {
        h.step(); // 10 s at 0.05 s a frame
    }
    assert!(h.query_by_label_contains("dropouts since start").is_none());
    assert_eq!(
        h.state().dropouts.total(),
        12,
        "the total stays for the menu"
    );
}

#[test]
fn the_delay_menu_reports_audio_health_and_advises_a_larger_delay_while_it_grows() {
    let mut b = FakeBackend::frozen(1.5);
    b.status.xruns = vec![("v1".into(), 12)];
    let mut h = window(demo_config(), Box::new(b), None, Theme::Dark, false);
    click(&mut h, "Audio delay: Balanced");
    h.get_by_label("Audio health: 12 dropouts since start");
    assert!(
        !has(&h, "Dropouts: choose a larger delay"),
        "nothing grew yet"
    );
    click(&mut h, "Audio delay: Balanced"); // close
    dropouts_grew_to(&mut h, 12);
    click(&mut h, "Audio delay: Balanced");
    h.get_by_label("Dropouts: choose a larger delay");
}

#[test]
fn a_clean_engine_says_so_in_the_delay_menu() {
    let mut h = open(demo_config());
    click(&mut h, "Audio delay: Balanced");
    h.get_by_label("Audio health: no dropouts since start");
}

fn permissions(status: EngineStatus, login_on: bool) -> (Harness<'static, App>, Machine) {
    let mut h = with(status);
    let m = machine(&mut h, login_on);
    // A permission macOS refused opens the setup sheet by itself; the footer menu is behind it.
    if h.state().setup.shown() {
        h.state_mut().setup.open_access();
        settle(&mut h);
    } else {
        click(&mut h, "More actions");
        click(&mut h, "Permissions");
    }
    (h, m)
}

#[test]
fn the_permissions_sheet_lists_every_requirement_with_its_state() {
    let (mut h, _) = permissions(installed(), true);
    h.get_by_label("System Audio Recording: Allowed");
    h.get_by_label("Microphone: Allowed");
    h.get_by_label("Audio driver: Installed");
    h.get_by_label("Start at login: On");
    h.get_by_label("Turn off");
    click(&mut h, "Done");
    assert!(!has(&h, "Start at login: On"));
    assert!(
        !h.state().setup.seen(),
        "looking at permissions is not finishing setup"
    );
}

#[test]
fn the_permissions_sheet_asks_for_what_is_missing_and_turns_start_at_login_on() {
    let status = EngineStatus {
        tcc: TccStatus::Denied,
        ..installed()
    };
    let (mut h, m) = permissions(status, false);
    h.set_size(vec2(1180.0, 900.0)); // the denied row's long advice would scroll the last row away
    settle(&mut h);
    h.get_by_label("System Audio Recording: Denied");
    h.get_by_label("Start at login: Off");
    click(&mut h, "Turn on");
    assert!(*m.login.lock().unwrap());
    h.get_by_label("Start at login: On");
    click(&mut h, setup::OPEN_SETTINGS);
}

#[test]
fn start_at_login_that_cannot_change_says_why() {
    let (mut h, _) = permissions(installed(), false);
    h.state_mut().os.set_login = Arc::new(|_| Err(std::io::ErrorKind::PermissionDenied.into()));
    click(&mut h, "Turn on");
    h.get_by_label_contains("Start at Login did not change");
}

#[test]
fn a_settled_window_with_a_banner_and_a_dot_asks_for_no_more_frames() {
    let mut h = with(EngineStatus {
        hogged: vec![hog()],
        xruns: vec![("v1".into(), 8)],
        ..demo_status()
    });
    dropouts_grew_to(&mut h, 8);
    h.run_steps(60);
    assert!(
        !h.ctx.has_requested_repaint(),
        "{:?}",
        h.ctx.repaint_causes()
    );
}

#[test]
fn the_banner_holds_up_at_the_minimum_window_width() {
    let mut h = Harness::builder()
        .with_size(vec2(1000.0, 620.0))
        .with_step_dt(0.05)
        .with_context_setup(crate::theme::install_fonts)
        .build_eframe(|cc| {
            let mut b = FakeBackend::frozen(1.5);
            b.status = EngineStatus {
                tap_silent: vec!["v1".into()],
                hogged: vec![hog()],
                ..installed()
            };
            let w = Worker::spawn_inline(Box::new(b), cc.egui_ctx.clone(), Box::new(|_| None));
            App::new(demo_config(), None, w, None, false, false)
        });
    settle(&mut h);
    let text = h.query_by_label_contains("looks silent").unwrap().rect();
    let fix = h.get_by_label(health::PRIVACY).rect();
    let close = h.get_by_label("Dismiss notice").rect();
    assert!(
        text.right() <= fix.left(),
        "the text wraps before the button"
    );
    assert!(fix.right() <= close.left());
    assert!(
        close.right() <= 1000.0 && text.left() >= 250.0,
        "inside the main pane"
    );
}

/// `cargo test -p soundrouter-app --features snapshots` (`UPDATE_SNAPSHOTS=1` rewrites the baselines).
#[cfg(feature = "snapshots")]
mod snapshots {
    use super::*;

    fn shot(
        name: &str,
        size: egui::Vec2,
        status: EngineStatus,
        before: impl Fn(&mut Harness<'static, App>),
    ) {
        let mut results = egui_kittest::SnapshotResults::new();
        for (theme, suffix) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            let status = status.clone();
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
                            .with_driver_check(|_| Ok(()));
                    app
                });
            super::super::tests::wait_published(&mut h);
            settle(&mut h);
            machine(&mut h, false);
            before(&mut h);
            h.run_steps(5);
            h.snapshot(format!("health_{name}_{suffix}"));
            results.extend_harness(&mut h);
        }
    }

    const MAIN: egui::Vec2 = egui::vec2(1180.0, 700.0);
    /// The smallest window the app allows.
    const MIN: egui::Vec2 = egui::vec2(1000.0, 620.0);

    fn all_three() -> EngineStatus {
        EngineStatus {
            tap_silent: vec!["v1".into()],
            hogged: vec![hog()],
            bluetooth_hfp: vec!["AirPods".into()],
            xruns: vec![("v1".into(), 12)],
            ..installed()
        }
    }

    #[test]
    fn banner_silent_capture_with_more_waiting() {
        shot("silent", MAIN, all_three(), |_| {});
    }

    #[test]
    fn banner_silent_capture_at_the_minimum_width() {
        shot("silent_min", MIN, all_three(), |_| {});
    }

    #[test]
    fn banner_hogged_device() {
        let status = EngineStatus {
            hogged: vec![hog()],
            ..installed()
        };
        shot("hogged", MAIN, status, |_| {});
    }

    #[test]
    fn banner_bluetooth_phone_call_mode_at_the_minimum_width() {
        let status = EngineStatus {
            bluetooth_hfp: vec!["AirPods".into()],
            ..installed()
        };
        shot("hfp_min", MIN, status, |_| {});
    }

    #[test]
    fn banner_capture_denied() {
        let status = EngineStatus {
            tcc: TccStatus::Denied,
            ..installed()
        };
        shot("denied", MIN, status, |h| {
            click(h, "Close"); // the setup sheet opens by itself for a refused permission
        });
    }

    #[test]
    fn dropout_dot_on_a_device_row() {
        let status = EngineStatus {
            xruns: vec![("v1".into(), 12)],
            ..installed()
        };
        shot("dropouts", MAIN, status, |h| dropouts_grew_to(h, 12));
    }

    #[test]
    fn delay_menu_with_growing_dropouts() {
        let status = EngineStatus {
            xruns: vec![("v1".into(), 12)],
            ..installed()
        };
        shot("delay_menu", MAIN, status, |h| {
            dropouts_grew_to(h, 12);
            click(h, "Audio delay: Balanced");
        });
    }

    #[test]
    fn permissions_sheet_all_set() {
        shot("permissions", MAIN, installed(), |h| {
            machine(h, true);
            click(h, "More actions");
            click(h, "Permissions");
        });
    }

    #[test]
    fn permissions_sheet_needs_attention() {
        let status = EngineStatus {
            mic: TccStatus::Unknown,
            tcc: TccStatus::Denied,
            ..installed()
        };
        shot("permissions_todo", egui::vec2(1180.0, 900.0), status, |h| {
            h.state_mut().setup.open_access();
            settle(h);
        });
    }
}
