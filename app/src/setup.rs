//! First-run setup: driver install/uninstall through the macOS administrator prompt, the
//! permission explanations, the loopback self-test and the sheet that walks through them.
use crate::sheet::{
    Glyph, Kind, Row, Sheet, Status, body, button, buttons, checklist, error, modal, progress,
};
use crate::theme::{Tokens, tokens};
use crate::widgets::name_button;
use crate::worker::SelfTestState;
use eframe::egui::{self, Align2, Context, CornerRadius, FontId, Id, Rect, Sense, Stroke, Theme};
use sr_core::backend::{EngineStatus, TccStatus};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};

pub const AUDIO_CAPTURE_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AudioCapture";
const MIC_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
/// The driver must show up in the HAL this long after the install script succeeded.
const DRIVER_WAIT_SECS: f64 = 10.0;
/// The loopback test must finish this long after the Verify step first saw it running.
const SELF_TEST_SECS: f64 = 30.0;

/// Where coreaudiod loads third-party HAL plug-ins from.
pub const HAL_DIR: &str = "/Library/Audio/Plug-Ins/HAL";
pub const INSTALLED_DRIVER: &str = "/Library/Audio/Plug-Ins/HAL/SoundRouter.driver";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The user dismissed the password dialog.
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed(m) => f.write_str(m),
        }
    }
}

/// Runs a shell script as root. Injected into the app so tests never prompt.
pub type Runner = Arc<dyn Fn(&str) -> Result<(), SetupError> + Send + Sync>;

/// POSIX single-quote `s`: safe for any bytes except NUL (which paths cannot contain).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// An AppleScript string literal for `s`.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', r#"\""#))
}

/// Copy the driver bundle at `src` into the HAL folder, fix owner and quarantine, restart coreaudiod.
/// `src` is quoted, so any path is safe (non-UTF-8 bytes are replaced, which makes `ditto` fail loudly).
pub fn install_script(src: &Path) -> String {
    let dest = sh_quote(INSTALLED_DRIVER);
    format!(
        "mkdir -p {hal} && rm -rf {dest} && ditto {src} {dest} && chown -R root:wheel {dest} \
         && xattr -dr com.apple.quarantine {dest} && killall coreaudiod",
        hal = sh_quote(HAL_DIR),
        src = sh_quote(&src.to_string_lossy()),
    )
}

/// Remove the driver; also the user's settings folder when `remove_config`.
pub fn uninstall_script(remove_config: bool) -> String {
    let dir = crate::store::default_path();
    uninstall_script_for(
        dir.as_deref()
            .and_then(Path::parent)
            .filter(|_| remove_config),
    )
}

/// `config_dir` is deleted too when given.
fn uninstall_script_for(config_dir: Option<&Path>) -> String {
    let mut s = format!("rm -rf {}", sh_quote(INSTALLED_DRIVER));
    if let Some(d) = config_dir {
        s += &format!(" && rm -rf {}", sh_quote(&d.to_string_lossy()));
    }
    s + " && killall coreaudiod"
}

/// Map osascript's outcome: error -128 is the user pressing Cancel.
fn classify(success: bool, stderr: &str) -> Result<(), SetupError> {
    if success {
        Ok(())
    } else if stderr.contains("(-128)") {
        Err(SetupError::Cancelled)
    } else {
        Err(SetupError::Failed(stderr.trim().to_string()))
    }
}

/// Run `script` as root behind the system password dialog. Blocks until the user answers.
pub fn run_admin(script: &str) -> Result<(), SetupError> {
    let apple = format!(
        "do shell script {} with administrator privileges",
        applescript_string(script)
    );
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(apple)
        .output()
        .map_err(|e| SetupError::Failed(format!("could not run osascript: {e}")))?;
    classify(out.status.success(), &String::from_utf8_lossy(&out.stderr))
}

/// The driver bundle shipped with this app: `Contents/Resources` of the .app, or `target/` when
/// running a cargo build.
pub fn locate_driver() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("../Resources/SoundRouter.driver"),
        dir.join("../../SoundRouter.driver"),
    ]
    .into_iter()
    .find(|p| p.is_dir())
    .and_then(|p| p.canonicalize().ok())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Driver,
    Permissions,
    Verify,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Installing,
    Uninstalling,
}

