//! Run in the background: menu-bar item, close-to-hide policy and the Dock-reopen hook.
//!
//! eframe 0.37 keeps calling `App::logic` while the window is hidden (no egui pass, no paint), and
//! the worker thread requests a repaint at 30 Hz, so menu events are polled from `logic`; the
//! menu handler also requests a repaint so a click never waits for the next worker tick.
use eframe::egui::Context;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

pub const HINT: &str = "SoundRouter is still running in the menu bar";
const HINT_FOR: Duration = Duration::from_secs(8);

// --- close-to-hide policy (pure) -------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WinState {
    /// The one-time "still running" hint was shown (now or in an earlier launch).
    pub hint_seen: bool,
    /// Quit was requested: the next close request must go through.
    pub quitting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Red close button / Cmd-W.
    CloseRequested,
    /// "Open SoundRouter" menu item.
    Open,
    /// Dock icon clicked.
    Reopen,
    /// "Quit" menu item.
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    /// Cancel the close and hide the window; `hint` = show the one-time hint.
    Hide {
        hint: bool,
    },
    Show,
    /// Really close, which ends the event loop and stops the engine.
    Quit,
}

pub fn step(s: WinState, e: Event) -> (WinState, Action) {
    match e {
        Event::CloseRequested if s.quitting => (s, Action::Nothing),
        Event::CloseRequested => (
            WinState {
                hint_seen: true,
                ..s
            },
            Action::Hide { hint: !s.hint_seen },
        ),
        Event::Open | Event::Reopen => (s, Action::Show),
        Event::Quit => (
            WinState {
                quitting: true,
                ..s
            },
            Action::Quit,
        ),
    }
}

fn hint_marker() -> Option<std::path::PathBuf> {
    Some(crate::store::default_path()?.with_file_name(".close-hint-seen"))
}

pub fn hint_seen() -> bool {
    hint_marker().is_some_and(|p| p.exists())
}

pub fn mark_hint_seen() {
    if let Some(p) = hint_marker() {
        let _ = std::fs::create_dir_all(p.parent().unwrap_or(&p));
        let _ = std::fs::write(p, "");
    }
}

// --- menu-bar item ---------------------------------------------------------------------------

pub enum Cmd {
    Open,
    Quit,
    /// New checked state of "Start at Login".
    Login(bool),
    /// A preset in the Presets submenu, by name.
    Preset(String),
    /// "Save current as…" in the Presets submenu.
    SavePreset,
}

pub struct Tray {
    icon: TrayIcon,
    status: MenuItem,
    status_text: String,
    login: CheckMenuItem,
    /// The Presets submenu, its rows, and the names they were built from.
    presets: Submenu,
    preset_rows: Vec<(MenuItem, String)>,
    save_preset_id: MenuId,
    preset_names: Vec<String>,
    open_id: MenuId,
    quit_id: MenuId,
    rx: Receiver<MenuId>,
    hint_until: Option<Instant>,
}

/// 18 pt monochrome "loop" glyph at 2x: a ring with a gap and an arrowhead (template image).
pub fn loop_glyph(n: u32) -> Vec<u8> {
    let c = n as f32 / 2.0;
    let (r_out, r_in) = (c * 0.78, c * 0.58);
    let r_mid = (r_out + r_in) / 2.0;
    let gap = (-70f32.to_radians(), -20f32.to_radians());
    // Arrowhead at the gap's start, pointing clockwise (y is down, so angle grows clockwise).
    let a = gap.0;
    let (dir, tan) = ((a.cos(), a.sin()), (-a.sin(), a.cos()));
    let at = |radial: f32, along: f32| {
        (
            c + dir.0 * radial + tan.0 * along,
            c + dir.1 * radial + tan.1 * along,
        )
    };
    let tri = [
        at(r_mid + c * 0.3, 0.0),
        at(r_mid - c * 0.3, 0.0),
        at(r_mid, c * 0.42),
    ];
    let inside_tri = |p: (f32, f32)| {
        let s =
            |a: (f32, f32), b: (f32, f32)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        let d = [s(tri[0], tri[1]), s(tri[1], tri[2]), s(tri[2], tri[0])];
        d.iter().all(|v| *v >= 0.0) || d.iter().all(|v| *v <= 0.0)
    };
    let covered = |x: f32, y: f32| {
        let (dx, dy) = (x - c, y - c);
        let r = dx.hypot(dy);
        let ang = dy.atan2(dx);
        ((r_in..=r_out).contains(&r) && !(gap.0..=gap.1).contains(&ang)) || inside_tri((x, y))
    };
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let hits = (0..9)
                .filter(|i| {
                    covered(
                        x as f32 + (i % 3) as f32 / 3.0 + 0.17,
                        y as f32 + (i / 3) as f32 / 3.0 + 0.17,
                    )
                })
                .count();
            px.extend([0, 0, 0, (hits * 255 / 9) as u8]);
        }
    }
    px
}

