//! Presets in the window: the toolbar menu and the sheets to save, manage, rename, delete and
//! confirm. State and file live here; the app applies a preset itself (its Undo, save and engine
//! apply are the app's) and is told what to apply through `Out`.
use crate::sheet::{self, Glyph, Kind, Sheet as Card, button, button_named, buttons, modal};
use crate::theme::{Tokens, body, caption, radius, strong};
use crate::widgets::name_widget;
use eframe::egui::{
    self, Align, Context, CornerRadius, Frame, Key, Layout, Margin, RichText, Sense, Stroke,
    TextEdit, Ui, UiBuilder, vec2,
};
use sr_core::config::{Config, MAX_NAME_CHARS};
use sr_core::preset::{Plan, PresetFile, plan};
use sr_core::preset_store;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Toolbar button, menu rows and sheet buttons; tests click them by these names.
pub const BUTTON: &str = "Presets";
pub const SAVE_AS: &str = "Save current as…";
pub const MANAGE: &str = "Manage presets…";

const ROW_H: f32 = 52.0;

/// What the sheets ask the app to do.
#[derive(Default)]
pub struct Out {
    /// Apply this preset; `true` once the user agreed to its device changes.
    pub apply: Option<(String, bool)>,
    pub toast: Option<String>,
}

struct NameState {
    /// The preset being renamed; `None` while saving the current routing.
    rename_of: Option<String>,
    text: String,
    error: Option<String>,
    /// Opened from the manage sheet: go back to it when done.
    back: bool,
    /// First frame: put the keyboard in the field.
    fresh: bool,
}

enum Sheet {
    None,
    Name(NameState),
    Manage,
    /// The preset needs virtual devices that are not set up.
    Confirm {
        name: String,
        plan: Plan,
    },
    Delete(String),
}

/// Modified time and size: when either changes, someone else wrote the file. `None`: no file.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

pub struct Presets {
    file: PresetFile,
    /// `None` in demo mode: presets live in memory only.
    path: Option<PathBuf>,
    /// The file as `file` last read or wrote it; `refresh` reloads when the disk differs.
    seen: Stamp,
    /// A problem found while reloading, shown as a toast by the next `show`.
    notice: Option<String>,
    sheet: Sheet,
}

impl Presets {
    /// Loads `presets.json` from `path`; a corrupt one is moved aside and reported.
    pub fn load(path: Option<PathBuf>) -> (Presets, Option<String>) {
        let (file, msg) = path
            .as_deref()
            .map_or_else(|| (PresetFile::default(), None), preset_store::load);
        let seen = path.as_deref().and_then(stamp);
        (
            Presets {
                file,
                path,
                seen,
                notice: None,
                sheet: Sheet::None,
            },
            msg,
        )
    }