/// What the sheet asks the app to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Create the devices if that has not happened yet, then loopback-test (the Microphone prompt).
    AllowMic,
    Test,
    /// Leave the real engine for the synthetic one.
    Demo,
    /// The uninstall deleted the settings folder.
    SettingsDeleted,
}

/// What the app tells the sheet each frame.
pub struct View<'a> {
    pub status: &'a EngineStatus,
    /// The first status poll has landed.
    pub ready: bool,
    pub test: &'a SelfTestState,
    pub device_name: &'a str,
    /// The driver is there but nothing has been applied yet, because the Microphone prompt would
    /// otherwise appear unexplained.
    pub first_apply_pending: bool,
    pub now: f64,
    pub diagnostics: &'a dyn Fn() -> String,
}

pub struct Setup {
    runner: Runner,
    /// The driver bundle to install; `None` when this build ships without one.
    source: Option<PathBuf>,
    step: Step,
    /// Opened by "Test Setup…" or after an install; otherwise the sheet shows only without a driver.
    open: bool,
    busy: Option<Busy>,
    rx: Option<Receiver<Result<(), SetupError>>>,
    error: Option<String>,
    /// Time by which the driver must have appeared after a successful install.
    deadline: Option<f64>,
    /// Uninstall dialog: `Some(delete settings too)`.
    confirm: Option<bool>,
    copied: bool,
    demo: bool,
    shown: bool,
    /// When the Verify step first saw the loopback test running; `None` while it is not.
    test_since: Option<f64>,
}

impl Setup {
    pub fn new(runner: Runner, source: Option<PathBuf>) -> Self {
        Self {
            runner,
            source,
            step: Step::Welcome,
            open: false,
            busy: None,
            rx: None,
            error: None,
            deadline: None,
            confirm: None,
            copied: false,
            demo: false,
            shown: false,
            test_since: None,
        }
    }

    /// The real thing: the administrator prompt and the bundled driver.
    pub fn system() -> Self {
        Self::new(Arc::new(run_admin), locate_driver())
    }

    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// Whether the sheet was drawn last frame.
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// "Test Setup…": start at the permissions, the driver is already there.
    pub fn open_test(&mut self) {
        self.step = Step::Permissions;
        self.open = true;
        self.copied = false;
    }

    pub fn ask_uninstall(&mut self) {
        self.confirm = Some(false);
        self.error = None;
    }

    fn spawn(&mut self, ctx: &Context, busy: Busy, script: String) {
        let (tx, rx) = mpsc::channel();
        let (runner, ctx) = (self.runner.clone(), ctx.clone());
        let job = move || {
            let _ = tx.send(runner(&script));
            ctx.request_repaint();
        };
        // Tests run the job here, so its result is there for the next frame whatever the scheduler does.
        if cfg!(test) {
            job();
        } else {
            std::thread::spawn(job);
        }
        self.busy = Some(busy);
        self.rx = Some(rx);
        self.error = None;
    }

    fn start_install(&mut self, ctx: &Context) {
        self.step = Step::Driver;
        match self.source.clone() {
            Some(src) => self.spawn(ctx, Busy::Installing, install_script(&src)),
            None => {
                self.error = Some(
                    "This copy of SoundRouter does not contain the driver (SoundRouter.driver)."
                        .into(),
                );
            }
        }
    }

    fn poll(&mut self, now: f64) -> Option<Action> {
        let r = self.rx.as_ref().and_then(|rx| rx.try_recv().ok())?;
        self.rx = None;
        match (self.busy.take(), r) {
            (Some(Busy::Installing), Ok(())) => self.deadline = Some(now + DRIVER_WAIT_SECS),
            (Some(Busy::Uninstalling), Ok(())) => {
                if self.confirm.take() == Some(true) {
                    return Some(Action::SettingsDeleted);
                }
            }
            (_, Err(SetupError::Cancelled)) => self.error = Some("Cancelled.".into()),
            (_, Err(SetupError::Failed(m))) => self.error = Some(m),
            (None, Ok(())) => {}
        }
        None
    }

