//! SoundRouter: Loopback-style virtual audio device router.
mod app;
mod fake;
mod graph;
mod layout;
mod meter;
mod model;
mod store;
mod theme;
mod widgets;
mod worker;

use eframe::egui;
use sr_core::backend::Backend;
use std::path::PathBuf;

struct Args {
    demo: bool,
    screenshot: Option<PathBuf>,
    theme: egui::ThemePreference,
}

fn parse_args() -> Args {
    let mut a = Args {
        demo: false,
        screenshot: None,
        theme: egui::ThemePreference::System,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--screenshot" => a.screenshot = it.next().map(PathBuf::from),
            "--theme" => {
                a.theme = match it.next().as_deref() {
                    Some("dark") => egui::ThemePreference::Dark,
                    Some("light") => egui::ThemePreference::Light,
                    _ => egui::ThemePreference::System,
                }
            }
            "-h" | "--help" => {
                println!("usage: SoundRouter [--demo] [--screenshot out.png] [--theme dark|light]");
                std::process::exit(0);
            }
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
    let args = parse_args();
    // A screenshot always renders the populated demo.
    let demo = args.demo || args.screenshot.is_some();
    let vibrancy_wanted = cfg!(target_os = "macos") && args.screenshot.is_none();
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
                .is_ok();
            #[cfg(not(target_os = "macos"))]
            let vibrancy = false;

            let (cfg, path, mut toast, backend): (_, _, _, Box<dyn Backend + Send>) = if demo {
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
            let shot = args.screenshot.clone().map(app::Shot::new);
            toast = toast.take();
            Ok(Box::new(app::App::new(
                cfg, path, worker, toast, vibrancy, demo, shot,
            )))
        }),
    )
}