    /// Pick up what another process (`sr-cli preset save`) wrote since we last looked, so the
    /// next read or change works on the file as it is on disk. A corrupt file is set aside and
    /// reported by the next `show`.
    pub fn refresh(&mut self) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        if stamp(path) == self.seen {
            return;
        }
        let (file, msg) = preset_store::load(path);
        self.file = file;
        self.seen = stamp(path);
        self.notice = msg.or(self.notice.take());
    }

    pub fn names(&self) -> Vec<String> {
        self.file.presets.iter().map(|p| p.name.clone()).collect()
    }

    /// What applying `name` to `cfg` would do, or why it cannot be done.
    pub fn plan_for(&mut self, cfg: &Config, name: &str) -> Result<Plan, String> {
        self.refresh();
        let p = self
            .file
            .find(name)
            .ok_or_else(|| format!("There is no preset named \"{name}\"."))?;
        plan(cfg, p)
    }

    /// The preset's stored name, with the letter case it was saved with.
    pub fn stored_name(&self, name: &str) -> Option<&str> {
        self.file.find(name).map(|p| p.name.as_str())
    }

    /// Snapshots: a preset of `cfg` without going through the sheet.
    #[cfg(all(test, feature = "snapshots"))]
    pub fn seed(&mut self, name: &str, cfg: &Config) {
        self.file.save_as(name, cfg).unwrap();
    }

    pub fn open_save(&mut self) {
        self.sheet = Sheet::Name(NameState {
            rename_of: None,
            text: String::new(),
            error: None,
            back: false,
            fresh: true,
        });
    }

    /// Ask before applying: the preset needs devices that are not set up. A name or delete sheet
    /// that is open stays; the answer is `Err` with what to tell the user.
    pub fn ask(&mut self, name: &str, plan: Plan) -> Result<(), &'static str> {
        if matches!(self.sheet, Sheet::Name(_) | Sheet::Delete(_)) {
            return Err("Finish what you are doing first.");
        }
        self.sheet = Sheet::Confirm {
            name: name.to_owned(),
            plan,
        };
        Ok(())
    }

    /// Held from the reload to the save of one change, so `sr-cli preset save` running at that
    /// moment waits instead of one of the two changes being lost. Without the lock file (no path,
    /// unwritable folder) the change goes ahead as before.
    fn update_lock(&self) -> Option<std::fs::File> {
        preset_store::lock_for_update(self.path.as_deref()?).ok()
    }

    fn persist(&mut self) -> Option<String> {
        let path = self.path.as_ref()?;
        let res = preset_store::save(path, &self.file);
        self.seen = stamp(path);
        res.err()
            .map(|e| format!("Could not save your presets: {e}. Changes last until you quit."))
    }

    /// The popup under the toolbar button: one row per preset (a click applies it), then the
    /// actions. Returns the preset to apply.
    pub fn menu(&mut self, ui: &mut Ui, t: &Tokens) -> Option<String> {
        self.refresh();
        ui.set_min_width(220.0);
        ui.set_max_width(320.0);
        let mut apply = None;
        if self.file.presets.is_empty() {
            ui.label(RichText::new("No presets yet").font(body()).color(t.muted));
        }
        for p in &self.file.presets {
            let row = ui.add(egui::Button::new(p.name.as_str()).truncate());
            if row.on_hover_text("Apply this preset").clicked() {
                apply = Some(p.name.clone());
            }
        }
        ui.separator();
        if ui.button(SAVE_AS).clicked() {
            self.open_save();
        }
        if ui.button(MANAGE).clicked() {
            self.sheet = Sheet::Manage;
        }
        apply
    }

    /// Draw the open sheet, if any, and carry out what it decided.
    pub fn show(&mut self, ctx: &Context, t: &Tokens, cfg: &Config) -> Out {
        let mut out = Out::default();
        match std::mem::replace(&mut self.sheet, Sheet::None) {
            Sheet::None => {}
            Sheet::Name(mut st) => match name_sheet(ctx, t, &mut st, &self.file) {
                None => self.sheet = Sheet::Name(st),
                Some(false) => {
                    if st.back {
                        self.sheet = Sheet::Manage;
                    }
                }
                Some(true) => {
                    let _lock = self.update_lock();
                    self.refresh();
                    let done = match &st.rename_of {
                        None => self.file.save_as(&st.text, cfg).map(|replaced| {
                            let verb = if replaced { "Replaced" } else { "Saved" };
                            format!("{verb} preset \"{}\".", st.text.trim())
                        }),
                        Some(old) => self
                            .file
                            .rename(old, &st.text)
                            .map(|()| format!("Renamed preset to \"{}\".", st.text.trim())),
                    };
                    match done {
                        Ok(msg) => {
                            out.toast = self.persist().or(Some(msg));
                            if st.back {
                                self.sheet = Sheet::Manage;
                            }
                        }
                        Err(e) => {
                            st.error = Some(e);
                            self.sheet = Sheet::Name(st);
                        }
                    }
                }
            },
            Sheet::Manage => match manage_sheet(ctx, t, &self.file) {
                None => self.sheet = Sheet::Manage,
                Some(Row::Done) => {}
                Some(Row::Save) => {
                    self.open_save();
                    if let Sheet::Name(st) = &mut self.sheet {
                        st.back = true;
                    }
                }
                Some(Row::Apply(n)) => out.apply = Some((n, false)),
                Some(Row::Rename(n)) => {
                    self.sheet = Sheet::Name(NameState {
                        text: n.clone(),
                        rename_of: Some(n),
                        error: None,
                        back: true,
                        fresh: true,
                    });
                }
                Some(Row::Delete(n)) => self.sheet = Sheet::Delete(n),
            },
            Sheet::Confirm { name, plan } => match confirm_sheet(ctx, t, &plan) {
                None => self.sheet = Sheet::Confirm { name, plan },
                Some(false) => {}
                Some(true) => out.apply = Some((name, true)),
            },
            Sheet::Delete(name) => match delete_sheet(ctx, t, &name) {
                None => self.sheet = Sheet::Delete(name),
                Some(false) => self.sheet = Sheet::Manage,
                Some(true) => {
                    let _lock = self.update_lock();
                    self.refresh();
                    self.file.delete(&name);
                    out.toast = self
                        .persist()
                        .or_else(|| Some(format!("Deleted preset \"{name}\".")));
                    self.sheet = Sheet::Manage;
                }
            },
        }
        out.toast = out.toast.or(self.notice.take());
        out
    }
}

