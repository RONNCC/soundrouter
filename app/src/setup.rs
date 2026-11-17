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
/// Where AUDIO_CAPTURE_SETTINGS lands (checked on macOS 14.7.1) and the switch to turn on there.
const CAPTURE_DENIED: &str = "Turn on SoundRouter in System Settings › Privacy & Security › Screen & System Audio Recording › System Audio Recording Only. macOS may ask you to Quit & Reopen SoundRouter.";
/// How long a System Audio Recording request waits for the user to answer the prompt.
const CAPTURE_PROMPT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
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

/// Asks macOS for System Audio Recording and returns the answer. Injected so tests never prompt.
pub type Asker = Arc<dyn Fn() -> TccStatus + Send + Sync>;

/// What decides whether the setup sheet opens by itself (it always opens on "Test Setup…").
#[derive(Clone, Copy, Debug)]
pub struct Needs {
    pub driver: bool,
    pub mic: TccStatus,
    pub capture: TccStatus,
    /// An enabled device: the engine reads its input, which takes the Microphone permission.
    pub mic_source: bool,
    /// An enabled device has an app source, which takes System Audio Recording.
    pub app_source: bool,
    /// The sheet was finished or dismissed in this version.
    pub seen: bool,
}

impl Needs {
    /// Hold the first apply so the sheet explains the Microphone prompt before macOS shows it.
    /// Once the sheet was seen, macOS asks on its own (its prompt carries our usage text).
    pub fn hold_first_apply(&self) -> bool {
        self.driver && self.mic_source && self.mic == TccStatus::Unknown && !self.seen
    }
}

/// Open the sheet only when something is missing that the app needs, and never again in this
/// version once the user finished or dismissed it, except for a missing driver (nothing works).
/// A System Audio Recording question still open is not missing: macOS asks when an app source starts.
pub fn should_show_setup(n: &Needs) -> bool {
    !n.driver
        || (!n.seen
            && ((n.mic_source && n.mic != TccStatus::Granted)
                || (n.app_source && n.capture == TccStatus::Denied)))
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Marker next to the config: the version whose setup sheet was finished or dismissed.
fn seen_marker() -> Option<PathBuf> {
    Some(crate::store::default_path()?.with_file_name(".setup-seen"))
}

fn seen_at(p: &Path) -> bool {
    std::fs::read_to_string(p).is_ok_and(|v| v == VERSION)
}

fn mark_seen_at(p: &Path) {
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(p));
    let _ = std::fs::write(p, VERSION);
}

/// Run `job` off the UI thread (inline in tests, so the answer is there by the next frame) and
/// repaint when it is done.
fn off_ui<T: Send + 'static>(
    ctx: &Context,
    job: impl FnOnce() -> T + Send + 'static,
) -> Receiver<T> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let job = move || {
        let _ = tx.send(job());
        ctx.request_repaint();
    };
    if cfg!(test) {
        job();
    } else {
        std::thread::spawn(job);
    }
    rx
}

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
    /// otherwise appear unexplained (`Needs::hold_first_apply`).
    pub first_apply_pending: bool,
    /// `should_show_setup` said yes.
    pub missing: bool,
    pub now: f64,
    pub diagnostics: &'a dyn Fn() -> String,
}

pub struct Setup {
    runner: Runner,
    asker: Asker,
    /// The driver bundle to install; `None` when this build ships without one.
    source: Option<PathBuf>,
    step: Step,
    /// Opened by "Test Setup…", after an install, or when `should_show_setup` says so.
    open: bool,
    busy: Option<Busy>,
    rx: Option<Receiver<Result<(), SetupError>>>,
    /// A System Audio Recording request in flight.
    asking: Option<Receiver<TccStatus>>,
    /// Its answer, shown until the next status poll knows it too.
    answer: Option<TccStatus>,
    /// Continue was pressed: ask for System Audio Recording once the Microphone question is settled.
    ask_after_mic: bool,
    /// The Microphone request is in flight (the self-test's prompt is up).
    mic_asking: bool,
    /// A request from a row button, handed to the app at the end of `sheet`.
    queued: Option<Action>,
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
    /// Finished or dismissed in this version (`Needs::seen`).
    seen: bool,
    /// Where `seen` is remembered; `None` in tests.
    marker: Option<PathBuf>,
}

impl Setup {
    pub fn new(runner: Runner, source: Option<PathBuf>) -> Self {
        Self {
            runner,
            asker: Arc::new(|| TccStatus::Unknown),
            source,
            step: Step::Welcome,
            open: false,
            busy: None,
            rx: None,
            asking: None,
            answer: None,
            ask_after_mic: false,
            mic_asking: false,
            queued: None,
            error: None,
            deadline: None,
            confirm: None,
            copied: false,
            demo: false,
            shown: false,
            test_since: None,
            seen: false,
            marker: None,
        }
    }

