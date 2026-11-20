//! Presets through the whole window: the toolbar menu, the sheets and `soundrouter://` links.
use super::tests::{click, has, open, settle};
use super::*;
use crate::fake::demo_config;
use crate::model::{Edit, apply};
use egui_kittest::{Harness, kittest::Queryable};

fn open_menu(h: &mut Harness<App>) {
    click(h, presets::BUTTON);
}

/// Save the current routing as `name` through the menu and the sheet.
fn save_as(h: &mut Harness<App>, name: &str) {
    open_menu(h);
    click(h, presets::SAVE_AS);
    h.get_by_label("Preset name").type_text(name);
    settle(h);
    click(h, "Save");
}

fn link(h: &mut Harness<App>, url: &str) {
    let ctx = h.ctx.clone();
    h.state_mut().open_link(&ctx, url);
    settle(h);
}

/// The preset `name` applied to the current config, as a config.
fn merged(h: &Harness<App>, name: &str) -> Config {
    let app = h.state();
    app.presets.plan_for(&app.cfg, name).unwrap().config
}

#[test]
fn saving_stores_the_whole_routing_and_says_so() {
    let mut h = open(demo_config());
    save_as(&mut h, "  Streaming ");
    assert_eq!(h.state().presets.names(), ["Streaming"]);
    assert_eq!(
        merged(&h, "Streaming"),
        h.state().cfg,
        "everything is in it"
    );
    h.get_by_label("Saved preset \"Streaming\".");
    assert!(!has(&h, "Preset name"), "the sheet closed");
}

#[test]
fn saving_over_a_name_says_replace_and_keeps_one_preset() {
    let mut h = open(demo_config());
    save_as(&mut h, "Mix");
    apply(&mut h.state_mut().cfg, Edit::Mute(0, true));
    open_menu(&mut h);
    click(&mut h, presets::SAVE_AS);
    h.get_by_label("Preset name").type_text("mix");
    settle(&mut h);
    h.get_by_label_contains("Saving replaces it");
    click(&mut h, "Replace");
    assert_eq!(h.state().presets.names(), ["mix"]);
    h.get_by_label("Replaced preset \"mix\".");
    assert!(merged(&h, "mix").devices[0].muted);
}

#[test]
fn a_blank_name_is_refused_in_the_sheet() {
    let mut h = open(demo_config());
    open_menu(&mut h);
    click(&mut h, presets::SAVE_AS);
    click(&mut h, "Save");
    h.get_by_label("Give the preset a name.");
    assert!(h.state().presets.names().is_empty());
}

#[test]
fn applying_from_the_menu_restores_the_routing_and_undo_takes_it_back() {
    let mut h = open(demo_config());
    save_as(&mut h, "Streaming");
    let saved = h.state().cfg.clone();
    apply(&mut h.state_mut().cfg, Edit::Mute(0, true));
    apply(&mut h.state_mut().cfg, Edit::Volume(1, -20.0));
    apply(&mut h.state_mut().cfg, Edit::BufferFrames(64));
    let changed = h.state().cfg.clone();
    assert_ne!(changed, saved);
    open_menu(&mut h);
    click(&mut h, "Streaming");
    assert_eq!(h.state().cfg, saved);
    h.get_by_label("Applied preset Streaming");
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, changed);
    assert!(!has(&h, "Undo"));
}

#[test]
fn a_preset_that_needs_a_missing_device_asks_before_adding_it() {
    let mut h = open(demo_config());
    save_as(&mut h, "Three");
    let full = h.state().cfg.clone();
    h.state_mut().cfg.devices.remove(1);
    let two = h.state().cfg.clone();
    open_menu(&mut h);
    click(&mut h, "Three");
    h.get_by_label("Add virtual devices?");
    h.get_by_label("This preset needs 1 virtual device that is not set up; add it?");
    assert_eq!(h.state().cfg, two, "nothing happens before the yes");
    click(&mut h, "Cancel");
    assert_eq!(h.state().cfg, two);
    assert!(!has(&h, "Add virtual devices?"));

    open_menu(&mut h);
    click(&mut h, "Three");
    click(&mut h, "Add devices");
    let by_id = |c: &Config| {
        let mut d = c.devices.clone();
        d.sort_by(|a, b| a.id.cmp(&b.id));
        d
    };
    assert_eq!(
        by_id(&h.state().cfg),
        by_id(&full),
        "the missing one is back"
    );
    click(&mut h, "Undo");
    assert_eq!(h.state().cfg, two);
}