/// Save or rename: a name field. `Some(true)` = go ahead, `Some(false)` = cancelled.
fn name_sheet(ctx: &Context, t: &Tokens, st: &mut NameState, file: &PresetFile) -> Option<bool> {
    let renaming = st.rename_of.is_some();
    let clash = file.find(&st.text).is_some_and(|p| {
        !st.rename_of
            .as_deref()
            .is_some_and(|old| old.eq_ignore_ascii_case(&p.name))
    });
    let (title, intro, ok) = if renaming {
        ("Rename preset", "", "Rename")
    } else {
        (
            "Save preset",
            "Saves every device with its sources, wires, volumes and mutes, and the audio delay, so you can come back to this setup later.",
            if clash { "Replace" } else { "Save" },
        )
    };
    let result = Cell::new(None);
    modal(
        ctx,
        t,
        &Card {
            id: "preset_name",
            glyph: Glyph::App,
            title,
            intro,
            buttons: true,
        },
        st,
        |ui, st| {
            let id = egui::Id::unique("preset_name_field");
            let ring = if ui.memory(|m| m.has_focus(id)) {
                t.focus
            } else {
                t.hairline
            };
            let te = Frame::new()
                .fill(t.canvas)
                .stroke(Stroke::new(if ring == t.focus { 2.0 } else { 1.0 }, ring))
                .corner_radius(CornerRadius::same(radius::INPUT))
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.add(
                        TextEdit::singleline(&mut st.text)
                            .id(id)
                            .frame(Frame::NONE)
                            .hint_text("Preset name")
                            .char_limit(MAX_NAME_CHARS)
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner;
            name_widget(ui, &te, "Preset name");
            if std::mem::take(&mut st.fresh) {
                te.request_focus();
            }
            if te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                result.set(Some(true));
            }
            if let Some(e) = &st.error {
                sheet::error(ui, t, e);
            } else if clash && !renaming {
                sheet::body(ui, t, "A preset with this name exists. Saving replaces it.");
            }
        },
        |ui, st| {
            buttons(
                ui,
                st,
                |_, _| {},
                |ui, _| {
                    if button(ui, t, ok, Kind::Primary) {
                        result.set(Some(true));
                    }
                    if button(ui, t, "Cancel", Kind::Secondary) {
                        result.set(Some(false));
                    }
                },
            );
        },
    );
    result.into_inner()
}

enum Row {
    Apply(String),
    Rename(String),
    Delete(String),
    Save,
    Done,
}

/// The list of presets with a row of buttons each.
fn manage_sheet(ctx: &Context, t: &Tokens, file: &PresetFile) -> Option<Row> {
    let result = Cell::new(None);
    modal(
        ctx,
        t,
        &Card {
            id: "preset_manage",
            glyph: Glyph::App,
            title: "Presets",
            intro: "A preset is your whole routing under a name. Applying one can be undone.",
            buttons: true,
        },
        &mut (),
        |ui, ()| {
            if file.presets.is_empty() {
                sheet::body(
                    ui,
                    t,
                    "No presets yet. Save the current setup to switch back to it later.",
                );
                return;
            }
            let w = ui.available_width();
            Frame::new()
                .fill(t.canvas)
                .corner_radius(CornerRadius::same(radius::ROW))
                .inner_margin(Margin::symmetric(14, 0))
                .show(ui, |ui| {
                    ui.set_width(w - 28.0);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for (i, p) in file.presets.iter().enumerate() {
                        if i > 0 {
                            let (line, _) = ui.allocate_exact_size(
                                vec2(ui.available_width(), 1.0),
                                Sense::hover(),
                            );
                            ui.painter().rect_filled(line, 0.0, t.hairline);
                        }
                        if let Some(r) = preset_row(ui, t, p) {
                            result.set(Some(r));
                        }
                    }
                });
        },
        |ui, ()| {
            buttons(
                ui,
                &mut (),
                |ui, ()| {
                    if button(ui, t, SAVE_AS, Kind::Secondary) {
                        result.set(Some(Row::Save));
                    }
                },
                |ui, ()| {
                    if button(ui, t, "Done", Kind::Primary) {
                        result.set(Some(Row::Done));
                    }
                },
            );
        },
    );
    result.into_inner()
}