    /// The real thing: the administrator prompt, the System Audio Recording prompt, the bundled
    /// driver, and the "setup seen" marker.
    pub fn system() -> Self {
        let marker = seen_marker();
        Self {
            seen: marker.as_deref().is_some_and(seen_at),
            marker,
            ..Self::new(Arc::new(run_admin), locate_driver())
        }
        .with_asker(Arc::new(|| {
            sr_audio::tcc::request_audio_capture(CAPTURE_PROMPT_WAIT).into()
        }))
    }

    pub fn with_asker(mut self, asker: Asker) -> Self {
        self.asker = asker;
        self
    }

    /// Finished or dismissed in this version.
    pub fn seen(&self) -> bool {
        self.seen
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
        let runner = self.runner.clone();
        self.rx = Some(off_ui(ctx, move || runner(&script)));
        self.busy = Some(busy);
        self.error = None;
    }

    /// Show macOS's System Audio Recording prompt (an immediate answer once it was decided).
    fn ask_capture(&mut self, ctx: &Context) {
        let asker = self.asker.clone();
        self.asking = Some(off_ui(ctx, move || asker()));
    }

    /// System Audio Recording as the sheet shows it: the status poll, or the request's answer
    /// while the poll has not caught up.
    fn capture(&self, v: &View) -> TccStatus {
        match (v.status.tcc, self.answer) {
            (TccStatus::Unknown, Some(a)) => a,
            (s, _) => s,
        }
    }

    /// The row's button: ask macOS, or once it said no, its Settings pane.
    fn capture_action(&mut self, ctx: &Context, v: &View) {
        if self.capture(v) == TccStatus::Denied {
            open_url(AUDIO_CAPTURE_SETTINGS);
        } else {
            self.ask_capture(ctx);
        }
    }

    /// The Microphone row's button: Settings once it said no; else the request Continue makes
    /// for it (the self-test asks), alone.
    fn mic_action(&mut self, v: &View) {
        if v.status.mic == TccStatus::Denied {
            open_url(MIC_SETTINGS);
        } else {
            self.queued = self.continue_to_verify(Action::AllowMic);
            self.ask_after_mic = false;
        }
    }

