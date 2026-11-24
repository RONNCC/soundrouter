//! Pointer reliability: every control takes a click that lands anywhere in its target (not only on
//! the drawn glyph), neighbours never steal each other's clicks, and a careless hand (drift, a slow
//! press) still clicks. Targets are read from the accessibility tree, which reports each widget's
//! interaction rect.
use super::tests::{open, settle};
use super::*;
use crate::fake::demo_config;
use crate::widgets::MIN_HIT;
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};

fn rect(h: &Harness<App>, label: &str) -> Rect {
    h.get_by_label(label).rect()
}

/// Press and release at `at` without moving.
fn tap(h: &mut Harness<App>, at: egui::Pos2) {
    h.hover_at(at);
    h.step();
    h.drag_at(at);
    h.step();
    h.drop_at(at);
    settle(h);
}

/// The four corners of `r`, pulled 1 px inside.
fn corners(r: Rect) -> [egui::Pos2; 4] {
    let r = r.shrink(1.0);
    [
        r.left_top(),
        r.right_top(),
        r.left_bottom(),
        r.right_bottom(),
    ]
}

fn enabled(h: &Harness<App>, dev: usize) -> bool {
    h.state().cfg.devices[dev].enabled
}

fn volume(h: &Harness<App>, label: &str) -> f64 {
    h.get_by_label(label)
        .accesskit_node()
        .numeric_value()
        .expect("a slider has a value")
}

#[test]
fn the_sidebar_switch_takes_a_click_at_every_corner_of_its_target() {
    let mut h = open(demo_config());
    let r = rect(&h, "Enable Loopback Audio");
    assert!(r.width() >= 44.0 && r.height() >= MIN_HIT, "{r:?}");
    for (n, at) in corners(r).into_iter().enumerate() {
        tap(&mut h, at);
        assert_eq!(enabled(&h, 0), n % 2 == 1, "corner {n} at {at:?}");
    }
}

#[test]
fn the_sidebar_mute_button_takes_a_click_at_every_corner_of_its_target() {
    let mut h = open(demo_config());
    let r = rect(&h, "Mute Loopback Audio");
    assert!(r.width() >= MIN_HIT && r.height() >= MIN_HIT, "{r:?}");
    for (n, at) in corners(r).into_iter().enumerate() {
        tap(&mut h, at);
        assert_eq!(h.state().cfg.devices[0].muted, n % 2 == 0, "corner {n}");
    }
}

#[test]
fn the_sidebar_x_takes_a_click_at_every_corner_of_its_target() {
    for n in 0..4 {
        let mut h = open(demo_config());
        let r = rect(&h, "Remove Loopback Audio");
        assert!(r.width() >= MIN_HIT && r.height() >= MIN_HIT, "{r:?}");
        tap(&mut h, corners(r)[n]);
        assert_eq!(h.state().cfg.devices.len(), 2, "corner {n}");
        assert_eq!(h.state().cfg.devices[0].name, "Stream Mix");
    }
}

#[test]
fn the_x_and_the_switch_four_pixels_apart_do_not_steal_each_others_clicks() {
    let (switch, x) = {
        let h = open(demo_config());
        (
            rect(&h, "Enable Loopback Audio"),
            rect(&h, "Remove Loopback Audio"),
        )
    };
    assert_eq!(x.left() - switch.right(), 4.0, "{switch:?} {x:?}");

    // The switch's last pixel toggles it and removes nothing.
    let mut h = open(demo_config());
    tap(&mut h, egui::pos2(switch.right() - 0.5, switch.center().y));
    assert!(!enabled(&h, 0));
    assert_eq!(h.state().cfg.devices.len(), 3);

    // The X's first pixel removes the device and does not touch the neighbours' switches.
    let mut h = open(demo_config());
    tap(&mut h, egui::pos2(x.left() + 0.5, x.center().y));
    assert_eq!(h.state().cfg.devices.len(), 2);
    assert!(enabled(&h, 0), "Stream Mix keeps its switch");

    // Between them one of the two answers, never both.
    let mut h = open(demo_config());
    tap(&mut h, egui::pos2(switch.right() + 2.0, switch.center().y));
    let toggled = !enabled(&h, 0);
    let removed = h.state().cfg.devices.len() == 2;
    assert!(!(toggled && removed), "one click did both");
}

#[test]
fn a_click_on_a_device_name_selects_the_row() {
    let mut h = open(demo_config());
    assert_eq!(h.state().selected, 0);
    let at = rect(&h, "Rename Stream Mix").center();
    tap(&mut h, at);
    assert_eq!(h.state().selected, 1);
}

