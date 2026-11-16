# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.1.0]

First release. Driver-in-`coreaudiod` and end-to-end tap behaviour are not yet verified in automation (see README, Status).

### Added
- AudioServerPlugIn driver (Rust): up to 16 runtime-configurable loopback devices, each with a hidden engine companion, controlled through the `srcf` plug-in property and persisted across restarts.
- CoreAudio engine: per-device private aggregate, one IOProc and mixer; process taps per app, device sources, pass-thru, monitors, volume, mute and meters.
- Per-app "Mute in original output" and "Choose App…" for apps that are not running.
- egui node-graph GUI with dark and light themes, real app icons, accessible names.
- Menu-bar background mode, Dock reopen, Start at Login.
- First-run setup sheet: in-app driver install and uninstall, permission steps, self-test.
- Single-instance guard, config quarantine and newer-version protection, log file at `~/Library/Logs/SoundRouter/soundrouter.log`.
- `sr-cli`: `list`, `driver`, `loopback-test`, `run`, `meter`, `doctor`, `-v`.
- Packaging: driver and app bundles, `.pkg`, `.dmg`, notarization script, `uninstall.sh`, ThirdPartyLicenses via cargo-about.
- CI: build, coverage, GPU snapshots, experimental BlackHole e2e; release workflow.