    /// Continue: the self-test asks for the Microphone; System Audio Recording follows once that
    /// question is settled, so the two prompts never overlap.
    fn continue_to_verify(&mut self, action: Action) -> Option<Action> {
        self.step = Step::Verify;
        self.ask_after_mic = true;
        self.mic_asking = action == Action::AllowMic;
        Some(action)
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
        if let Some(a) = self.asking.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.asking = None;
            self.answer = Some(a);
        }
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
            if self.deadline.take().is_some() || v.missing {
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
        if v.status.mic != TccStatus::Unknown {
            self.mic_asking = false;
        }
        if self.ask_after_mic && v.status.mic != TccStatus::Unknown && self.asking.is_none() {
            self.ask_after_mic = false;
            if self.capture(v) == TccStatus::Unknown {
                self.ask_capture(ctx);
            }
        }
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
        self.ask_after_mic = false;
        self.mic_asking = false;
        self.seen = true;
        if let Some(p) = &self.marker {
            mark_seen_at(p);
        }
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
                let cap = self.capture(v);
                let go = match (mic == TccStatus::Unknown, cap == TccStatus::Unknown) {
                    (true, true) => "Allow All",
                    (true, false) => "Allow Microphone",
                    (false, true) => "Allow System Audio Recording",
                    (false, false) => "Continue",
                };
                modal(
                    ctx,
                    t,
                    &Sheet {
                        id: "setup_permissions",
                        buttons: true,
                        glyph: Glyph::App,
                        title: "Allow access",
                        intro: "SoundRouter needs these permissions to hear your mic and capture app audio.",
                    },
                    self,
                    |ui, s| {
                        let rows = s.permission_rows(v, true);
                        match checklist(ui, t, &rows) {
                            Some(0) => s.mic_action(v),
                            Some(1) => s.capture_action(ctx, v),
                            _ => {}
                        }
                    },
                    |ui, s| {
                        buttons(
                            ui,
                            s,
                            |_, _| {},
                            |ui, s| {
                                if button(ui, t, go, Kind::Primary) {
                                    action = s.continue_to_verify(if mic == TccStatus::Unknown {
                                        Action::AllowMic
                                    } else {
                                        Action::Test
                                    });
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
                        let mut rows = s.permission_rows(v, false).to_vec();
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
                        match checklist(ui, t, &rows) {
                            Some(1) => s.mic_action(v),
                            Some(2) => s.capture_action(ctx, v),
                            _ => {}
                        }
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
        action.or(self.queued.take())
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

impl Setup {
    /// Microphone and System Audio Recording as checklist rows (a row's button asks macOS, or opens
    /// its Settings pane once it said no). `explain` adds the why.
    fn permission_rows<'a>(&self, v: &View, explain: bool) -> [Row<'a>; 2] {
        let look = |s: TccStatus| match s {
            TccStatus::Granted => Status::Ok,
            TccStatus::Denied => Status::Fail,
            TccStatus::Unknown => Status::Pending,
        };
        let (mic, cap) = (v.status.mic, self.capture(v));
        let capture = if self.asking.is_some() {
            Row {
                state: Status::Running,
                title: "System Audio Recording",
                status: "Asking…",
                detail: "Answer the macOS prompt.",
                action: None,
            }
        } else {
            Row {
                state: look(cap),
                title: "System Audio Recording",
                status: tcc_word(cap),
                detail: match cap {
                    TccStatus::Denied => CAPTURE_DENIED,
                    _ if explain => "Captures audio from apps you add as sources.",
                    _ => "",
                },
                action: match cap {
                    TccStatus::Granted => None,
                    TccStatus::Denied => Some("Open Settings"),
                    TccStatus::Unknown => Some("Allow System Audio Recording"),
                },
            }
        };
        [
            Row {
                state: if self.mic_asking && mic == TccStatus::Unknown {
                    Status::Running
                } else {
                    look(mic)
                },
                title: "Microphone",
                status: if self.mic_asking && mic == TccStatus::Unknown {
                    "Asking…"
                } else {
                    tcc_word(mic)
                },
                detail: if explain {
                    "Reads the virtual device's input to check that audio flows, and any microphone or input device you add as a source."
                } else {
                    ""
                },
                action: match mic {
                    TccStatus::Unknown if !self.mic_asking => Some("Allow Microphone"),
                    TccStatus::Denied => Some("Open Settings"),
                    _ => None,
                },
            },
            capture,
        ]
    }
}

/// For support text: the status as the user sees it when it is not decided, else its name.
fn tcc_diag(s: TccStatus) -> String {
    match s {
        TccStatus::Unknown => "Needs permission".into(),
        _ => format!("{s:?}"),
    }
}

fn tcc_word(s: TccStatus) -> &'static str {
    match s {
        TccStatus::Granted => "Allowed",
        TccStatus::Denied => "Denied",
        TccStatus::Unknown => "Needs permission",
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
            "TCC audio capture: Needs permission",
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

    #[test]
    fn the_sheet_opens_only_for_something_missing_and_not_again_once_seen() {
        use TccStatus::{Denied as D, Granted as G, Unknown as U};
        // (driver, mic, capture, mic source, app source, seen) -> show, hold first apply
        #[rustfmt::skip]
        let table = [
            (false, G, G, true,  true,  true,  true,  false), // no driver: always
            (false, U, U, false, false, false, true,  false),
            (true,  G, G, true,  true,  false, false, false), // all granted
            (true,  G, U, true,  true,  false, false, false), // capture not asked: macOS asks at the tap
            (true,  G, U, true,  false, false, false, false),
            (true,  U, U, true,  false, false, true,  true ), // explain the Microphone prompt first
            (true,  U, U, true,  false, true,  false, false), // seen: macOS asks on its own
            (true,  U, U, false, false, false, false, false), // nothing reads an input
            (true,  D, G, true,  false, false, true,  false),
            (true,  D, G, true,  false, true,  false, false), // dismissed: no nagging
            (true,  D, G, false, false, false, false, false),
            (true,  G, D, true,  true,  false, true,  false), // capture denied with an app source
            (true,  G, D, true,  false, false, false, false), // ... without one: not needed
            (true,  G, D, true,  true,  true,  false, false),
        ];
        for (driver, mic, capture, mic_source, app_source, seen, show, hold) in table {
            let n = Needs {
                driver,
                mic,
                capture,
                mic_source,
                app_source,
                seen,
            };
            assert_eq!(should_show_setup(&n), show, "{n:?}");
            assert_eq!(n.hold_first_apply(), hold, "{n:?}");
        }
    }

    #[test]
    fn seen_is_per_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SoundRouter/.setup-seen");
        assert!(!seen_at(&p));
        mark_seen_at(&p);
        assert!(seen_at(&p));
        std::fs::write(&p, "0.0.0-older").unwrap();
        assert!(!seen_at(&p));
    }
}