#[test]
fn a_link_applies_within_enabled_devices_without_asking() {
    let mut h = open(demo_config());
    save_as(&mut h, "Streaming");
    let saved = h.state().cfg.clone();
    apply(&mut h.state_mut().cfg, Edit::Volume(0, -30.0));
    link(&mut h, "soundrouter://preset/streaming");
    assert_eq!(h.state().cfg, saved);
    h.get_by_label("Applied preset Streaming");
}

#[test]
fn a_link_never_turns_on_a_device_the_user_turned_off_without_asking() {
    let mut h = open(demo_config());
    save_as(&mut h, "All on");
    apply(&mut h.state_mut().cfg, Edit::Enable(1, false));
    let before = h.state().cfg.clone();
    link(&mut h, "soundrouter://preset/All%20on");
    h.get_by_label("Add virtual devices?");
    assert_eq!(h.state().cfg, before);
    click(&mut h, "Cancel");
    assert_eq!(h.state().cfg, before, "declined");
    link(&mut h, "soundrouter://preset/All%20on");
    click(&mut h, "Add devices");
    assert!(h.state().cfg.devices[1].enabled, "after the yes");
}

#[test]
fn an_unknown_preset_or_bad_link_shows_a_message_and_changes_nothing() {
    let mut h = open(demo_config());
    let before = h.state().cfg.clone();
    link(&mut h, "soundrouter://preset/nope");
    h.get_by_label("There is no preset named \"nope\".");
    link(&mut h, "soundrouter://frobnicate/x");
    h.get_by_label("Unknown SoundRouter link action \"frobnicate\".");
    link(&mut h, "soundrouter://preset/%zz");
    h.get_by_label_contains("Bad escape");
    assert_eq!(h.state().cfg, before);
    assert!(!has(&h, "Undo"));
}

#[test]
fn a_preset_that_no_longer_validates_is_refused_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("presets.json");
    std::fs::write(
        &path,
        r#"{"version":1,"presets":[{"name":"Broken","devices":[{"id":"v1","name":"V","channels":2,
        "sources":[],"wires":[{"source":"ghost","src_ch":0,"out_ch":0}],"monitors":[]}]}]}"#,
    )
    .unwrap();
    let mut h = open(demo_config());
    h.state_mut().presets = Presets::load(Some(path)).0;
    let before = h.state().cfg.clone();
    open_menu(&mut h);
    click(&mut h, "Broken");
    h.get_by_label_contains("Preset \"Broken\" cannot be applied");
    assert_eq!(h.state().cfg, before);
}

#[test]
fn manage_renames_deletes_and_applies() {
    let mut h = open(demo_config());
    save_as(&mut h, "Streaming");
    save_as(&mut h, "Podcast");
    open_menu(&mut h);
    click(&mut h, presets::MANAGE);
    click(&mut h, "Rename Streaming");
    super::tests::select_all(&mut h);
    h.get_by_label("Preset name").type_text("podcast");
    settle(&mut h);
    click(&mut h, "Rename");
    h.get_by_label("A preset named \"podcast\" already exists.");
    h.get_by_label("Preset name").click();
    super::tests::select_all(&mut h);
    h.get_by_label("Preset name").type_text("Live");
    settle(&mut h);
    click(&mut h, "Rename");
    assert_eq!(h.state().presets.names(), ["Live", "Podcast"]);
    h.get_by_label("Done"); // back in the manage sheet

    click(&mut h, "Delete Live");
    h.get_by_label("Delete \"Live\"?");
    click(&mut h, "Cancel");
    assert_eq!(h.state().presets.names().len(), 2, "cancel keeps it");
    click(&mut h, "Delete Live");
    click(&mut h, "Delete");
    assert_eq!(h.state().presets.names(), ["Podcast"]);

    apply(&mut h.state_mut().cfg, Edit::Mute(0, true));
    click(&mut h, "Apply Podcast");
    assert!(!h.state().cfg.devices[0].muted);
    assert!(!has(&h, "Done"), "applying closes the sheet");
}

#[test]
fn presets_survive_a_restart_and_a_corrupt_file_is_moved_aside() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("presets.json");
    let mut h = open(demo_config());
    h.state_mut().presets = Presets::load(Some(path.clone())).0;
    save_as(&mut h, "Kept");
    let (again, msg) = Presets::load(Some(path.clone()));
    assert!(msg.is_none());
    assert_eq!(again.names(), ["Kept"]);

    std::fs::write(&path, "{nope").unwrap();
    let (none, msg) = Presets::load(Some(path.clone()));
    assert!(none.names().is_empty());
    assert!(msg.unwrap().contains("presets.json.bad"));
    assert!(dir.path().join("presets.json.bad").exists());
}