    /// Move between steps when the world changed under the sheet.
    fn sync(&mut self, v: &View) {
        let driver = v.status.driver_version.is_some();
        if !driver {
            if !matches!(self.step, Step::Welcome | Step::Driver) {
                self.step = Step::Welcome; // driver removed behind our back
            }
            if self.deadline.is_some_and(|d| v.now > d) {
                self.deadline = None;
                self.error = Some(
                    "The driver was installed but macOS has not loaded it yet. Try again, or log out and back in."
                        .into(),
                );
            }
        } else if matches!(self.step, Step::Welcome | Step::Driver) {
            if self.deadline.take().is_some() || v.first_apply_pending {
                self.step = Step::Permissions;
                self.open = true;
            } else {
                self.step = Step::Welcome;
            }
        }
    }

    /// Draw whatever is due; returns at most one request for the app.
    pub fn show(&mut self, ctx: &Context, v: &View) -> Option<Action> {
        let deleted = self.poll(v.now);
        self.shown = false;
        if self.demo || !v.ready {
            return deleted;
        }
        self.sync(v);
        let t = tokens(ctx.theme() == Theme::Dark);
        let mut action = None;
        if self.open || v.status.driver_version.is_none() {
            self.shown = true;
            action = self.sheet(ctx, &t, v);
        }
        if self.confirm.is_some() {
            self.uninstall_dialog(ctx, &t);
        }
        deleted.or(action)
    }

    fn close(&mut self) {
        self.open = false;
        self.step = Step::Welcome;
        self.test_since = None;
    }