impl Tray {
    /// Must run on the main thread after the event loop started (inside the `run_native` creator).
    pub fn new(ctx: &Context, login_enabled: bool) -> Result<Tray, String> {
        let status = MenuItem::new("No devices running", false, None);
        let open = MenuItem::new("Open SoundRouter", true, None);
        let login = CheckMenuItem::new("Start at Login", true, login_enabled, None);
        let quit = MenuItem::new("Quit SoundRouter", true, None);
        let presets = Submenu::new("Presets", true);
        let save_preset = MenuItem::new("Save current as…", true, None);
        presets.append(&save_preset).map_err(|e| e.to_string())?;
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &status,
            &PredefinedMenuItem::separator(),
            &presets,
            &login,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| e.to_string())?;
        let (tx, rx) = channel();
        let repaint = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let _ = tx.send(e.id);
            repaint.request_repaint();
        }));
        const N: u32 = 36;
        let icon = Icon::from_rgba(loop_glyph(N), N, N).map_err(|e| e.to_string())?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon_templated(icon)
            .with_tooltip("SoundRouter")
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Tray {
            icon,
            status,
            status_text: String::new(),
            login,
            presets,
            preset_rows: Vec::new(),
            save_preset_id: save_preset.id().clone(),
            preset_names: Vec::new(),
            open_id: open.id().clone(),
            quit_id: quit.id().clone(),
            rx,
            hint_until: None,
        })
    }

    /// Commands from clicked menu items; also expires the hint title.
    pub fn poll(&mut self) -> Vec<Cmd> {
        if self.hint_until.is_some_and(|t| Instant::now() >= t) {
            self.hint_until = None;
            self.icon.set_title(None::<&str>);
        }
        self.rx
            .try_iter()
            .filter_map(|id| {
                if id == self.open_id {
                    Some(Cmd::Open)
                } else if id == self.quit_id {
                    Some(Cmd::Quit)
                } else if id == *self.login.id() {
                    Some(Cmd::Login(self.login.is_checked()))
                } else if id == self.save_preset_id {
                    Some(Cmd::SavePreset)
                } else {
                    self.preset_rows
                        .iter()
                        .find(|(row, _)| *row.id() == id)
                        .map(|(_, name)| Cmd::Preset(name.clone()))
                }
            })
            .collect()
    }

    pub fn set_status(&mut self, text: String) {
        if text != self.status_text {
            self.status.set_text(&text);
            self.status_text = text;
        }
    }

    /// Rows above "Save current as…", one per preset; rebuilt only when the names change.
    pub fn set_presets(&mut self, names: &[String]) {
        if self.preset_names == names {
            return;
        }
        for (row, _) in self.preset_rows.drain(..) {
            let _ = self.presets.remove(&row);
        }
        for (i, name) in names.iter().enumerate() {
            let row = MenuItem::new(menu_title(name), true, None);
            if self.presets.insert(&row, i).is_ok() {
                self.preset_rows.push((row, name.clone()));
            }
        }
        self.preset_names = names.to_vec();
    }

    pub fn set_login_checked(&self, on: bool) {
        self.login.set_checked(on);
    }

    /// Show `HINT` next to the menu-bar icon for a few seconds.
    pub fn hint(&mut self) {
        self.icon.set_title(Some(HINT));
        self.hint_until = Some(Instant::now() + HINT_FOR);
    }
}

/// muda treats a single `&` in a title as a mnemonic marker and drops it; `&&` shows one.
fn menu_title(name: &str) -> String {
    name.replace('&', "&&")
}

pub fn status_text(running: usize) -> String {
    match running {
        0 => "No devices running".into(),
        1 => "1 device running".into(),
        n => format!("{n} devices running"),
    }
}

// --- Dock icon click -------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub use dock::take_reopen;