#[test]
fn the_row_selects_on_press_not_on_release() {
    let mut h = open(demo_config());
    let r = rect(&h, "Select Stream Mix");
    let at = egui::pos2(r.left() + 1.0, r.bottom() - 1.0);
    h.hover_at(at);
    h.step();
    h.drag_at(at);
    h.step();
    assert_eq!(h.state().selected, 1, "still held down");
    h.drop_at(at);
    settle(&mut h);
    assert_eq!(h.state().selected, 1);
}

#[test]
fn a_press_on_a_control_does_not_select_its_row() {
    let mut h = open(demo_config());
    let at = rect(&h, "Mute Stream Mix").center();
    tap(&mut h, at);
    assert_eq!(h.state().selected, 0, "the mute button took the press");
    assert!(h.state().cfg.devices[1].muted);
}

#[test]
fn a_switched_off_device_is_fully_interactive() {
    let mut h = open(demo_config());
    assert!(!enabled(&h, 2));
    let at = rect(&h, "Select Zoom Out").left_bottom() + vec2(1.0, -1.0);
    tap(&mut h, at);
    assert_eq!(h.state().selected, 2);
    let at = rect(&h, "Mute Zoom Out").center();
    tap(&mut h, at);
    assert!(h.state().cfg.devices[2].muted);
    let r = rect(&h, "Zoom Out volume");
    let before = h.state().cfg.devices[2].volume_db;
    tap(
        &mut h,
        egui::pos2(r.left() + r.width() * 0.25, r.center().y),
    );
    assert_ne!(h.state().cfg.devices[2].volume_db, before);
    let at = rect(&h, "Enable Zoom Out").center();
    tap(&mut h, at);
    assert!(enabled(&h, 2));
}

#[test]
fn a_press_drag_release_of_eight_pixels_is_still_a_click() {
    let mut h = open(demo_config());
    let r = rect(&h, "Enable Loopback Audio");
    let from = r.center() - vec2(4.0, 0.0);
    let to = r.center() + vec2(4.0, 0.0);
    h.hover_at(from);
    h.step();
    h.drag_at(from);
    h.step();
    h.hover_at(to);
    h.step();
    h.drop_at(to);
    settle(&mut h);
    assert!(!enabled(&h, 0), "an 8 px drift between press and release");
}

#[test]
fn a_slow_press_is_still_a_click() {
    let mut h = open(demo_config());
    let at = rect(&h, "Mute Loopback Audio").center();
    h.hover_at(at);
    h.step();
    h.drag_at(at);
    h.run_steps(22); // 1.1 s at 0.05 s a step: past egui's 0.8 s default
    h.drop_at(at);
    settle(&mut h);
    assert!(h.state().cfg.devices[0].muted);
}

#[test]
fn the_click_slop_is_set_for_the_whole_app() {
    let h = open(demo_config());
    let (dist, secs) = h.ctx.options(|o| {
        (
            o.input_options.max_click_dist,
            o.input_options.max_click_duration,
        )
    });
    assert_eq!(
        (dist, secs),
        (crate::theme::CLICK_SLOP, crate::theme::CLICK_SECS)
    );
    assert!(dist >= 10.0 && secs >= 1.2);
}

#[test]
fn the_slider_track_is_hit_ten_pixels_above_and_below_its_line() {
    for dy in [-10.0, 10.0, -13.0, 13.0] {
        let mut h = open(demo_config());
        let r = rect(&h, "Stream Mix volume");
        assert!(r.height() >= MIN_HIT, "{r:?}");
        // Stream Mix starts at 100 %: a click a quarter of the way in lowers it there.
        tap(
            &mut h,
            egui::pos2(r.left() + r.width() * 0.25, r.center().y + dy),
        );
        let v = volume(&h, "Stream Mix volume");
        let want = (r.width() * 0.25 - 10.5) / (r.width() - 21.0) * 100.0;
        assert!((v as f32 - want).abs() < 1.5, "dy {dy}: {v} vs {want}");
    }
}