    fn sheet(&mut self, ctx: &Context, t: &Tokens, v: &View) -> Option<Action> {
        let mut action = None;
        match self.step {
            Step::Welcome => modal(
                ctx,
                t,
                &Sheet {
                    id: "setup_welcome",
                    buttons: true,
                    glyph: Glyph::App,
                    title: "Set up SoundRouter",
                    intro: "SoundRouter creates virtual audio devices and routes app and microphone audio \
                            between them. It needs a small system audio driver; macOS will ask for your \
                            password to install it.",
                },
                self,
                |ui, _| {
                    checklist(
                        ui,
                        t,
                        &[Row {
                            state: Status::Pending,
                            title: "Audio driver",
                            status: "Not installed",
                            detail: "Adds the virtual devices to macOS.",
                            action: None,
                        }],
                    );
                },
                |ui, s| {
                    buttons(
                        ui,
                        s,
                        |ui, s| {
                            if button(ui, t, "Try Demo", Kind::Secondary) {
                                s.demo = true;
                                action = Some(Action::Demo);
                            }
                        },
                        |ui, s| {
                            if button(ui, t, "Install Driver…", Kind::Primary) {
                                s.start_install(ctx);
                            }
                        },
                    );
                },
            ),
            Step::Driver => {
                let installing = self.busy == Some(Busy::Installing);
                let starting = self.deadline.is_some();
                let (glyph, title, intro) = if installing {
                    (
                        Glyph::App,
                        "Install the audio driver",
                        "macOS asks for your password to copy the driver into place.",
                    )
                } else if starting {
                    (
                        Glyph::App,
                        "Install the audio driver",
                        "The driver is installed. macOS is loading it.",
                    )
                } else {
                    (
                        Glyph::Warn,
                        "Driver not installed",
                        "SoundRouter needs its audio driver to create virtual devices.",
                    )
                };
                modal(
                    ctx,
                    t,
                    &Sheet {
                        id: "setup_driver",
                        buttons: !(installing || starting),
                        glyph,
                        title,
                        intro,
                    },
                    self,
                    |ui, s| {
                        if installing {
                            progress(ui, t, "Waiting for the password dialog…");
                        } else if starting {
                            progress(ui, t, "Starting the driver…");
                        } else {
                            if let Some(e) = &s.error {
                                error(ui, t, e);
                            }
                            if s.copied {
                                body(ui, t, "Copied to clipboard");
                            }
                        }
                    },
                    |ui, s| {
                        if installing || starting {
                            return;
                        }
                        buttons(
                            ui,
                            s,
                            |ui, s| {
                                if button(ui, t, "Try Demo", Kind::Secondary) {
                                    s.demo = true;
                                    action = Some(Action::Demo);
                                }
                                s.copy_button(ui, t, v);
                            },
                            |ui, s| {
                                if button(ui, t, "Retry", Kind::Primary) {
                                    s.start_install(ctx);
                                }
                            },
                        );
                    },
                )
            }
            Step::Permissions => {
                let mic = v.status.mic;
                modal(
                    ctx,
                    t,
                    &Sheet {
                        id: "setup_permissions",
                        buttons: true,
                        glyph: Glyph::App,
                        title: "Allow access",
                        intro: "SoundRouter needs two permissions. macOS asks for each the first time it is needed.",
                    },
                    self,
                    |ui, _| {
                        let rows = permission_rows(v, true);
                        if checklist(ui, t, &rows) == Some(1) {
                            open_url(AUDIO_CAPTURE_SETTINGS);
                        }
                    },
                    |ui, s| {
                        buttons(
                            ui,
                            s,
                            |_, _| {},
                            |ui, s| {
                                match mic {
                                    TccStatus::Granted => {
                                        if button(ui, t, "Continue", Kind::Primary) {
                                            s.step = Step::Verify;
                                            action = Some(Action::Test);
                                        }
                                    }
                                    TccStatus::Unknown => {
                                        if button(ui, t, "Allow Microphone", Kind::Primary) {
                                            s.step = Step::Verify;
                                            action = Some(Action::AllowMic);
                                        }
                                    }
                                    TccStatus::Denied => {
                                        if button(ui, t, "Open Microphone Settings", Kind::Primary)
                                        {
                                            open_url(MIC_SETTINGS);
                                        }
                                        if button(ui, t, "Continue", Kind::Secondary) {
                                            s.step = Step::Verify;
                                            action = Some(Action::Test);
                                        }
                                    }
                                }
                                if !v.first_apply_pending && button(ui, t, "Close", Kind::Secondary)
                                {
                                    s.close();
                                }
                            },
                        );
                    },
                )
            }
            Step::Verify => {
                let running = matches!(v.test, SelfTestState::Idle | SelfTestState::Running);
                if !running {
                    self.test_since = None;
                }
                let since = *self.test_since.get_or_insert(v.now);
                let hung = running && v.now - since > SELF_TEST_SECS;
                let running = running && !hung;
                let (glyph, intro) = match v.test {
                    _ if hung => (Glyph::Fail, "The tone did not come back."),
                    SelfTestState::Idle | SelfTestState::Running => (
                        Glyph::App,
                        "SoundRouter plays a quiet tone into the virtual device and listens for it to come back.",
                    ),
                    SelfTestState::Done(Ok(r)) if r.passed => {
                        (Glyph::Ok, "The tone made it through the virtual device.")
                    }
                    SelfTestState::Done(_) => (Glyph::Fail, "The tone did not come back cleanly."),
                };
                modal(
                    ctx,
                    t,
                    &Sheet {
                        id: "setup_verify",
                        buttons: true,
                        glyph,
                        title: "Check the audio path",
                        intro,
                    },
                    self,
                    |ui, s| {
                        let flows;
                        let why;
                        let mut rows = permission_rows(v, false).to_vec();
                        rows.insert(
                            0,
                            Row {
                                state: Status::Ok,
                                title: "Audio driver",
                                status: "Loaded",
                                detail: "",
                                action: None,
                            },
                        );
                        rows.push(match v.test {
                            _ if hung => Row {
                                state: Status::Fail,
                                title: "Loopback test",
                                status: "Failed",
                                detail: "The test did not finish",
                                action: None,
                            },
                            SelfTestState::Idle | SelfTestState::Running => Row {
                                state: Status::Running,
                                title: "Loopback test",
                                status: "Testing",
                                detail: "",
                                action: None,
                            },
                            SelfTestState::Done(Ok(r)) if r.passed => {
                                flows = format!(
                                    "Audio flows ({:.0} dBFS, {} dropouts)",
                                    r.rms_dbfs, r.dropouts
                                );
                                Row {
                                    state: Status::Ok,
                                    title: "Loopback test",
                                    status: "Passed",
                                    detail: &flows,
                                    action: None,
                                }
                            }
                            SelfTestState::Done(r) => {
                                why = match r {
                                    Ok(r) => format!(
                                        "No clean audio came back ({:.0} dBFS, {} dropouts).",
                                        r.rms_dbfs, r.dropouts
                                    ),
                                    Err(e) => format!("The test failed: {e}"),
                                };
                                Row {
                                    state: Status::Fail,
                                    title: "Loopback test",
                                    status: "Failed",
                                    detail: &why,
                                    action: None,
                                }
                            }
                        });
                        checklist(ui, t, &rows);
                        match v.test {
                            _ if hung => body(ui, t, "Try again, or copy diagnostics for support."),
                            SelfTestState::Idle | SelfTestState::Running => {
                                progress(ui, t, "Playing a quiet tone through the virtual device…");
                            }
                            SelfTestState::Done(Ok(r)) if r.passed => {}
                            SelfTestState::Done(_) => body(
                                ui,
                                t,
                                if v.status.mic == TccStatus::Granted {
                                    "Microphone access is allowed, so the audio path itself looks silent. Try again, or copy diagnostics for support."
                                } else {
                                    "Check the Microphone permission, then try again."
                                },
                            ),
                        }
                        if s.copied {
                            body(ui, t, "Copied to clipboard");
                        }
                    },
                    |ui, s| match v.test {
                        _ if running => buttons(
                            ui,
                            s,
                            |_, _| {},
                            |ui, s| {
                                if button(ui, t, "Cancel", Kind::Secondary) {
                                    s.close();
                                }
                            },
                        ),
                        _ if hung => buttons(
                            ui,
                            s,
                            |ui, s| s.copy_button(ui, t, v),
                            |ui, s| {
                                if button(ui, t, "Retry", Kind::Primary) {
                                    s.test_since = None;
                                    action = Some(Action::Test);
                                }
                                if button(ui, t, "Close", Kind::Secondary) {
                                    s.close();
                                }
                            },
                        ),
                        SelfTestState::Idle | SelfTestState::Running => {}
                        SelfTestState::Done(Ok(r)) if r.passed => buttons(
                            ui,
                            s,
                            |_, _| {},
                            |ui, s| {
                                if button(ui, t, "Continue", Kind::Primary) {
                                    s.step = Step::Done;
                                }
                            },
                        ),
                        SelfTestState::Done(_) => buttons(
                            ui,
                            s,
                            |ui, s| s.copy_button(ui, t, v),
                            |ui, s| {
                                if button(ui, t, "Retry", Kind::Primary) {
                                    s.test_since = None;
                                    action = Some(Action::Test);
                                }
                                if button(ui, t, "Close", Kind::Secondary) {
                                    s.close();
                                }
                            },
                        ),
                    },
                )
            }
            Step::Done => modal(
                ctx,
                t,
                &Sheet {
                    id: "setup_done",
                    buttons: true,
                    glyph: Glyph::Ok,
                    title: "You're all set",
                    intro: &format!(
                        "In Zoom, OBS or any other app, choose “{}” as the microphone or speaker.",
                        v.device_name
                    ),
                },
                self,
                |_, _| {},
                |ui, s| {
                    buttons(
                        ui,
                        s,
                        |_, _| {},
                        |ui, s| {
                            if button(ui, t, "Done", Kind::Primary) {
                                s.close();
                            }
                        },
                    );
                },
            ),
        }
        action
    }

