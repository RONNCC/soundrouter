use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sr_audio::{Engine, IoState, apps, driver, loopback, tcc};
use sr_core::backend::Backend;
use sr_core::config::Config;
use sr_core::driver_proto::{DEVICE_UID_PREFIX, DriverDevice, PLUGIN_BUNDLE_ID, companion_uid};

#[derive(Parser)]
#[command(name = "sr-cli", version, about = "SoundRouter headless tool")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List hardware devices and audio apps (grouped by responsible app)
    List,
    /// Read or replace the driver's virtual device list
    Driver {
        #[command(subcommand)]
        op: DriverOp,
    },
    /// Play a 1 kHz tone into a virtual device and measure what comes back on its input
    LoopbackTest {
        /// Device uid, or a short id like `v1`
        uid: String,
        #[arg(long, default_value_t = 5)]
        secs: u64,
    },
    /// Apply a config and print meters at 4 Hz until Ctrl-C
    Run { config: PathBuf },
    /// Print per-channel input peaks of a virtual device (what apps record from it) at 4 Hz
    Meter { device_id: String },
    /// Check driver, plug-in, TCC and system state
    Doctor,
}

#[derive(Subcommand)]
enum DriverOp {
    Get,
    /// JSON file: [{"uid":"soundrouter:v1","name":"Loopback","channels":2}]
    Set {
        json: PathBuf,
    },
}

static STOP: AtomicBool = AtomicBool::new(false);
fn install_ctrlc() -> Result<()> {
    Ok(ctrlc::set_handler(|| STOP.store(true, Relaxed))?)
}

fn full_uid(s: &str) -> String {
    if s.contains(':') {
        s.to_owned()
    } else {
        format!("{DEVICE_UID_PREFIX}{s}")
    }
}

fn db(x: f32) -> String {
    if x <= 1e-6 {
        "  -inf".into()
    } else {
        format!("{:6.1}", 20.0 * x.log10())
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::List => list(),
        Cmd::Driver { op: DriverOp::Get } => {
            let list = driver::get_devices(driver::require_plugin()?)?;
            println!("{}", serde_json::to_string_pretty(&list)?);
            Ok(())
        }
        Cmd::Driver {
            op: DriverOp::Set { json },
        } => {
            let list: Vec<DriverDevice> = serde_json::from_str(
                &std::fs::read_to_string(&json)
                    .with_context(|| format!("reading {}", json.display()))?,
            )
            .context("parsing device list")?;
            driver::set_devices(driver::require_plugin()?, &list)?;
            let uids: Vec<String> = list.iter().map(|d| d.uid.clone()).collect();
            let missing = driver::wait_for_uids(&uids, Duration::from_secs(2));
            if !missing.is_empty() {
                bail!("set accepted but devices did not appear: {missing:?}");
            }
            println!("ok: {} device(s)", list.len());
            Ok(())
        }
        Cmd::LoopbackTest { uid, secs } => {
            let uid = full_uid(&uid);
            let r = loopback::loopback_test(&uid, secs)?;
            println!(
                "{uid}: rate {} Hz, {} callbacks, rms {:.1} dBFS, dropouts {}",
                r.rate, r.callbacks, r.rms_dbfs, r.dropouts
            );
            if r.passed() {
                println!("PASS");
                Ok(())
            } else {
                bail!("FAIL (need rms > -20 dBFS and 0 dropouts)")
            }
        }
        Cmd::Run { config } => run(&config),
        Cmd::Meter { device_id } => {
            install_ctrlc()?;
            let uid = full_uid(&device_id);
            let p = loopback::Probe::start(&uid, false).with_context(|| {
                format!("cannot meter '{device_id}' (uid {uid}); run `sr-cli list` or `sr-cli driver get` for valid ids")
            })?;
            while !STOP.load(Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let peaks = p.stats.take_peaks(p.in_channels);
                println!(
                    "{}",
                    peaks.iter().map(|&x| db(x)).collect::<Vec<_>>().join(" ")
                );
            }
            Ok(())
        }
        Cmd::Doctor => doctor(),
    }
}

fn list() -> Result<()> {
    let engine = Engine::new()?;
    println!("hardware:");
    for d in engine.hardware() {
        println!(
            "  {:<46} in={:<2} out={:<2} {}",
            d.uid, d.in_ch, d.out_ch, d.name
        );
    }
    println!("apps (grouped by responsible app):");
    for g in apps::list_groups() {
        let helpers: Vec<&str> = g
            .members
            .iter()
            .map(String::as_str)
            .filter(|m| *m != g.bundle_id)
            .collect();
        println!(
            "  {}{:<40} {:<24} procs={}{}",
            if g.running_output { "* " } else { "  " },
            g.bundle_id,
            g.name,
            g.pids.len(),
            if helpers.is_empty() {
                String::new()
            } else {
                format!(" via {}", helpers.join(", "))
            }
        );
    }
    println!("(* = producing output)");
    Ok(())
}

fn run(path: &PathBuf) -> Result<()> {
    let cfg: Config = serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .context("parsing config")?;
    cfg.validate()
        .map_err(|e| anyhow::anyhow!("invalid config: {e}"))?;
    install_ctrlc()?;
    let mut engine = Engine::new()?;
    engine.apply(&cfg)?;
    let ids: Vec<&str> = cfg
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| d.id.as_str())
        .collect();
    println!("running {} device(s); Ctrl-C to stop", ids.len());
    while !STOP.load(Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        for id in &ids {
            let st = match engine.io_state(id) {
                Some(IoState::Running) => "running".to_owned(),
                Some(IoState::Starting) => "starting (waiting for TCC prompt?)".to_owned(),
                Some(IoState::Failed(s)) => format!("FAILED start status {s}"),
                None => "not built".to_owned(),
            };
            if let Some(m) = engine.meters(id) {
                let ch: Vec<String> = m.channels.iter().map(|&x| db(x)).collect();
                println!("{id} [{st}] out: {}", ch.join(" "));
                for (s, v) in &m.sources {
                    println!(
                        "    {s}: {}",
                        v.iter().map(|&x| db(x)).collect::<Vec<_>>().join(" ")
                    );
                }
            }
        }
    }
    Ok(())
}

fn doctor() -> Result<()> {
    let mut healthy = true;
    let ver = sr_audio::os_version();
    println!("macOS:           {ver}");
    match driver::find_plugin() {
        Some(p) => {
            println!(
                "driver plug-in:  found ({PLUGIN_BUNDLE_ID}, object {p}), protocol version {}",
                driver::version(p).unwrap_or_else(|| "?".into())
            );
            match driver::get_devices(p) {
                Ok(list) => {
                    println!("driver devices:  {}", list.len());
                    for d in list {
                        let vis = sr_audio::hal::device_by_uid(&d.uid).is_some();
                        let comp = sr_audio::hal::device_by_uid(&companion_uid(&d.uid)).is_some();
                        println!(
                            "  {} ({} ch) visible={vis} companion={comp}",
                            d.uid, d.channels
                        );
                    }
                }
                Err(e) => {
                    healthy = false;
                    println!("driver devices:  error: {e:#}")
                }
            }
        }
        None => {
            healthy = false;
            println!(
                "driver plug-in:  NOT FOUND ({PLUGIN_BUNDLE_ID}); install: packaging/driver-dev.sh install (dev) or the SoundRouter .pkg"
            )
        }
    }
    println!("TCC audio capture: {:?}", tcc::audio_capture_status());
    if !healthy {
        bail!("doctor: problems found (see above)");
    }
    Ok(())
}
