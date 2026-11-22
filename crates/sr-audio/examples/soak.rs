//! Long-run soak driver: applies a config and then does what the app's worker thread does while the
//! window is open (meters at 30 Hz, hardware + apps + status every 2 s), for `secs` seconds, counting
//! the calls. Sample the process from outside (`ps`, `lsof`, `top -pid`) to see slopes.
//! `cargo run --release -p sr-audio --example soak -- <config.json> <secs> [hidden]`
//! `hidden` reads no meters (the window is hidden: only the 2 s refresh runs).
use sr_audio::Engine;
use sr_core::backend::Backend;
use sr_core::config::Config;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("config.json");
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let hidden = args.next().as_deref() == Some("hidden");
    let cfg: Config = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let mut e = Engine::new()?;
    e.apply(&cfg)?;
    let ids: Vec<String> = cfg
        .devices
        .iter()
        .filter(|d| d.enabled)
        .map(|d| d.id.clone())
        .collect();
    let (t0, mut next_refresh) = (Instant::now(), Instant::now());
    let (mut meter_calls, mut refreshes) = (0u64, 0u64);
    while t0.elapsed() < Duration::from_secs(secs) {
        if !hidden {
            for id in &ids {
                std::hint::black_box(e.meters(id));
                meter_calls += 1;
            }
        }
        if Instant::now() >= next_refresh {
            next_refresh += Duration::from_secs(2);
            std::hint::black_box((e.apps(), e.hardware(), e.status()));
            refreshes += 1;
        }
        std::thread::sleep(Duration::from_millis(if hidden { 500 } else { 33 }));
    }
    println!("done: {meter_calls} meter reads, {refreshes} refreshes");
    for id in &ids {
        println!(
            "{id}: glitches {}, mismatches {}, tap {:?}",
            e.glitches(id),
            e.mismatches(id),
            e.tap_counters(id).unwrap_or_default()
        );
    }
    println!("xruns {:?}", e.status().xruns);
    Ok(())
}
