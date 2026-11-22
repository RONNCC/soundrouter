# Testing

## Pyramid

| Layer | Covers | Command |
|-------|--------|---------|
| Unit + property | sr-core config/mixer/meters (incl. no-alloc test), driver slot pool, rings, parsing, engine layout planning, tap ring accounting (cushion, underrun, trim, drop), plan/ring swap order, tap IOProc copy, no-alloc test of the tap copy and ring pop, CLI exit codes | `cargo test --workspace` |
| Sanitizers (nightly, optional) | ThreadSanitizer over the real-thread mailbox and tap ring tests (acquire/release of the plan and ring mailboxes, the SPSC ring) and the sr-core / sr-audio unit tests | `rustup component add rust-src --toolchain nightly`, then `RUSTFLAGS=-Zsanitizer=thread cargo +nightly test -Zbuild-std --target aarch64-apple-darwin -p sr-core --test mailbox_threads` and `-p sr-audio --lib tapring` |
| Fake host | Driver loaded through CFPlugIn and driven through its vtable, no sudo; then 8 seeds of the randomized scenario fuzz | `packaging/driver-dev.sh fakehost` |
| Fake host fuzz | Seeded random lists (valid, invalid, repeated), rates, volumes, I/O start/stop, Performs and Aborts in any order, I/O on removed ids, concurrent property reads; guard words behind every buffer, slot quarantine, convergence to the last list | `packaging/driver-dev.sh fuzz` (`SEEDS=50 STEPS=1000` default; one seed: `fakehost <driver> fuzz <seed> [steps]`) |
| GUI | egui kittest interaction tests; GPU (Metal) snapshot baselines | `cargo test -p soundrouter-app --features snapshots` (`UPDATE_SNAPSHOTS=1` rewrites baselines) |
| Hardware (ignored) | Needs driver installed, permissions, audio hardware | `cargo test -- --ignored` |
| BlackHole e2e | `srtone` (into BlackHole ch 8-9) -> process tap -> engine mixer -> BlackHole 16ch ch 0-1 | `cargo test -p sr-audio --test blackhole -- --ignored` |
| Bundled driver signature | Install Driver's check accepts the app's own driver and refuses a tampered one, against a real ad-hoc or dev-signed build | `cargo test -p soundrouter-app -- --ignored real_bundle` (`SR_APP=<SoundRouter.app>`, default `target/SoundRouter.app`) |
| Live driver | Driver inside coreaudiod (sudo) | `packaging/driver-dev.sh test` |
| Live engine | `srtone` into `soundrouter:v2` -> process tap -> tap aggregate -> ring -> engine aggregate with the hidden companion -> `soundrouter:v1` input, installed driver, no sudo. `SR_TAP_SPLIT=0` runs the old single aggregate with the taps inside, `SR_TAP_SPLIT=1` (default) the split; run both | `packaging/engine-live-test.sh` (`SR_TAP_SPLIT=0 packaging/engine-live-test.sh`) |
| Churn soak | Engine apply loops against the installed driver: 200 volume changes, 100 app-source add/removes, 50 disable/enables (aggregate and driver devices torn down and re-created); and 60 add/removes of an app source on a real tapped process (`srtone` built with clang into `soundrouter:v2`), which builds, starts, swaps and destroys the tap aggregate each time. Asserts every apply succeeds and RSS grows < 8 MiB per loop. Changes the driver list: quit the app first | `cargo test -p sr-audio --test churn -- --ignored --nocapture --test-threads=1` |
| Config lifecycle | Engine against the installed driver while running: add, rename, re-channel, add a second device, disable, re-enable, buffer change, remove, then a burst of 8 edits. Each step checks the driver list, every enabled device Running on its companion, no shape mismatches, no background error, status in step. Changes the driver list: quit the app first | `cargo test -p sr-audio --test lifecycle -- --ignored --nocapture` |

The BlackHole test sets `SR_MAIN_UID_OVERRIDE=<device uid>` so the engine uses an existing loopback device as the main device and never touches the SoundRouter driver. It needs BlackHole 16ch installed (`brew install --cask blackhole-16ch`) and audio permissions.

## Tests are silent by design

No test, script or documented procedure plays to a physical output. The only tone sources are:

- `packaging/srtone.c` (`srtone <device-uid> [secs] [L] [R] [first-ch]`): a 1 kHz stereo sine into one device's output. It looks the device up by uid and exits 2 unless the uid starts with `soundrouter:` or the name contains `BlackHole`, so it cannot reach speakers or headphones. It is a separate process, so the engine taps it by path (`path:<srtone binary>`) like any app.
- `loopback::Probe::start(uid, tone = true)` (used by `sr-cli loopback-test` and the app self-test): refuses any device whose transport type is not virtual (`tone_allowed`, unit tested; fails closed). The app self-test only ever targets `soundrouter:<id>`.

Neither changes a default audio device. The live engine test temporarily sets the driver list to v1 (device under test, the one the meter reads) + v2 (player target, which nothing reads, so the tap is feedback-free) and restores the original list on exit, also on failure or Ctrl-C. The BlackHole test plays into BlackHole channels 8-9 while the engine writes channels 0-1, and the probe reads only 0-1. A stereo-mixdown tap scales a 16-channel player device (measured 1/8), so that test takes unity from the first stable reading and checks the cases relative to it; absolute levels are checked by the live engine test, whose player device is 2 ch.

## CI jobs

`.github/workflows/ci.yml`: `build` (deny, fmt, shellcheck, clippy `-D warnings`, workspace tests, driver build, fakehost, app bundle, signature check), `coverage` (llvm-cov, non-blocking), `snapshots` (non-blocking: baselines come from a local GPU, so hosted-runner glyph antialiasing differs; uploads diffs), `blackhole-e2e` (experimental: hosted runners may deny audio-capture TCC), and a non-blocking `driver-smoke`.

## Manual hardware checklist

Run on a Mac with the driver installed and permissions granted.

1. `packaging/driver-dev.sh install`, then `packaging/driver-dev.sh test`. It checks, in order: driver loaded; runtime create without restart; companions hidden from `system_profiler`; loopback latency at 512/128/32 frames; channel change 8 to 16 during IO; rename; persistence across a coreaudiod restart; removal. Every step must print without `FAIL`.
2. `sr-cli doctor` exits 0.
3. `packaging/engine-live-test.sh` (installed driver with `soundrouter:v1`, 2 ch; System Audio Recording granted to the terminal; no sudo). It plays a quiet 1 kHz tone with `srtone` into `soundrouter:v2` only (left peak 0.2, right peak 0.1, so channel routing is visible; nothing is audible) and taps that process and checks, with `sr-cli meter v1` (peaks per channel in dB, 4 Hz) while `sr-cli run <config>` is up:

   | Case | Config | Expected V input (L R) |
   |------|--------|------------------------|
   | unity | wires 0->0, 1->1 | -14.0 -20.0 dB (0.2, 0.1) |
   | volume | `volume_db` -6 | -20.0 -26.0 dB |
   | mute | source `muted` | -inf -inf |
   | route | wire 0->1 only | -inf -14.0 |
   | swap | wires 0->1, 1->0 | -20.0 -14.0 |
   | stop | `sr-cli run` stopped while `srtone` keeps playing | -inf -inf (V input back to the direct loop) |

   It also checks that the run log says `aggregate main device 'soundrouter:v1.engine'` (the companion, not the visible-device fallback), that `sr-cli loopback-test v1` passes with no engine and fails while an empty engine runs (V input is ring B then), that `sr-cli doctor` exits 0 with the engine running, and that `system_profiler` lists only the visible device. Last line must be `PASS`. By hand: `sr-cli -v run cfg.json` in one terminal, `sr-cli meter v1` in another, `srtone soundrouter:v2 30` in a third (with v2 in the config and driver list).
4. `sr-cli loopback-test v1` reports the tone on the device's input.
5. In the app: add a device, play audio from Music or Safari into it, and see the meter move; record from the device in QuickTime.
6. Per-app source: add a running app, confirm the permission prompt, the audio arrives; toggle "Mute in original output" and confirm the app goes silent on speakers only.
7. Choose App… with a non-running app: start the app afterwards and confirm capture begins.
8. Monitor to headphones: audio plays and volume/mute work. This item and 5-6 route real audio to real outputs by design: turn the system volume down first.
9. Close the window: routing continues from the menu bar; Quit stops it. Start at Login: log out and in.
10. Launch a second copy: it must defer to the first.
11. Uninstall from the setup sheet; devices disappear.
