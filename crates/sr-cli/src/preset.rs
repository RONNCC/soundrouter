//! `sr-cli preset list | save <name> | apply <name>`: the app's routing presets from a terminal.
use anyhow::{Context, Result, anyhow, bail};
use sr_core::config::Config;
use sr_core::lock;
use sr_core::preset::{PresetFile, plan, preset_url};
use sr_core::preset_store;
use std::path::PathBuf;
use std::process::Command;

pub struct Paths {
    pub config: PathBuf,
    pub presets: PathBuf,
    pub instance_lock: PathBuf,
}

impl Paths {
    pub fn from_home() -> Result<Paths> {
        let dir = lock::support_dir().context("HOME is not set")?;
        Ok(Paths {
            config: dir.join("config.json"),
            presets: dir.join("presets.json"),
            instance_lock: dir.join("instance.lock"),
        })
    }
}

/// The presets, with a load problem (corrupt file moved aside, newer version) on stderr.
fn load_presets(p: &Paths) -> PresetFile {
    let (f, msg) = preset_store::load(&p.presets);
    if let Some(m) = msg {
        eprintln!("warning: {m}");
    }
    f
}

fn load_config(p: &Paths) -> Result<Config> {
    let text = std::fs::read_to_string(&p.config).with_context(|| {
        format!(
            "reading {} (open SoundRouter once to create it)",
            p.config.display()
        )
    })?;
    let cfg: Config = serde_json::from_str(&text).context("parsing config")?;
    cfg.validate().map_err(|e| anyhow!("invalid config: {e}"))?;
    Ok(cfg)
}

pub fn list(p: &Paths) -> Result<()> {
    let f = load_presets(p);
    if f.presets.is_empty() {
        println!("no presets (save one with `sr-cli preset save <name>`)");
    }
    for pr in &f.presets {
        let n = pr.devices.len();
        println!("{}  ({n} device{})", pr.name, if n == 1 { "" } else { "s" });
    }
    Ok(())
}

pub fn save(p: &Paths, name: &str) -> Result<()> {
    let cfg = load_config(p)?;
    let mut f = load_presets(p);
    let replaced = f.save_as(name, &cfg).map_err(|e| anyhow!(e))?;
    preset_store::save(&p.presets, &f)?;
    println!(
        "{} preset \"{}\"",
        if replaced { "replaced" } else { "saved" },
        name.trim()
    );
    Ok(())
}

/// With the app running, ask it (its config and undo are the live ones); otherwise edit
/// `config.json`. Offline, changes to the virtual devices themselves need `add_devices`.
pub fn apply(p: &Paths, name: &str, add_devices: bool) -> Result<()> {
    let f = load_presets(p);
    let Some(preset) = f.find(name) else {
        let names: Vec<&str> = f.presets.iter().map(|p| p.name.as_str()).collect();
        bail!(
            "no preset named \"{name}\"{}",
            if names.is_empty() {
                String::new()
            } else {
                format!(" (have: {})", names.join(", "))
            }
        );
    };
    if matches!(lock::try_lock(&p.instance_lock), Ok(None)) {
        // The URL names the preset as saved, so a case-only difference still resolves in the app.
        let url = preset_url(&preset.name);
        let opener = std::env::var_os("SR_OPEN").unwrap_or_else(|| "/usr/bin/open".into());
        let ok = Command::new(&opener)
            .arg(&url)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            bail!("could not hand {url} to SoundRouter");
        }
        println!(
            "asked SoundRouter to apply \"{}\" (it may ask before adding devices)",
            preset.name
        );
        return Ok(());
    }
    let plan = plan(&load_config(p)?, preset).map_err(|e| anyhow!(e))?;
    if !plan.changes.is_empty() && !add_devices {
        let lines: Vec<String> = plan
            .changes
            .iter()
            .map(|c| format!("  {}", c.describe()))
            .collect();
        bail!(
            "preset \"{}\" changes your virtual devices:\n{}\npass --add-devices to allow it",
            preset.name,
            lines.join("\n")
        );
    }
    sr_core::fsio::write_atomic(&p.config, &serde_json::to_vec_pretty(&plan.config)?)?;
    println!("applied \"{}\" to {}", preset.name, p.config.display());
    Ok(())
}