    fn copy_button(&mut self, ui: &mut egui::Ui, t: &Tokens, v: &View) {
        if button(ui, t, "Copy diagnostics", Kind::Secondary) {
            ui.ctx().copy_text((v.diagnostics)());
            self.copied = true;
        }
    }

    fn uninstall_dialog(&mut self, ctx: &Context, t: &Tokens) {
        let busy = self.busy == Some(Busy::Uninstalling);
        modal(
            ctx,
            t,
            &Sheet {
                id: "uninstall_dialog",
                buttons: !busy,
                glyph: Glyph::Warn,
                title: "Uninstall the audio driver?",
                intro: &format!(
                    "This removes SoundRouter.driver from {HAL_DIR} and briefly restarts macOS audio. \
                     macOS will ask for your password."
                ),
            },
            self,
            |ui, s| {
                if let Some(remove) = &mut s.confirm {
                    ui.add_enabled(
                        !busy,
                        egui::Checkbox::new(remove, "Also delete my SoundRouter settings"),
                    );
                }
                if busy {
                    progress(ui, t, "Waiting for the password dialog…");
                } else if let Some(e) = &s.error {
                    error(ui, t, &format!("Not uninstalled: {e}"));
                }
            },
            |ui, s| {
                if busy {
                    return;
                }
                buttons(
                    ui,
                    s,
                    |_, _| {},
                    |ui, s| {
                        if button(ui, t, "Uninstall", Kind::Danger) {
                            let script = uninstall_script(s.confirm == Some(true));
                            s.spawn(ctx, Busy::Uninstalling, script);
                        }
                        if button(ui, t, "Cancel", Kind::Secondary) {
                            s.confirm = None;
                            s.error = None;
                        }
                    },
                );
            },
        );
    }
}