fn preset_row(ui: &mut Ui, t: &Tokens, p: &sr_core::preset::Preset) -> Option<Row> {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let mut act = None;
    let mut btns = ui.new_child(
        UiBuilder::new()
            .max_rect(rect)
            .layout(Layout::right_to_left(Align::Center)),
    );
    btns.spacing_mut().item_spacing.x = 6.0;
    if sheet::button_named(
        &mut btns,
        t,
        "Delete",
        &format!("Delete {}", p.name),
        Kind::Secondary,
    ) {
        act = Some(Row::Delete(p.name.clone()));
    }
    if button_named(
        &mut btns,
        t,
        "Rename",
        &format!("Rename {}", p.name),
        Kind::Secondary,
    ) {
        act = Some(Row::Rename(p.name.clone()));
    }
    if button_named(
        &mut btns,
        t,
        "Apply",
        &format!("Apply {}", p.name),
        Kind::Secondary,
    ) {
        act = Some(Row::Apply(p.name.clone()));
    }
    let used = rect.right() - btns.min_rect().left();
    let max_w = (rect.width() - used - 12.0).max(40.0);
    let n = p.devices.len();
    let detail = format!("{n} device{}", if n == 1 { "" } else { "s" });
    crate::graph::text_left(
        ui,
        rect.left(),
        rect.center().y - 8.0,
        &p.name,
        strong(),
        t.ink,
        max_w,
    );
    crate::graph::text_left(
        ui,
        rect.left(),
        rect.center().y + 10.0,
        &detail,
        caption(),
        t.muted,
        max_w,
    );
    act
}

/// "This preset needs 2 virtual devices; add them?" `Some(true)` = yes, `Some(false)` = cancelled.
fn confirm_sheet(ctx: &Context, t: &Tokens, plan: &Plan) -> Option<bool> {
    let (title, question, ok) = plan.confirm()?;
    let mut result = None;
    modal(
        ctx,
        t,
        &Card {
            id: "preset_confirm",
            glyph: Glyph::Warn,
            title: &title,
            intro: &question,
            buttons: true,
        },
        &mut (),
        |ui, ()| {
            for c in &plan.changes {
                ui.label(RichText::new(c.describe()).font(body()).color(t.ink));
            }
        },
        |ui, ()| {
            buttons(
                ui,
                &mut (),
                |_, ()| {},
                |ui, ()| {
                    if button(ui, t, ok, Kind::Primary) {
                        result = Some(true);
                    }
                    if button(ui, t, "Cancel", Kind::Secondary) {
                        result = Some(false);
                    }
                },
            );
        },
    );
    result
}

fn delete_sheet(ctx: &Context, t: &Tokens, name: &str) -> Option<bool> {
    let mut result = None;
    modal(
        ctx,
        t,
        &Card {
            id: "preset_delete",
            glyph: Glyph::Warn,
            title: &format!("Delete \"{name}\"?"),
            intro: "The saved setup is removed. Your current routing stays as it is.",
            buttons: true,
        },
        &mut (),
        |_, ()| {},
        |ui, ()| {
            buttons(
                ui,
                &mut (),
                |_, ()| {},
                |ui, ()| {
                    if button(ui, t, "Delete", Kind::Danger) {
                        result = Some(true);
                    }
                    if button(ui, t, "Cancel", Kind::Secondary) {
                        result = Some(false);
                    }
                },
            );
        },
    );
    result
}
