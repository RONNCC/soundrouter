# Testing

## Pyramid

| Layer | Covers | Command |
|-------|--------|---------|
| Unit + property | sr-core config/mixer/meters (incl. no-alloc test), driver slot pool, rings, parsing, engine layout planning, CLI exit codes | `cargo test --workspace` |
| Fake host | Driver loaded through CFPlugIn and driven through its vtable, no sudo | `packaging/driver-dev.sh fakehost` |
| GUI | egui kittest interaction tests; GPU (Metal) snapshot baselines | `cargo test -p soundrouter-app --features snapshots` (`UPDATE_SNAPSHOTS=1` rewrites baselines) |
| Hardware (ignored) | Needs driver installed, permissions, audio hardware | `cargo test -- --ignored` |
| BlackHole e2e | `afplay` -> process tap -> engine mixer -> BlackHole 16ch | `cargo test -p sr-audio --test blackhole -- --ignored` |
| Live driver | Driver inside coreaudiod (sudo) | `packaging/driver-dev.sh test` |

The BlackHole test sets `SR_MAIN_UID_OVERRIDE=<device uid>` so the engine uses an existing loopback device as the main device and never touches the SoundRouter driver. It needs BlackHole installed (`brew install --cask blackhole-2ch`) and audio permissions; it plays a short quiet tone through the speakers.

## CI jobs

`.github/workflows/ci.yml`: `build` (deny, fmt, clippy `-D warnings`, workspace tests, driver build, fakehost, app bundle, signature check), `coverage` (llvm-cov, non-blocking), `snapshots` (non-blocking, uploads diffs), `blackhole-e2e` (experimental: hosted runners may deny audio-capture TCC), and a non-blocking `driver-smoke`.

## Manual hardware checklist

Run on a Mac with the driver installed and permissions granted.

1. `packaging/driver-dev.sh install`, then `packaging/driver-dev.sh test`. It checks, in order: driver loaded; runtime create without restart; companions hidden from `system_profiler`; loopback latency at 512/128/32 frames; channel change 8 to 16 during IO; rename; persistence across a coreaudiod restart; removal. Every step must print without `FAIL`.
2. `sr-cli doctor` exits 0.
3. `sr-cli loopback-test v1` reports the tone on the device's input.
4. In the app: add a device, play audio from Music or Safari into it, and see the meter move; record from the device in QuickTime.
5. Per-app source: add a running app, confirm the permission prompt, the audio arrives; toggle "Mute in original output" and confirm the app goes silent on speakers only.
6. Choose App… with a non-running app: start the app afterwards and confirm capture begins.
7. Monitor to headphones: audio plays and volume/mute work.
8. Close the window: routing continues from the menu bar; Quit stops it. Start at Login: log out and in.
9. Launch a second copy: it must defer to the first.
10. Uninstall from the setup sheet; devices disappear.
