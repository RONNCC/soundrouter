use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sr_audio::engine::mic_status;
use sr_audio::{Engine, IoState, apps, driver, loopback, tcc};
use sr_core::backend::{Backend, TccStatus};
use sr_core::config::Config;
use sr_core::driver_proto::{DEVICE_UID_PREFIX, DriverDevice, PLUGIN_BUNDLE_ID, companion_uid};
use sr_core::lock;
use sr_core::selftest;

#[derive(Parser)]
#[command(name = "sr-cli", version, about = "SoundRouter headless tool")]
struct Cli {
    /// Log more: -v info, -vv debug (default: warnings only; RUST_LOG overrides)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,
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
    Run {
        config: PathBuf,
        /// After applying, run the app's Test Setup tone through the running engine for this device id
        /// (temporary Pass-Thru route, routing restored afterwards), print the result and exit
        #[arg(long, value_name = "ID")]
        self_test: Option<String>,
        /// Engine IO buffer in frames (64, 128, 256 or 512); overrides the config's `buffer_frames`.
        /// Smaller buffers cut latency but cost CPU and battery.
        #[arg(long, value_name = "FRAMES")]
        buffer: Option<u32>,
        /// Run even if SoundRouter.app or another `sr-cli run` owns the audio engine (they fight over the driver list)
        #[arg(long)]
        force: bool,
    },
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
        /// Write even if SoundRouter.app or `sr-cli run` owns the audio engine (it resets the list on its next apply)
        #[arg(long)]
        force: bool,
    },
}

static STOP: AtomicBool = AtomicBool::new(false);
fn install_ctrlc() -> Result<()> {
    Ok(ctrlc::set_handler(|| STOP.store(true, Relaxed))?)
}

