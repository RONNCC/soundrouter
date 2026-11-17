# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- App: source and monitor cards have a × ("Remove <name>") next to the On switch, and removing a card or deleting a device shows "Removed <name> · Undo" for 6 s.
- App: the Verify step's self-test has a Cancel button and reports "The test did not finish" after 30 s.
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
- App: when the audio engine cannot start, meters stay at silence and a standing banner says routing is not running (it no longer falls back to demo meters with a toast that vanishes).
- Engine: an apply whose driver devices were slow to settle (many devices added or re-channeled at once) failed after 2 s and kept the previous config, so the next device event re-applied that old config and removed the devices the user had just added. The newest config is now kept even when the driver is late, the settle window is 6 s, and devices that are removed or change channel count are torn down before the driver changes them (a channel change under 16 running aggregates had crashed `coreaudiod` once).
- App: a failed apply is written to the log file, not only shown in the status bar.
- Driver: an app that plays into a SoundRouter device now hears itself on that device's input while the engine is not running (V.out -> V.in, Soundflower-style); before, the input stayed silent until the engine attached. With the engine running the input carries the routed mix as before.
- Driver: a device-list change could hand a slot freed milliseconds earlier (by a second, concurrent reconcile) to a new device with more channels under the same object ids, while coreaudiod could still run I/O on them with the old, smaller buffer: a heap overflow inside coreaudiod. Reconciles now run on one worker (newest list wins) and a freed slot stays out of use for the 1 s teardown time, however many passes run. Needs the new driver installed.
- Driver: a device-list change applied while a sample-rate change had landed on the engine companion only sent the companion back to the old rate, leaving the pair silent until the next device-list change. The pair now converges on the last requested rate.
- App: on macOS the window showed only an empty grey surface, because the sidebar blur view ended up composited on top of the GPU layer that egui draws into. The blur now always stays underneath. If it cannot be placed there, the window falls back to an opaque one.
- App: launching a second copy of the app now brings back the window of the one already running, even when it was hidden in the menu bar. Before, `open -b` could send that request to a different registered copy, which then just exited.
- Engine: when a device ran on the visible-device fallback (hidden companion unavailable), its Pass-Thru source read the driver's new V.out -> V.in loop, which also carries the engine's own output, so the mix fed back into itself. Pass-Thru is now off on that fallback.
- Engine: the self-test routes its tone through the engine whenever the device is enabled, not only while its aggregate is up. Before, a recovery or retry that rebuilt the device mid-test could send the tone to the user's monitor outputs.
- Engine: a removed device's failed IOProc starts no longer keep the event debounce at its maximum (up to 5 s) for the rest of the session.
- Engine: reading a device's stream layout never trusts a buffer count beyond the bytes the HAL returned.
- App: with Start at Login (`--background`) the config was only sent to the engine from a drawn frame, and eframe stops drawing once the window is hidden, so routing usually did not start until the window was opened. The first apply now runs while hidden.
- App: opening a source's or monitor's Options no longer rewrites a volume outside the slider's -60..+12 dB range (from `sr-cli` or a hand-edited config) to the nearest end; it changes only when the slider is moved.
- App: an unplugged input device or monitor is shown by its last known name instead of its raw uid, and an unplugged monitor now says "Not connected" too. Sources and monitors record the device's (or app's) name in the config (`name`, optional).
- App: device names stop at the driver's 64-character limit while typing instead of failing with an error that named the device by its internal id.
- App: clicking a highlighted wire removes it; before, hovering highlighted it but clicking did nothing.
- App: the Monitors popup scrolls inside the window when there are many output devices; before, the devices below the window edge could not be reached.
- App: after "Uninstall… > Also delete my SoundRouter settings" the app no longer writes the settings folder back on the next edit or window close.

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
