# Troubleshooting

Start with `sr-cli -v doctor` (exits 1 when unhealthy) and the log `~/Library/Logs/SoundRouter/soundrouter.log`.

## Driver not found / no SoundRouter devices
- Check `/Library/Audio/Plug-Ins/HAL/SoundRouter.driver` exists; install via the setup sheet or `packaging/driver-dev.sh install`.
- Restart audio: `sudo killall coreaudiod` (audio drops for 1-2 s).
- Check `codesign -dv /Library/Audio/Plug-Ins/HAL/SoundRouter.driver` and remove quarantine: `sudo xattr -dr com.apple.quarantine <driver>`.
- Watch `log stream --predicate 'process == "coreaudiod"'` while it restarts.

## Permissions reset after a rebuild
Rebuilds keep asking for permissions: run `packaging/make-dev-identity.sh` once, then rebuild with `packaging/bundle-app.sh`. Ad-hoc signing gives each build a new identity (the grant is tied to the code hash), so grants reset; the script creates a stable self-signed identity, `SoundRouter Local Dev`, that the build scripts use automatically. Grant Microphone and System Audio Recording once more after the first build signed with it (System Settings > Privacy & Security). If a build shows "codesign wants to use key", enter your login password and choose Always Allow. Check with `codesign -dr - target/SoundRouter.app`: it must say `certificate leaf`, not `cdhash`.

## System Audio Recording ("Needs permission", app sources silent)
- Open **Test Setup…** and click **Allow System Audio Recording** (or **Allow All** / **Continue**): macOS shows its own prompt; answer Allow. The row turns "Allowed" without a relaunch.
- "Denied": click **Open Settings** (System Settings › Privacy & Security › Screen & System Audio Recording) and turn SoundRouter on under **System Audio Recording Only**. If macOS offers Quit & Reopen, accept it.
- macOS asks once per app identity: after a denial no prompt appears again, only the Settings switch works. `tccutil reset AudioCapture com.ronncc.soundrouter` forgets the answer so the prompt can appear again.
- The log's startup line records what macOS answers this build: `TCC audio capture Unknown(2)` = not asked yet, `Granted`, `Denied`. `sr-cli doctor` run from Terminal reports the terminal's permission, not the app's.

## No sound
- Is the app running (or in the menu bar)? Routing stops when it quits; devices remain as plain loopbacks.
- Check both permissions are granted and the setup self-test passes.
- Check the node is not muted, volume is up, and the monitor device is the right one.
- Use `sr-cli meter v1` to see what apps record from device `v1`.
- For app sources, the app must be producing audio; "Mute in original output" silences that app on its original output by design.

## Feedback loops
Do not route a virtual device's own output into a monitor that an app records from it, and do not monitor a device to an output that is also tapped. Mute the source or break the cycle in the graph.

## Uninstall
`packaging/uninstall.sh` removes the driver, restarts coreaudiod and forgets installer receipts (delete the app by hand); `--purge` also removes `~/Library/Application Support/SoundRouter` (config), logs and the login LaunchAgent. `--dry-run` shows what would be removed.

## Logs
GUI: `~/Library/Logs/SoundRouter/soundrouter.log`. CLI: stderr; `-v` info, `-vv` debug, `RUST_LOG` overrides.

## rustc 1.89 is not supported
If you see `rustc 1.89 is not supported`, rustup's cargo must come first on PATH (`export PATH="$HOME/.cargo/bin:$PATH"`); Homebrew cargo ignores `rust-toolchain.toml`. `packaging/*.sh` do this automatically.