#[test]
fn dragging_the_slider_follows_the_pointer_and_the_edges_clamp() {
    let mut h = open(demo_config());
    let r = rect(&h, "Loopback Audio volume");
    let y = r.center().y;
    h.hover_at(egui::pos2(r.right() - 11.0, y));
    h.step();
    h.drag_at(egui::pos2(r.right() - 11.0, y));
    h.step();
    h.hover_at(egui::pos2(r.center().x, y));
    h.step();
    h.hover_at(egui::pos2(r.left() - 30.0, y + 9.0)); // past the end and off the strip
    h.step();
    h.drop_at(egui::pos2(r.left() - 30.0, y + 9.0));
    settle(&mut h);
    assert_eq!(volume(&h, "Loopback Audio volume"), 0.0);
}

#[test]
fn grabbing_the_knob_by_its_edge_does_not_move_it() {
    let mut h = open(demo_config());
    let r = rect(&h, "Loopback Audio volume");
    let before = volume(&h, "Loopback Audio volume");
    // 100 % puts the knob at the right end; press 6 px left of its centre and let go.
    let at = egui::pos2(r.right() - 10.5 - 6.0, r.center().y);
    tap(&mut h, at);
    assert_eq!(volume(&h, "Loopback Audio volume"), before);
}

#[test]
fn double_click_and_option_click_reset_a_slider_to_100() {
    let mut h = open(demo_config());
    let r = rect(&h, "Loopback Audio volume");
    let at = egui::pos2(r.left() + r.width() * 0.3, r.center().y);
    tap(&mut h, at);
    assert!(volume(&h, "Loopback Audio volume") < 50.0);
    tap(&mut h, at);
    tap(&mut h, at);
    // two quick taps are a double-click
    h.hover_at(at);
    h.step();
    for _ in 0..2 {
        h.drag_at(at);
        h.step();
        h.drop_at(at);
        h.step();
        h.hover_at(at);
    }
    settle(&mut h);
    assert_eq!(volume(&h, "Loopback Audio volume"), 100.0);

    tap(&mut h, at);
    assert!(volume(&h, "Loopback Audio volume") < 50.0);
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::ALT));
    tap(&mut h, at);
    h.event(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
    settle(&mut h);
    assert_eq!(volume(&h, "Loopback Audio volume"), 100.0);
}

#[test]
fn the_arrow_keys_still_move_a_focused_slider() {
    let mut h = open(demo_config());
    h.get_by_label("Loopback Audio volume").focus();
    settle(&mut h);
    h.key_press(egui::Key::ArrowLeft);
    settle(&mut h);
    assert!((volume(&h, "Loopback Audio volume") - 99.0).abs() < 0.01);
    h.key_press(egui::Key::ArrowRight);
    settle(&mut h);
    assert_eq!(volume(&h, "Loopback Audio volume"), 100.0);
}

#[test]
fn a_card_switch_toggles_from_its_on_label_and_its_x_from_its_corners() {
    let mut h = open(demo_config());
    let sw = rect(&h, "Safari enabled");
    // The "On" word sits at the left end of the target.
    tap(&mut h, egui::pos2(sw.left() + 3.0, sw.center().y));
    assert!(h.state().cfg.devices[0].sources[0].muted);
    tap(&mut h, egui::pos2(sw.right() - 1.0, sw.center().y));
    assert!(!h.state().cfg.devices[0].sources[0].muted);

    let x = rect(&h, "Remove Safari");
    assert!(x.width() >= MIN_HIT && x.height() >= MIN_HIT, "{x:?}");
    assert!(x.left() - sw.right() >= 4.0, "{sw:?} {x:?}");
    tap(&mut h, corners(x)[3]);
    assert_eq!(h.state().cfg.devices[0].sources.len(), 2);
}

#[test]
fn the_card_options_slider_and_remove_button_take_edge_clicks() {
    let mut h = open(demo_config());
    let at = rect(&h, "Safari options").center();
    tap(&mut h, at);
    let v = rect(&h, "Safari volume");
    assert!(v.height() >= MIN_HIT, "{v:?}");
    tap(
        &mut h,
        egui::pos2(v.left() + v.width() * 0.5, v.center().y - 12.0),
    );
    assert!(h.state().cfg.devices[0].sources[0].volume_db < 0.0);
    let rm = rect(&h, "Remove Source Safari");
    assert!(rm.height() >= MIN_HIT, "{rm:?}");
    assert!(
        rm.top() >= v.bottom() - 0.5,
        "target overlaps the slider: {rm:?} {v:?}"
    );
    tap(&mut h, corners(rm)[2]);
    assert_eq!(h.state().cfg.devices[0].sources.len(), 2);
}

#[test]
fn the_footer_and_header_buttons_are_at_least_a_target_tall() {
    let h = open(demo_config());
    for label in [NEW_DEVICE, "Presets", "Audio delay: Balanced"] {
        let r = rect(&h, label);
        assert!(r.height() >= MIN_HIT, "{label}: {r:?}");
    }
}