/// Microphone and System Audio Recording as checklist rows. `explain` adds the why and the
/// Settings shortcut (index 1 is the audio capture row).
fn permission_rows<'a>(v: &View, explain: bool) -> [Row<'a>; 2] {
    let look = |s: TccStatus| match s {
        TccStatus::Granted => Status::Ok,
        TccStatus::Denied => Status::Fail,
        TccStatus::Unknown => Status::Pending,
    };
    let (mic, cap) = (v.status.mic, v.status.tcc);
    [
        Row {
            state: look(mic),
            title: "Microphone",
            status: tcc_word(mic),
            detail: if explain {
                "Reads the virtual device's input to check that audio flows, and any microphone or input device you add as a source. macOS asks the first time."
            } else {
                ""
            },
            action: None,
        },
        Row {
            state: look(cap),
            title: "System Audio Recording",
            status: tcc_word(cap),
            detail: if explain {
                "Captures audio from apps you add as sources. macOS asks when the first app source starts."
            } else {
                ""
            },
            action: (explain && cap != TccStatus::Granted).then_some("Open Settings"),
        },
    ]
}

/// For support text: the status as the user sees it when it is not decided, else its name.
fn tcc_diag(s: TccStatus) -> String {
    match s {
        TccStatus::Unknown => "Not asked yet".into(),
        _ => format!("{s:?}"),
    }
}

fn tcc_word(s: TccStatus) -> &'static str {
    match s {
        TccStatus::Granted => "Allowed",
        TccStatus::Denied => "Denied",
        TccStatus::Unknown => "Not asked yet",
    }
}

fn open_url(url: &str) {
    let _ = Command::new("open").arg(url).spawn();
}

/// Footer button in the style of the Delete button.
pub fn footer_button(ui: &mut egui::Ui, rect: Rect, id: &str, label: &str, t: &Tokens) -> bool {
    let resp = ui.interact(rect, Id::unique(id), Sense::click());
    name_button(&resp, label);
    let fill = if resp.hovered() {
        t.card_stroke
    } else {
        t.card
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        fill,
        Stroke::new(1.0, t.card_stroke),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(12.5),
        t.text,
    );
    resp.clicked()
}