/// A short id like `v1` becomes the driver uid; a uid of an existing device (BlackHole, ...) stays as is.
fn full_uid(s: &str) -> String {
    if s.contains(':') || sr_audio::hal::device_by_uid(s).is_some() {
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

/// Two engines apply their own driver list, each resetting the other's, forever. The app holds
/// `instance.lock` and `engine.lock`; `sr-cli run` holds `engine.lock`. Refuse while either is held
/// (unless `force`); otherwise take `engine.lock` for the caller to hold. An unusable lock file
/// never blocks. `None` when forced out or the lock is unavailable.
fn claim_engine(force: bool) -> Result<Option<File>> {
    claim_engine_at(
        lock::instance_lock_path().as_deref(),
        lock::engine_lock_path().as_deref(),
        force,
    )
}

fn claim_engine_at(
    instance: Option<&Path>,
    engine: Option<&Path>,
    force: bool,
) -> Result<Option<File>> {
    if !force {
        if let Some(p) = instance
            && matches!(lock::try_lock(p), Ok(None))
        {
            bail!(
                "SoundRouter.app is running and owns the audio engine. Quit it first, or pass --force to override"
            );
        }
        if let Some(p) = engine
            && matches!(lock::try_lock(p), Ok(None))
        {
            bail!(
                "another `sr-cli run` owns the audio engine. Stop it first, or pass --force to override"
            );
        }
    }
    Ok(engine.and_then(|p| lock::try_lock(p).ok().flatten()))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let level = match cli.verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();
    match cli.cmd {
        Cmd::List => list(),
        Cmd::Driver { op: DriverOp::Get } => {
            let list = driver::get_devices(driver::require_plugin()?)?;
            println!("{}", serde_json::to_string_pretty(&list)?);
            Ok(())
        }
        Cmd::Driver {
            op: DriverOp::Set { json, force },
        } => {
            let _engine = claim_engine(force)?;
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
        Cmd::LoopbackTest { uid, secs } => loopback_test(&full_uid(&uid), secs),
        Cmd::Run {
            config,
            self_test,
            buffer,
            force,
        } => run(&config, self_test.as_deref(), buffer, force),
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

/// Without an engine the driver loops the device's output straight back; with one running (in another
/// process, since this one has none) the input carries that engine's mix, which is silent unless a
/// Pass-Thru source is wired. This process cannot change another engine's routing, so say so.
fn loopback_test(uid: &str, secs: u64) -> Result<()> {
    let held = loopback::engine_holds_companion(uid);
    let r = loopback::loopback_test(uid, secs)?;
    println!(
        "{uid}: rate {} Hz, {} callbacks, rms {:.1} dBFS, dropouts {}",
        r.rate, r.callbacks, r.rms_dbfs, r.dropouts
    );
    let mic = mic_status();
    if let Err(e) = selftest::verdict(r.to_self_test(), mic) {
        if held && mic == TccStatus::Granted {
            bail!(
                "FAIL: an engine (the SoundRouter app or `sr-cli run`) is running for {uid}, so its input \
                 carries that engine's mix, not the driver's direct loop; this probe only passes while a \
                 Pass-Thru source is wired channel 0->0 and 1->1 there. Quit that engine and rerun, or \
                 test through it: `sr-cli run <config> --self-test <id>` or the app's Test Setup."
            );
        }
        return Err(e);
    }
    if held {
        println!(
            "(an engine is running for {uid}; the tone came back through its Pass-Thru route)"
        );
    }
    if r.passed() {
        println!("PASS");
        Ok(())
    } else {
        bail!("FAIL (need rms > -20 dBFS and 0 dropouts)")
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

fn run(path: &PathBuf, self_test: Option<&str>, buffer: Option<u32>, force: bool) -> Result<()> {
    let _engine_lock = claim_engine(force)?;
    let mut cfg: Config = serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .context("parsing config")?;
    if let Some(n) = buffer {
        cfg.buffer_frames = n;
    }
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
    if let Some(id) = self_test {
        // The IOProc starts asynchronously (and may wait on a TCC prompt): give it a moment first.
        std::thread::sleep(Duration::from_secs(1));
        let t = engine.self_test(id)?;
        println!(
            "self-test {id}: {:.1} dBFS, {} dropouts, passed={}",
            t.rms_dbfs, t.dropouts, t.passed
        );
        return if t.passed {
            Ok(())
        } else {
            bail!("self-test FAILED")
        };
    }
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
                let delay = engine.delay(id).map_or_else(
                    || "delay n/a".to_owned(),
                    |d| format!("delay {:.1} ms", d.ms()),
                );
                println!(
                    "{id} [{st}] out: {} ({delay}, glitches {}, mismatches {})",
                    ch.join(" "),
                    engine.glitches(id),
                    engine.mismatches(id)
                );
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
                        // listed by the driver but not published by coreaudiod: nothing can route through it
                        healthy &= vis && comp;
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
    println!("TCC microphone:    {:?}", tcc::microphone_status());
    if !healthy {
        bail!("doctor: problems found (see above)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_refuses_while_held_unless_forced() {
        let d = tempfile::tempdir().unwrap();
        let (inst, eng) = (d.path().join("instance.lock"), d.path().join("engine.lock"));
        let app = lock::try_lock(&inst).unwrap().unwrap();
        let e = claim_engine_at(Some(&inst), Some(&eng), false).unwrap_err();
        assert!(e.to_string().contains("SoundRouter.app is running"), "{e}");
        assert!(
            claim_engine_at(Some(&inst), Some(&eng), true)
                .unwrap()
                .is_some()
        );
        drop(app);
        let first = claim_engine_at(Some(&inst), Some(&eng), false).unwrap();
        assert!(first.is_some());
        let e = claim_engine_at(Some(&inst), Some(&eng), false).unwrap_err();
        assert!(e.to_string().contains("another `sr-cli run`"), "{e}");
        assert!(
            claim_engine_at(Some(&inst), Some(&eng), true)
                .unwrap()
                .is_none()
        );
        drop(first);
        assert!(
            claim_engine_at(Some(&inst), Some(&eng), false)
                .unwrap()
                .is_some()
        );
    }
}