#[test]
fn the_pointer_says_what_can_be_done() {
    let mut h = open(demo_config());
    let cursor = |h: &Harness<App>| h.output().platform_output.cursor_icon;
    for (label, want) in [
        ("Remove Loopback Audio", egui::CursorIcon::PointingHand),
        ("Enable Loopback Audio", egui::CursorIcon::PointingHand),
        ("Mute Loopback Audio", egui::CursorIcon::PointingHand),
        ("Loopback Audio volume", egui::CursorIcon::ResizeHorizontal),
    ] {
        h.hover_at(rect(&h, label).center());
        h.run_steps(3);
        assert_eq!(cursor(&h), want, "{label}");
    }
}

/// Roles a pointer lands on.
const TARGET_ROLES: [Role; 5] = [
    Role::Button,
    Role::CheckBox,
    Role::RadioButton,
    Role::Slider,
    Role::Switch,
];

/// Every pressable node of the window that is smaller than the macOS 24 pt pointer minimum.
fn undersized(h: &Harness<App>) -> Vec<String> {
    let mut bad = Vec::new();
    for role in TARGET_ROLES {
        for n in h.query_all_by_role(role) {
            let r = n.rect();
            if r.width() < 24.0 || r.height() < 24.0 {
                bad.push(format!(
                    "{role:?} {:?} {:.0}x{:.0}",
                    n.accesskit_node().label().unwrap_or_default(),
                    r.width(),
                    r.height()
                ));
            }
        }
    }
    bad
}

fn audit(what: &str, h: &Harness<App>) {
    let bad = undersized(h);
    assert!(
        bad.is_empty(),
        "{what}: targets under 24x24:\n{}",
        bad.join("\n")
    );
}

#[test]
fn no_button_switch_or_slider_in_the_main_window_is_smaller_than_24_points() {
    let mut h = open(demo_config());
    assert!(
        TARGET_ROLES
            .iter()
            .any(|r| h.query_all_by_role(*r).count() > 0),
        "the audit found no controls at all"
    );
    audit("main window", &h);
    click_label(&mut h, "Safari options");
    audit("an expanded card", &h);
    click_label(&mut h, "Add source");
    audit("the source menu", &h);
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    click_label(&mut h, "Audio delay: Balanced");
    audit("the delay menu", &h);
    h.key_press(egui::Key::Escape);
    settle(&mut h);
    click_label(&mut h, "Presets");
    audit("the presets menu", &h);
}

#[test]
fn no_control_of_the_empty_window_or_a_sheet_is_smaller_than_24_points() {
    let mut h = open(demo_config());
    for name in ["Loopback Audio", "Stream Mix", "Zoom Out"] {
        click_label(&mut h, &format!("Remove {name}"));
    }
    h.get_by_label("Create your first device");
    audit("no devices", &h);

    let mut b = crate::fake::FakeBackend::frozen(1.5);
    b.status.driver_version = None;
    let h = super::tests::window(demo_config(), Box::new(b), None, Theme::Dark, false);
    assert!(
        h.query_all_by_role(Role::Button).count() > 3,
        "the setup sheet is open"
    );
    audit("the setup sheet", &h);
}

#[test]
fn no_control_in_the_narrow_sidebar_is_smaller_than_24_points() {
    let mut h = open(demo_config());
    h.set_size(vec2(250.0, 520.0));
    settle(&mut h);
    audit("the sidebar", &h);
}

#[test]
fn nothing_clips_at_the_minimum_window() {
    let mut h = open(demo_config());
    h.set_size(vec2(1000.0, 520.0));
    settle(&mut h);
    audit("1000 px window", &h);
    let window = Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 520.0));
    let mut out = Vec::new();
    for role in TARGET_ROLES {
        for n in h.query_all_by_role(role) {
            let r = n.rect();
            // The sidebar list scrolls; everything else must be on screen.
            if r.left() < -0.5 || r.right() > window.right() + 0.5 {
                out.push(format!("{:?} {r:?}", n.accesskit_node().label()));
            }
        }
    }
    assert!(
        out.is_empty(),
        "outside the window sideways:\n{}",
        out.join("\n")
    );
}

fn click_label(h: &mut Harness<App>, label: &str) {
    super::tests::click(h, label);
}

