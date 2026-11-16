//! SoundRouter: Loopback-style virtual audio device router.
mod app;
mod fake;
mod graph;
mod icons;
mod instance;
mod layout;
mod logging;
mod login;
mod meter;
mod model;
mod setup;
mod store;
mod theme;
mod tray;
mod widgets;
mod worker;

use eframe::egui;
use sr_core::backend::Backend;

struct Args {
    demo: bool,
    /// Start with the window hidden (the Start-at-Login agent passes this).
    background: bool,
    help: bool,
    theme: egui::ThemePreference,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Args {
    let mut a = Args {
        demo: false,
        background: false,
        help: false,
        theme: egui::ThemePreference::System,
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--background" => a.background = true,
            "--theme" => {
                a.theme = match it.next().as_deref() {
                    Some("dark") => egui::ThemePreference::Dark,
                    Some("light") => egui::ThemePreference::Light,
                    _ => egui::ThemePreference::System,
                }
            }
            "-h" | "--help" => a.help = true,
            other => eprintln!("ignoring unknown argument {other}"),
        }
    }
    a
}

/// The CoreAudio engine; swapped for the fake backend only under `--demo`.
fn real_backend() -> anyhow::Result<Box<dyn Backend + Send>> {
    Ok(Box::new(sr_audio::Engine::new()?))
}

fn main() -> eframe::Result {
    let args = parse_args(std::env::args().skip(1));
    if args.help {
        println!("usage: SoundRouter [--demo] [--background] [--theme dark|light]");
        return Ok(());
    }
    // Held until process exit.
    let _instance = if instance::bypasses_guard(args.demo) {
        None
    } else {
        match instance::lock_path().map(|p| instance::acquire(&p)) {
            Some(Ok(None)) => {
                instance::raise_existing();
                return Ok(());
            }
            Some(Ok(Some(g))) => Some(g),
            // Guard unavailable: run anyway rather than refuse to start.
            _ => None,
        }
    };
    let log_file = logging::init();
    logging::banner(log_file.as_deref());
    let demo = args.demo;
    let vibrancy_wanted = cfg!(target_os = "macos");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SoundRouter")
            .with_inner_size([1180.0, 700.0])
            .with_min_inner_size([1000.0, 520.0])
            .with_transparent(vibrancy_wanted)
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        ..Default::default()
    };
    eframe::run_native(
        "SoundRouter",
        opts,
        Box::new(move |cc| {
            theme::install_fonts(&cc.egui_ctx);
            cc.egui_ctx.set_theme(args.theme);
            #[cfg(target_os = "macos")]
            let vibrancy = vibrancy_wanted
                && window_vibrancy::apply_vibrancy(
                    cc,
                    window_vibrancy::NSVisualEffectMaterial::Sidebar,
                    None,
                    None,
                )
                .is_ok()
                && blur_under_content(cc);
            #[cfg(not(target_os = "macos"))]
            let vibrancy = false;

            let (cfg, path, toast, backend): (_, _, _, Box<dyn Backend + Send>) = if demo {
                (
                    fake::demo_config(),
                    None,
                    None,
                    Box::new(fake::FakeBackend::new()),
                )
            } else {
                let path = store::default_path();
                let (cfg, msg) = path
                    .as_deref()
                    .map_or_else(|| (model::default_config(), None), store::load);
                match real_backend() {
                    Ok(b) => (cfg, path, msg, b),
                    Err(e) => (
                        cfg,
                        path,
                        Some(format!("Audio engine unavailable: {e:#}")),
                        Box::new(fake::FakeBackend::new()),
                    ),
                }
            };
            let worker = worker::Worker::spawn(backend, cc.egui_ctx.clone());
            if login::is_enabled() {
                // The agent plist points at the executable that enabled it; follow the app if it moved.
                let _ = login::set_enabled(true);
            }
            let tray = tray::Tray::new(&cc.egui_ctx, login::is_enabled())
                .inspect_err(|e| log::warn!("menu-bar item unavailable: {e}"))
                .ok();
            if !tray::install_reopen_hook(&cc.egui_ctx) {
                log::warn!("Dock-click reopen hook not installed");
            }
            let win = tray::WinState {
                hint_seen: demo || tray::hint_seen(),
                quitting: false,
            };
            Ok(Box::new(
                app::App::new(cfg, path, worker, toast, vibrancy, demo).background(
                    tray,
                    win,
                    args.background,
                ),
            ))
        }),
    )
}

/// window-vibrancy puts its blur view inside winit's view, but wgpu draws into a `CAMetalLayer`
/// that is a plain sublayer of that view, and AppKit appends the blur view's layer after it when
/// the window first shows: the blur then covers every frame and the window looks blank. A
/// negative `zPosition` keeps the blur underneath; if that fails the blur is removed again.
#[cfg(target_os = "macos")]
fn blur_under_content(cc: &eframe::CreationContext<'_>) -> bool {
    use objc2::{ClassType, runtime::NSObjectProtocol};
    use objc2_app_kit::{NSView, NSVisualEffectView};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let mut sunk = false;
    if let Ok(RawWindowHandle::AppKit(h)) = cc.window_handle().map(|h| h.as_raw()) {
        // SAFETY: winit's AppKit handle is its live content view; the creator runs on the main thread.
        let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
        for blur in view.subviews().to_vec() {
            if blur.isKindOfClass(NSVisualEffectView::class()) {
                blur.setWantsLayer(true);
                if let Some(layer) = blur.layer() {
                    layer.setZPosition(-1.0);
                    sunk = true;
                }
            }
        }
    }
    if !sunk {
        log::warn!("vibrancy off: could not put the blur view under the drawing layer");
        let _ = window_vibrancy::clear_vibrancy(cc);
    }
    sunk
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(a: &[&str]) -> Args {
        parse_args(a.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_is_a_normal_launch() {
        let a = parse(&[]);
        assert!(!a.demo && !a.background && !a.help);
        assert_eq!(a.theme, egui::ThemePreference::System);
    }

    #[test]
    fn background_and_demo_flags() {
        let a = parse(&["--background", "--demo"]);
        assert!(a.background && a.demo);
    }

    #[test]
    fn unknown_arguments_are_ignored() {
        let a = parse(&["-psn_0_12345", "--frobnicate", "--background"]);
        assert!(a.background && !a.demo);
    }

    #[test]
    fn theme_takes_a_value() {
        assert_eq!(
            parse(&["--theme", "dark"]).theme,
            egui::ThemePreference::Dark
        );
        assert_eq!(
            parse(&["--theme", "light"]).theme,
            egui::ThemePreference::Light
        );
        assert_eq!(
            parse(&["--theme", "x"]).theme,
            egui::ThemePreference::System
        );
    }
}
