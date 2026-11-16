//! SoundRouter: Loopback-style virtual audio device router.
mod app;
mod fake;
mod graph;
mod icons;
mod layout;
mod logging;
mod meter;
mod model;
mod setup;
mod store;
mod theme;
mod widgets;
mod worker;

use eframe::egui;
use sr_core::backend::Backend;

struct Args {
    demo: bool,
    theme: egui::ThemePreference,
}

fn parse_args() -> Args {
    let mut a = Args {
        demo: false,
        theme: egui::ThemePreference::System,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => a.demo = true,
            "--theme" => {
                a.theme = match it.next().as_deref() {
                    Some("dark") => egui::ThemePreference::Dark,
                    Some("light") => egui::ThemePreference::Light,
                    _ => egui::ThemePreference::System,
                }
            }
            "-h" | "--help" => {
                println!("usage: SoundRouter [--demo] [--theme dark|light]");
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
                .is_ok();
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
            Ok(Box::new(app::App::new(
                cfg, path, worker, toast, vibrancy, demo,
            )))
        }),
    )
}