const CHIP: &str = "Cable trim, Safari, channel 1 (L) to Channel 1 (L)";
const UNPLUG: &str = "Unplug cable, Safari, channel 1 (L) to Channel 1 (L)";

/// A window of `size` showing Safari's first cable trimmed by `db` (0 leaves it plain).
fn with_trim(size: egui::Vec2, db: f32) -> Harness<'static, App> {
    let mut cfg = demo_config();
    if db != 0.0 {
        let l = crate::model::Link {
            from: crate::model::Port::Src { src: 0, ch: 0 },
            to: crate::model::Port::OutIn { ch: 0 },
        };
        crate::model::apply(&mut cfg, crate::model::Edit::WireGain(0, l, db));
    }
    let mut h = open(cfg);
    h.set_size(size);
    settle(&mut h);
    h
}

/// The part of a chip's target a click must land in: `MIN_HIT` wide, however narrow the chip is
/// (a wide chip's ends may run under a port), and 20 tall, clear of the 4 px two straight
/// neighbours 24 apart share.
fn core(r: Rect) -> Rect {
    Rect::from_center_size(r.center(), vec2(MIN_HIT, 20.0))
}

#[test]
fn a_cable_chip_takes_a_click_at_every_corner_of_its_target_in_both_window_sizes() {
    for size in [vec2(1180.0, 700.0), vec2(1000.0, 520.0)] {
        for db in [0.0, -6.0, -12.5] {
            for n in 0..4 {
                let mut h = with_trim(size, db);
                let r = h.get_by_label_contains(CHIP).rect();
                assert!(r.width() >= MIN_HIT && r.height() >= MIN_HIT, "{r:?}");
                tap(&mut h, corners(core(r))[n]);
                h.get_by_label("Cable gain, dB");
                // This cable's popover, not a neighbour's.
                h.get_by_label("Safari, channel 1 (L) to Channel 1 (L)");
                assert_eq!(
                    h.state().cfg.devices[0].wires.len(),
                    6,
                    "{size:?} {db} corner {n}: a chip click trims, never unplugs"
                );
            }
        }
    }
}

#[test]
fn the_x_over_a_cable_takes_a_click_at_every_corner_and_never_overlaps_the_chip() {
    for size in [vec2(1180.0, 700.0), vec2(1000.0, 520.0)] {
        for n in 0..4 {
            let mut h = with_trim(size, -6.0);
            let chip = h.get_by_label_contains(CHIP).rect();
            h.hover_at(chip.center());
            settle(&mut h);
            let x = rect(&h, UNPLUG);
            assert!(x.width() >= MIN_HIT && x.height() >= MIN_HIT, "{x:?}");
            assert!(x.bottom() <= chip.top() + 0.5, "x {x:?} over chip {chip:?}");
            // The pointer travels from the chip up to the x without the x going away.
            for step in 1..=4 {
                h.hover_at(chip.center() + (x.center() - chip.center()) * (step as f32 / 4.0));
                h.step();
            }
            tap(&mut h, corners(x)[n]);
            assert_eq!(
                h.state().cfg.devices[0].wires.len(),
                5,
                "{size:?} corner {n}"
            );
        }
    }
}

#[test]
fn the_popover_controls_are_at_least_24_points() {
    for (db, who) in [(0.0, "plain"), (-6.0, "trimmed")] {
        let mut h = with_trim(vec2(1180.0, 700.0), db);
        h.get_by_label_contains(CHIP).click();
        settle(&mut h);
        h.get_by_label("Cable gain, dB");
        h.get_by_label("Balance cables");
        audit(&format!("a {who} cable's popover"), &h);
        let slider = rect(&h, "Cable gain, dB");
        assert!(
            slider.width() > 80.0 && slider.height() >= MIN_HIT,
            "{slider:?}"
        );
        assert!(rect(&h, "Flip polarity").height() >= MIN_HIT);
        assert!(rect(&h, "Balance cables").height() >= 24.0);
    }
}

#[test]
fn the_pan_row_is_a_full_target_too() {
    let mut h = open(demo_config());
    click_label(&mut h, "Select Stream Mix");
    let name = h
        .query_all_by_label_contains("Cable trim, ")
        .filter_map(|n| n.accesskit_node().label())
        .find(|l| l.contains(", channel 1 to Channel 1 (L)"))
        .unwrap();
    click_label(&mut h, &name);
    audit("the pan popover", &h);
    let pan = rect(&h, "Cable pan");
    assert!(pan.width() > 80.0 && pan.height() >= MIN_HIT, "{pan:?}");
}
