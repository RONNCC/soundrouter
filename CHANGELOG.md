# Changelog

All notable changes to this project are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added
- Engine/App/CLI: **Latency** setting, the engine aggregate's IO buffer (64, 128, 256 or 512 frames; default 256, was the HAL's 512). Config field `buffer_frames` (older configs load with 256, no version bump), footer menu "Latency: 5.3 ms (256 frames)" in the app, `sr-cli run --buffer N`. Smaller buffers cost more CPU and battery.
- App: before the administrator prompt, Install Driver checks the bundled driver with `codesign --verify --strict --deep` and that it carries the app's signing identity (same Team ID, or same leaf certificate for the self-signed dev identity; an ad-hoc app only accepts an ad-hoc driver inside its own `Contents/Resources`). Otherwise it stops with "The bundled driver does not match this app."
- Packaging: app, `sr-cli` and driver are built with `MACOSX_DEPLOYMENT_TARGET=14.2` (was 14.0; set in `packaging/lib.sh` and both workflows), so macOS 14.0/14.1 refuse them cleanly instead of crashing on the missing process-tap symbols; the scripts check each binary's `minos`. The driver's `LSMinimumSystemVersion` is 14.2 too.
- Engine: `Engine::glitches(id)` counts IOProc cycles that arrive after the HAL skipped audio (sample-time discontinuity); `sr-cli run` prints it and the shape-mismatch count next to the meters. `cargo test -p sr-audio --test churn -- --ignored` soaks the apply/rebuild loops for leaks against the installed driver.
- Docs: README "How routing works" diagram and CI badge; a quick-fix table in TROUBLESHOOTING; a first-release checklist in RELEASING; PLAN.md marked historical. The app bundle now also carries `LICENSE-MIT` and `LICENSE-APACHE` next to `ThirdPartyLicenses.html`.
- App: every permission row of the setup sheet that is not Allowed has its own button: **Allow Microphone** / **Allow System Audio Recording** (macOS's own prompt, off the UI thread; the row shows "Asking…" meanwhile), or **Open Settings** after a denial, with where the switch lives. The status reads **Needs permission** (was "Not asked yet"). The footer button is **Allow All**, the name of the one permission left, or **Continue**, and asks for every permission still unanswered, Microphone first, then System Audio Recording.
- App: the log's startup line records the raw TCC answers for the app (`TCC audio capture Unknown(2)` = not asked yet).
- Packaging: `packaging/make-dev-identity.sh` creates a stable self-signed `SoundRouter Local Dev` code-signing identity; the build scripts use it when `SIGN_ID` is unset, so macOS keeps Microphone and System Audio Recording grants across rebuilds.
- App: each virtual device row in the sidebar has a × ("Remove <name>", top-right, next to the On switch) instead of the − button at the bottom; it uses the same "Removed <name> · Undo" toast, and removing a device above the selected one keeps the selection on its device.
- App: source and monitor cards have a × ("Remove <name>") next to the On switch, and removing a card or deleting a device shows "Removed <name> · Undo" for 6 s.
- App: the Verify step's self-test has a Cancel button and reports "The test did not finish" after 30 s.
- Packaging: the `.pkg` declares its requirements (Apple silicon, macOS 14.2+), so Installer refuses Intel Macs and older systems instead of installing an arm64 driver coreaudiod cannot load; README lists both limits and the supported device sample rates.
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
- App: the window repaints at the worker's 30 Hz only when a meter value changed (was ~70 fps always: 6.4 % CPU with the window open on 3 routes, now about 1 %), and a hidden window stops reading meters and wakes twice a second instead of 30 times (2.1 % to 1.1 % with the engine running).
- App: device rows no longer show a green "running" dot; only starting, waiting-for-permission and failed states show a dot (status stays in the tooltip and accessibility label).
- Packaging: with `SIGN_ID` the driver, `sr-cli`, app and dmg get the hardened runtime and a secure timestamp; the version comes from `[workspace.package]` in `Cargo.toml` and is stamped into both `Info.plist` files; the driver pkg removes an older driver before installing; `notarize.sh` checks signatures first and reports Apple's log on rejection; release workflow checks the tag, attaches checksums and labels unsigned builds. See `docs/RELEASING.md`.
- New virtual devices, and the first-run "Loopback Audio" device, start with a Pass-Thru source wired 1→1, 2→2 (Loopback-style). Existing configs are not touched.
- Setup, driver and uninstall pop-ups share one macOS-style sheet: dimmed window, rounded card, icon and title, a checklist with status chips, and the default button on the right.

### Fixed
- Engine: rebuilding a virtual device's aggregate grew memory by about 0.4 MiB each time (+31 MiB over 38 disable/enable cycles). The IOProc's two 1 MiB scratch buffers came from malloc's medium zone, which kept them dirty after free; they are now mapped from the kernel and unmapped on teardown (+0.7 MiB over the same loop). The churn soak (`cargo test -p sr-audio --test churn -- --ignored`) now asserts it.
- Engine: enabling a device right after disabling it could fail with "neither the hidden companion nor the visible device is present": the HAL still listed the device the driver had just removed, the settle wait took it as the re-added one and lost it a moment later. An apply now also waits until removed devices are gone.
- Engine: a rate change on a monitored (or source) device that landed while the vdev was starting could be taken as the new baseline without a rebuild. A device with a rate event in the same batch as the start keeps its build-time rate, and rate listeners are registered before the baseline is read and the aggregate built.
- App: Start at Login refuses to point its LaunchAgent at the release dmg (`/Volumes/SoundRouter…`) or a Gatekeeper-translocated copy, and a launch from one no longer overwrites a working agent; it shows "move it to Applications first".
- Packaging: `uninstall.sh` no longer aborts before removing the driver when the target user cannot be looked up; README names where the installed `uninstall.sh` and `sr-cli` live.
- App: when the audio engine cannot start, meters stay at silence and a standing banner says routing is not running (it no longer falls back to demo meters with a toast that vanishes).
- Engine: an apply whose driver devices were slow to settle (many devices added or re-channeled at once) failed after 2 s and kept the previous config, so the next device event re-applied that old config and removed the devices the user had just added. The newest config is now kept even when the driver is late, the settle window is 6 s, and devices that are removed or change channel count are torn down before the driver changes them (a channel change under 16 running aggregates had crashed `coreaudiod` once).
- App: a failed apply is written to the log file, not only shown in the status bar.
- App: the setup sheet no longer opens by itself when nothing is missing. It opens for a missing driver, or (until finished or closed once in this version, remembered in `.setup-seen` next to the config) for a Microphone permission not granted while a device is enabled, or System Audio Recording denied while an app source exists. Once seen, a Microphone question still open is left to macOS's own prompt.
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

First release (not yet tagged; see docs/RELEASING.md). Driver-in-`coreaudiod` and end-to-end tap behaviour were checked by hand on one dev machine and are not covered by CI (see README, Status).

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
