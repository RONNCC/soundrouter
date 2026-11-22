//! `sr-cli preset list | save <name> | apply <name>`: the app's routing presets from a terminal.
use anyhow::{Context, Result, anyhow, bail};
use sr_core::config::Config;
use sr_core::lock;
use sr_core::preset::{PresetFile, plan, preset_url};
use sr_core::preset_store;
use std::path::{Path, PathBuf};
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
    // Two `preset save` at once would both load the old file; the second save would drop the first.
    let _lock = preset_store::lock_for_update(&p.presets).context("locking presets.json")?;
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

/// `open` arguments for a link: to the very copy that holds the instance lock (it records its
/// bundle in the lock file) when that still exists, else to whichever app LaunchServices picks.
fn open_args(lock_content: &str, url: &str) -> Vec<String> {
    let app = lock_content.trim();
    if !app.is_empty() && Path::new(app).is_dir() {
        vec!["-a".into(), app.into(), url.into()]
    } else {
        vec![url.into()]
    }
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
        let app = std::fs::read_to_string(&p.instance_lock).unwrap_or_default();
        let opener = std::env::var_os("SR_OPEN").unwrap_or_else(|| "/usr/bin/open".into());
        let ok = Command::new(&opener)
            .args(open_args(&app, &url))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_link_goes_to_the_copy_that_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().to_str().unwrap();
        let url = "soundrouter://preset/A";
        assert_eq!(open_args(app, url), ["-a", app, url]);
        assert_eq!(open_args(&format!("{app}\n"), url), ["-a", app, url]);
        // No record, or a bundle that was moved or deleted: let LaunchServices choose.
        assert_eq!(open_args("", url), [url]);
        assert_eq!(open_args("/no/such/SoundRouter.app", url), [url]);
    }
}
