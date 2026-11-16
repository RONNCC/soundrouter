//! First-run setup: driver install/uninstall through the macOS administrator prompt, the
//! permission explanations, the loopback self-test and the sheet that walks through them.
use crate::theme::Tokens;
use crate::widgets::name_button;
use crate::worker::SelfTestState;
use eframe::egui::{
    self, Align2, Color32, Context, CornerRadius, FontId, Id, Modal, Rect, RichText, Sense, Stroke,
};
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
        std::thread::spawn(move || {
            let _ = tx.send(runner(&script));
            ctx.request_repaint();
        });
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

    fn poll(&mut self, now: f64) {
        let Some(r) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.rx = None;
        match (self.busy.take(), r) {
            (Some(Busy::Installing), Ok(())) => self.deadline = Some(now + DRIVER_WAIT_SECS),
            (Some(Busy::Uninstalling), Ok(())) => self.confirm = None,
            (_, Err(SetupError::Cancelled)) => self.error = Some("Cancelled.".into()),
            (_, Err(SetupError::Failed(m))) => self.error = Some(m),
            (None, Ok(())) => {}
        }
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
        self.poll(v.now);
        self.shown = false;
        if self.demo || !v.ready {
            return None;
        }
        self.sync(v);
        let mut action = None;
        if self.open || v.status.driver_version.is_none() {
            self.shown = true;
            Modal::new(Id::unique("setup_sheet")).show(ctx, |ui| {
                ui.set_width(420.0);
                action = self.sheet(ui, v);
            });
        }
        if self.confirm.is_some() {
            Modal::new(Id::unique("uninstall_dialog")).show(ctx, |ui| {
                ui.set_width(420.0);
                self.uninstall_dialog(ui);
            });
        }
        action
    }

    fn close(&mut self) {
        self.open = false;
        self.step = Step::Welcome;
    }

    fn sheet(&mut self, ui: &mut egui::Ui, v: &View) -> Option<Action> {
        let mut action = None;
        let ctx = ui.ctx().clone();
        let heading = |ui: &mut egui::Ui, t: &str| ui.heading(t);
        match self.step {
            Step::Welcome => {
                heading(ui, "Set up SoundRouter");
                ui.label(
                    "SoundRouter creates virtual audio devices and routes app and microphone audio between them. \
                     It needs a small system audio driver; macOS will ask for your password to install it.",
                );
                ui.horizontal(|ui| {
                    if primary(ui, "Install Driver…") {
                        self.start_install(&ctx);
                    }
                    if ui.button("Try Demo").clicked() {
                        self.demo = true;
                        action = Some(Action::Demo);
                    }
                });
            }
            Step::Driver => {
                heading(ui, "Install the audio driver");
                if self.busy == Some(Busy::Installing) {
                    ui.label("Waiting for the password dialog…");
                } else if self.deadline.is_some() {
                    ui.label("Starting the driver…");
                } else {
                    ui.colored_label(ui.visuals().error_fg_color, "Driver not installed");
                    if let Some(e) = &self.error {
                        ui.label(e);
                    }
                    ui.horizontal(|ui| {
                        if primary(ui, "Retry") {
                            self.start_install(&ctx);
                        }
                        if ui.button("Try Demo").clicked() {
                            self.demo = true;
                            action = Some(Action::Demo);
                        }
                        self.copy_button(ui, v);
                    });
                }
            }
            Step::Permissions => {
                heading(ui, "Allow access");
                let (mic, cap) = (v.status.mic, v.status.tcc);
                ui.label(
                    "Microphone: SoundRouter reads the virtual device's input to check that audio flows. \
                     macOS asks the first time.",
                );
                ui.label(format!("Microphone: {}", tcc_word(mic)));
                ui.label(
                    "System Audio Recording: needed to capture audio from apps you add as sources. \
                     macOS asks when the first app source starts.",
                );
                ui.label(format!("System Audio Recording: {}", tcc_word(cap)));
                ui.horizontal(|ui| {
                    match mic {
                        TccStatus::Granted => {
                            if primary(ui, "Continue") {
                                self.step = Step::Verify;
                                action = Some(Action::Test);
                            }
                        }
                        TccStatus::Unknown => {
                            if primary(ui, "Allow Microphone") {
                                self.step = Step::Verify;
                                action = Some(Action::AllowMic);
                            }
                        }
                        TccStatus::Denied => {
                            if primary(ui, "Open Microphone Settings") {
                                open_url(MIC_SETTINGS);
                            }
                            if ui.button("Continue").clicked() {
                                self.step = Step::Verify;
                                action = Some(Action::Test);
                            }
                        }
                    }
                    if cap != TccStatus::Granted && ui.button("Open Settings").clicked() {
                        open_url(AUDIO_CAPTURE_SETTINGS);
                    }
                    if !v.first_apply_pending && ui.button("Close").clicked() {
                        self.close();
                    }
                });
            }
            Step::Verify => {
                heading(ui, "Check the audio path");
                match v.test {
                    SelfTestState::Idle | SelfTestState::Running => {
                        ui.label("Playing a quiet tone through the virtual device…");
                    }
                    SelfTestState::Done(Ok(t)) if t.passed => {
                        ui.label(format!(
                            "Audio flows ({:.0} dBFS, {} dropouts)",
                            t.rms_dbfs, t.dropouts
                        ));
                        if primary(ui, "Continue") {
                            self.step = Step::Done;
                        }
                    }
                    SelfTestState::Done(r) => {
                        let why = match r {
                            Ok(t) => format!(
                                "No clean audio came back ({:.0} dBFS, {} dropouts).",
                                t.rms_dbfs, t.dropouts
                            ),
                            Err(e) => format!("The test failed: {e}"),
                        };
                        ui.colored_label(ui.visuals().error_fg_color, why);
                        ui.label("Check the permissions above, then try again.");
                        ui.horizontal(|ui| {
                            if primary(ui, "Retry") {
                                action = Some(Action::Test);
                            }
                            self.copy_button(ui, v);
                            if ui.button("Close").clicked() {
                                self.close();
                            }
                        });
                    }
                }
            }
            Step::Done => {
                heading(ui, "You're all set");
                ui.label(format!(
                    "In Zoom, OBS or any other app, choose “{}” as the microphone or speaker.",
                    v.device_name
                ));
                if primary(ui, "Done") {
                    self.close();
                }
            }
        }
        action
    }

    fn copy_button(&mut self, ui: &mut egui::Ui, v: &View) {
        if ui.button("Copy diagnostics").clicked() {
            ui.ctx().copy_text((v.diagnostics)());
            self.copied = true;
        }
        if self.copied {
            ui.label("Copied to clipboard");
        }
    }

    fn uninstall_dialog(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.heading("Uninstall the audio driver?");
        ui.label(format!(
            "This removes SoundRouter.driver from {HAL_DIR} and briefly restarts macOS audio. \
             macOS will ask for your password."
        ));
        let busy = self.busy == Some(Busy::Uninstalling);
        if let Some(remove) = &mut self.confirm {
            ui.add_enabled(
                !busy,
                egui::Checkbox::new(remove, "Also delete my SoundRouter settings"),
            );
        }
        if busy {
            ui.label("Waiting for the password dialog…");
        } else {
            if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, format!("Not uninstalled: {e}"));
            }
            ui.horizontal(|ui| {
                if primary(ui, "Uninstall") {
                    let script = uninstall_script(self.confirm == Some(true));
                    self.spawn(&ctx, Busy::Uninstalling, script);
                }
                if ui.button("Cancel").clicked() {
                    self.confirm = None;
                    self.error = None;
                }
            });
        }
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

fn primary(ui: &mut egui::Ui, label: &str) -> bool {
    let b = egui::Button::new(RichText::new(label).color(Color32::WHITE))
        .fill(ui.visuals().selection.bg_fill);
    ui.add(b).clicked()
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
        format!("TCC audio capture: {:?}", status.tcc),
        format!("TCC microphone: {:?}", status.mic),
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
