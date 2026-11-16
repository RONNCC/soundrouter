# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Driver: each virtual device now has a volume and a mute control on its input and output scope (-64..0 dB, applied on the read side with no allocation; default 0 dB, unmuted), so apps and the system volume keys can control it. The hidden engine companion has none. Needs the new driver installed.
- Driver: late or oversized `WriteMix` calls are counted per device and readable through the custom property `'srov'`.
- Docs: driver clock/zero-timestamp contract, volume/mute gain placement and the default-device decisions in `docs/ARCHITECTURE.md`.
- `packaging/engine-live-test.sh`: live check of the engine against the installed driver (app tap, volume, mute, channel routing, input reverts when the engine stops). The engine logs which main device each aggregate uses (`sr-cli -v run`).
- Engine: recovers from system sleep/wake and from a changed nominal sample rate on a virtual device's main or sub devices by rebuilding the affected aggregates (off the realtime thread, with backoff). A vdev whose IOProc fails to start is rebuilt a bounded number of times. While a vdev is rebuilt it reports `Starting`; after the retries run out the error is shown.

- App: the Sources popup lists **Audio Inputs** (every input device with its channel count, never SoundRouter's own virtual devices) first, then Applications; playing apps come first, then regular GUI apps by name. Daemons, agents and helpers are hidden behind "Show background processes". A search field filters both lists, the list scrolls inside a popup that stays within the window, and the popup no longer stretches across the window over the source cards.
- App: an input-device source whose device is gone shows "Not connected" and is kept; a denied Microphone permission shows on the card. Adding an input while macOS has not been asked yet holds the first apply for the setup sheet, which now says it also covers microphones added as sources.
- Core: `AppInfo::regular` (activation policy `.regular`); `sr_audio::engine::is_own_uid`.
- `sr-cli meter` accepts the uid of any existing device (e.g. `BlackHole16ch_UID`), not only our driver ids.
- README: how to route the Mac's microphone into a virtual device.

### Changed
- Packaging: with `SIGN_ID` the driver, `sr-cli`, app and dmg get the hardened runtime and a secure timestamp; the version comes from `[workspace.package]` in `Cargo.toml` and is stamped into both `Info.plist` files; the driver pkg removes an older driver before installing; `notarize.sh` checks signatures first and reports Apple's log on rejection; release workflow checks the tag, attaches checksums and labels unsigned builds. See `docs/RELEASING.md`.
- New virtual devices, and the first-run "Loopback Audio" device, start with a Pass-Thru source wired 1→1, 2→2 (Loopback-style). Existing configs are not touched.
- Setup, driver and uninstall pop-ups share one macOS-style sheet: dimmed window, rounded card, icon and title, a checklist with status chips, and the default button on the right.

### Fixed
- Driver: an app that plays into a SoundRouter device now hears itself on that device's input while the engine is not running (V.out -> V.in, Soundflower-style); before, the input stayed silent until the engine attached. With the engine running the input carries the routed mix as before.
- App: on macOS the window showed only an empty grey surface, because the sidebar blur view ended up composited on top of the GPU layer that egui draws into. The blur now always stays underneath. If it cannot be placed there, the window falls back to an opaque one.
- App: launching a second copy of the app now brings back the window of the one already running, even when it was hidden in the menu bar. Before, `open -b` could send that request to a different registered copy, which then just exited.

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