/// Everything support needs, in the spirit of `sr-cli doctor`.
pub fn diagnostics(
    os: &str,
    status: &EngineStatus,
    test: &SelfTestState,
    apply_error: Option<&str>,
    devices: usize,
    driver_source: Option<&Path>,
) -> String {
    let mut l = vec![
        format!("SoundRouter {}", env!("CARGO_PKG_VERSION")),
        format!("macOS: {os}"),
        format!(
            "driver plug-in: {}",
            status
                .driver_version
                .as_deref()
                .map_or("NOT FOUND".into(), |v| format!(
                    "found, protocol version {v}"
                ))
        ),
        format!(
            "driver bundle in app: {}",
            driver_source.map_or("missing".into(), |p| p.display().to_string())
        ),
        format!("TCC audio capture: {}", tcc_diag(status.tcc)),
        format!("TCC microphone: {}", tcc_diag(status.mic)),
        format!("virtual devices: {devices}"),
    ];
    for (id, s) in &status.per_device {
        l.push(format!("  {id}: {s:?}"));
    }
    l.push(format!(
        "self-test: {}",
        match test {
            SelfTestState::Idle => "not run".to_string(),
            SelfTestState::Running => "running".into(),
            SelfTestState::Done(Ok(t)) => format!(
                "{} ({:.1} dBFS, {} dropouts, {} Hz)",
                if t.passed { "passed" } else { "FAILED" },
                t.rms_dbfs,
                t.dropouts,
                t.rate
            ),
            SelfTestState::Done(Err(e)) => format!("error: {e}"),
        }
    ));
    if let Some(e) = apply_error {
        l.push(format!("apply error: {e}"));
    }
    if let Some(e) = &status.background_error {
        l.push(format!("background error: {e}"));
    }
    l.extend(status.warnings.iter().map(|w| format!("warning: {w}")));
    l.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NASTY: &[&str] = &[
        "/Applications/Sound Router.app/Contents/Resources/SoundRouter.driver",
        "/tmp/it's here/x",
        r#"/tmp/say "hi"/x"#,
        "/tmp/$HOME/`id`/$(id)/x",
        r"/tmp/back\slash\\/x",
        "/tmp/semi;colon && rm -rf ~ #/x",
        "/tmp/new\nline/x",
        "'; touch /tmp/pwned; '",
        "",
    ];

    fn sh_echo(arg: &str) -> String {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {arg}"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn sh_quote_roundtrips_through_a_real_shell() {
        for s in NASTY {
            assert_eq!(sh_echo(&sh_quote(s)), *s);
        }
    }

    #[test]
    fn applescript_string_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn install_script_quotes_the_source_and_never_splits_it() {
        for s in NASTY {
            let script = install_script(Path::new(s));
            assert!(script.contains(&format!("ditto {} ", sh_quote(s))), "{s}");
            // the whole script is one command list: only our own `&&` separators, no stray `;`
            // outside quotes (checked by the shell parsing it as exactly the expected words).
            let probe = script.replace("killall coreaudiod", "true");
            let words = Command::new("/bin/sh")
                .arg("-n")
                .arg("-c")
                .arg(&probe)
                .status()
                .unwrap();
            assert!(words.success(), "does not parse: {probe}");
        }
    }

    #[test]
    fn install_script_does_the_documented_steps() {
        let s = install_script(&PathBuf::from("/x/SoundRouter.driver"));
        for step in [
            "ditto '/x/SoundRouter.driver' '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'",
            "chown -R root:wheel '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'",
            "xattr -dr com.apple.quarantine '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'",
        ] {
            assert!(s.contains(step), "{step}\n{s}");
        }
        assert!(s.ends_with("killall coreaudiod"));
    }

    #[test]
    fn uninstall_removes_config_only_when_asked() {
        let keep = uninstall_script_for(None);
        assert_eq!(
            keep,
            "rm -rf '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver' && killall coreaudiod"
        );
        let dir = Path::new("/Users/a b/Library/Application Support/SoundRouter");
        let drop = uninstall_script_for(Some(dir));
        assert!(drop.contains("rm -rf '/Users/a b/Library/Application Support/SoundRouter'"));
        assert!(drop.ends_with("killall coreaudiod"));
        assert!(!uninstall_script(false).contains("Application Support"));
    }

    #[test]
    fn diagnostics_cover_driver_permissions_and_test() {
        let status = EngineStatus {
            driver_version: Some("1".into()),
            mic: TccStatus::Denied,
            ..EngineStatus::default()
        };
        let d = diagnostics(
            "Version 14.7",
            &status,
            &SelfTestState::Done(Err("boom".into())),
            Some("tap denied"),
            2,
            None,
        );
        for want in [
            "macOS: Version 14.7",
            "protocol version 1",
            "driver bundle in app: missing",
            "TCC microphone: Denied",
            "TCC audio capture: Not asked yet",
            "virtual devices: 2",
            "self-test: error: boom",
            "apply error: tap denied",
        ] {
            assert!(d.contains(want), "{want}\n{d}");
        }
        let none = diagnostics(
            "x",
            &EngineStatus::default(),
            &SelfTestState::Idle,
            None,
            0,
            None,
        );
        assert!(none.contains("NOT FOUND") && none.contains("not run"));
    }

    #[test]
    fn osascript_cancel_is_cancelled() {
        assert_eq!(classify(true, ""), Ok(()));
        assert_eq!(
            classify(false, "execution error: User canceled. (-128)\n"),
            Err(SetupError::Cancelled)
        );
        assert_eq!(
            classify(false, "boom (1)\n"),
            Err(SetupError::Failed("boom (1)".into()))
        );
    }
}