#[cfg(not(target_os = "macos"))]
pub fn take_reopen() -> bool {
    false
}

#[cfg(target_os = "macos")]
mod dock {
    //! winit's application delegate has no `applicationShouldHandleReopen:hasVisibleWindows:`, so
    //! add it to that class at runtime; it flags the click and wakes the event loop.
    use eframe::egui::Context;
    use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
    use objc2::{Encode, ffi, sel};
    use std::ffi::CString;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    static REOPEN: AtomicBool = AtomicBool::new(false);
    static CTX: OnceLock<Context> = OnceLock::new();

    pub fn take_reopen() -> bool {
        REOPEN.swap(false, Ordering::AcqRel)
    }

    extern "C-unwind" fn should_handle_reopen(
        _this: *mut AnyObject,
        _sel: Sel,
        _app: *mut AnyObject,
        _has_visible_windows: Bool,
    ) -> Bool {
        REOPEN.store(true, Ordering::Release);
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Bool::NO // we show the window ourselves
    }

    /// Call once, after the event loop (and so the delegate class) exists.
    pub fn install_reopen_hook(ctx: &Context) -> bool {
        let _ = CTX.set(ctx.clone());
        let Some(cls) = AnyClass::get(c"WinitApplicationDelegate") else {
            return false;
        };
        let types = CString::new(format!("{}@:@{}", Bool::ENCODING, Bool::ENCODING))
            .expect("encoding has no NUL");
        // SAFETY: the IMP's signature matches the selector's type encoding above.
        unsafe {
            let imp: objc2::runtime::Imp = std::mem::transmute(
                should_handle_reopen
                    as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool,
            );
            ffi::class_addMethod(
                std::ptr::from_ref(cls).cast_mut(),
                sel!(applicationShouldHandleReopen:hasVisibleWindows:),
                imp,
                types.as_ptr(),
            )
            .as_bool()
        }
    }
}

#[cfg(target_os = "macos")]
pub use dock::install_reopen_hook;

#[cfg(not(target_os = "macos"))]
pub fn install_reopen_hook(_: &Context) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> WinState {
        WinState::default()
    }

    #[test]
    fn menu_titles_keep_their_ampersands() {
        assert_eq!(menu_title("Rock & Roll"), "Rock && Roll");
        assert_eq!(menu_title("Plain"), "Plain");
    }

    #[test]
    fn first_close_hides_with_hint_later_closes_hide_silently() {
        let (s, a) = step(fresh(), Event::CloseRequested);
        assert_eq!(a, Action::Hide { hint: true });
        let (s, a) = step(s, Event::CloseRequested);
        assert_eq!(a, Action::Hide { hint: false });
        assert!(s.hint_seen && !s.quitting);
    }

    #[test]
    fn hint_seen_in_an_earlier_launch_is_not_repeated() {
        let s = WinState {
            hint_seen: true,
            quitting: false,
        };
        assert_eq!(
            step(s, Event::CloseRequested).1,
            Action::Hide { hint: false }
        );
    }

    #[test]
    fn open_and_dock_click_show_the_window_and_change_nothing() {
        for e in [Event::Open, Event::Reopen] {
            assert_eq!(step(fresh(), e), (fresh(), Action::Show));
        }
    }

    #[test]
    fn quit_closes_and_the_resulting_close_request_is_let_through() {
        let (s, a) = step(fresh(), Event::Quit);
        assert_eq!(a, Action::Quit);
        assert_eq!(step(s, Event::CloseRequested).1, Action::Nothing);
    }

    #[test]
    fn glyph_is_an_opaque_black_ring_with_a_clear_middle() {
        let n = 36usize;
        let g = loop_glyph(n as u32);
        assert_eq!(g.len(), n * n * 4);
        assert!(g.chunks(4).all(|p| p[..3] == [0, 0, 0]));
        let alpha = |x: usize, y: usize| g[(y * n + x) * 4 + 3];
        assert_eq!(alpha(n / 2, n / 2), 0);
        assert!(g.chunks(4).filter(|p| p[3] == 255).count() > 100);
        assert_eq!(alpha(0, 0), 0);
    }

    #[test]
    fn status_line_pluralizes() {
        assert_eq!(status_text(0), "No devices running");
        assert_eq!(status_text(1), "1 device running");
        assert_eq!(status_text(2), "2 devices running");
    }
}
