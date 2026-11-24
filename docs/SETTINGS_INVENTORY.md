# Settings inventory

Research for a Settings sheet. Evidence at origin/main 2bee413. "Runtime" = takes effect without quitting the app. "Seam" = what a test drives today (or the one to add). Nothing here was run against live audio; ranges marked [INFERENCE] come from reading code and docs only.

## A. Already user-facing (candidates to also list in Settings)

| Setting | Where / how read | Runtime | Safe range, default | Control and label | Seam |
|---|---|---|---|---|---|
| Audio delay | `Config::buffer_frames` (`sr-core/config.rs:14-17`), validated against `BUFFER_FRAMES`, saved in config.json; edited by `Edit::BufferFrames` (`model.rs:300`), footer chip `app.rs:1138`, menu `app.rs:206` | Yes: a change rebuilds running devices (`worker.rs:233`, status goes through Starting) | 64 / 128 / 256 / 512 frames = Lowest / Low / Balanced / Safest; default 256 (+10.7 ms, `model.rs:75`); 64 and 128 can crackle in Low Power Mode (`model.rs:167-172`) | Keep the footer chip. In Settings, radio group "Audio delay" with the four names and "+x ms", plus the same Low Power Mode hint | `delay_name`, `delay_ms`, `apply(Edit)`; app_tests drive the chip menu |
| Start at login | LaunchAgent plist `~/Library/LaunchAgents/com.ronncc.soundrouter.login.plist`, `login.rs:9-57`; state is the file's existence (`login::is_enabled`) | Yes (takes effect next login; plist only, no launchctl) | on/off; default off; refuses `/Volumes/SoundRouter` and AppTranslocation paths (`login.rs:70`) | Switch "Open SoundRouter at login" (opens in the menu bar). Already in Permissions sheet (`setup.rs:1321`) and tray | `os.login_is_on` / `os.set_login` in `OsHooks` (`app.rs:100-111`), `login::write_agent` |
| Per-device volume, mute, enabled, channels, names | config.json `devices[]` (`config.rs:45-60`); volume `<= MAX_DB` 24 dB (`config.rs:10`), driver floor -64 dB (`driver_proto.rs:38`) | Yes (volume and mute do not rebuild) | Device channels even, 2..=64; at most 16 enabled (`MAX_DEVICES`, `driver_proto.rs:21`); name 1..=64 chars | Not Settings: stays on the graph | `Config::validate` |
| Source "Mute original" | `Source::mute_original` (`config.rs:76`); `CATapMuteBehavior` | Yes (rebuilds that device) | on/off; default off | Not Settings: per-source switch on the graph | `starts_something` (`worker.rs:233`) |
| Presets | `presets.json` next to config.json (`preset_store.rs:7`, `app.rs:297`); versioned, `.bad` quarantine | Yes | Unbounded list; names unique | Not Settings: Presets menu. Settings could add "Show presets file in Finder" | `Presets::load`, `presets_tests.rs` |

## B. Environment variables (power-user, no UI today)

| Knob | Location / read | Runtime | Safe range, default | Recommended | Seam |
|---|---|---|---|---|---|
| `SR_TAP_SPLIT` | `tapagg.rs:9,24`: `0` off, `1` on, else default on (`TAP_SPLIT_DEFAULT`). Read on each engine build (`engine.rs:1949`), but an env var is fixed per process | No (relaunch); a config field would apply at next rebuild | `1` (default) keeps the fast mic/monitor paths at the buffer size; `0` puts taps in the main aggregate: 1024-frame safety offset on every path (+~20 ms, `ARCHITECTURE.md:112`) | Do not expose. If ever: Advanced, "Keep app capture separate" on, with a hint | `split_setting` (`engine/tests.rs:1849`) |
| `SR_TAP_CUSHION` | `tapagg.rs:11,33-39`: frames, parsed as `usize`, bad value falls back to default; read when a tap aggregate is built (`tapagg.rs:218`) | No (relaunch) | Default = one IO buffer (64..512 frames). Rounded up to whole tap blocks (`tapring.rs:87`), so 1..block = block. Safe: 1x to 4x the buffer [INFERENCE: docs only tested 1x, `ARCHITECTURE.md:145,151`]; each extra block adds that much delay. `0` = no margin, every late cycle drops a block, "for measuring". Ring is `max(8 x cushion, 2 x 4096)` frames (`tapring.rs:18,95`); more than `keep + 3 blocks` is trimmed (`tapring.rs:179`) | Do not expose as a number. If ever: tri-state "App capture safety margin: Normal / Extra" = 1x / 2x buffer | `tapring::keep_frames` tests (`tapring/tests.rs:59`), `default_cushion` |
| `SR_TRACE_APPLY` | `sr-core/trace.rs:12`: only the exact value `1`; cached once in a `OnceLock` | No (relaunch) | off; writes timing lines to stderr (invisible from Finder) | Diagnostics only, never in Settings | `trace::enabled` |
| `SR_MAIN_UID_OVERRIDE` | `engine.rs:1599-1631`: routes into an existing loopback device (BlackHole) and skips driver sync; read on every apply | No (process env) | Unset. A wrong uid yields no candidates, so devices fail to start | Never in Settings (test hook; CI uses BlackHole) | `main_candidates_in` (`engine/tests.rs:91`), `tests/blackhole.rs:139` |
| `RUST_LOG` | `logging.rs:83` (app); `sr-cli` has `-v` / `-vv`. Malformed value falls back to `info` | No (relaunch; GUI launched from Finder has no env) | Default `info` | Settings "Log detail: Normal / Detailed" (debug) would need a runtime filter switch (`fern` filter is built once, `logging.rs:88`) | `logging::init` has no seam; `Rotating` tests only |
| `SR_OPEN` | `sr-cli/preset.rs:105`: opener binary, default `/usr/bin/open` | n/a | Test hook | Never | `cli.rs:383` |
| `SR_APP`, `SR_STRESS_TONE`, `SIGN_ID`, `SR_SIGN_NOTED` | `setup.rs:1835` (test), `examples/stress.rs:220`, `packaging/lib.sh:41` | n/a | Build/test only | Never | n/a |

## C. Hard-coded constants a user might want

| Behaviour | Location | Runtime | Safe range, default | Recommended | Seam |
|---|---|---|---|---|---|
| Drift compensation quality | `engine.rs:2642-2655`: sub-devices (mic, monitors) other than the main get `kAudioAggregateDriftCompensationLowQuality` (0x20), with drift on; main and companion have drift off (`tapagg.rs:145`) | No (needs rebuild of the aggregate; no config field) | HAL values 0 Min, 0x20 Low, 0x40 Medium, 0x60 High, 0x7F Max (`objc2-core-audio AudioHardware.rs:1708-1716`). Low "halves the resampler's fixed delay" (code comment); higher costs delay and CPU, gains fidelity on mismatched clocks [INFERENCE on audibility, not measured] | Do not expose by default. If asked: Advanced, "Clock matching for other devices: Lower delay / Higher quality" = Low / High, applied on rebuild | None; add a `drift_quality(cfg) -> u32` helper to unit test |
| Driver read-ahead `srla` | `driver_proto.rs:15`, driver `state.rs:17` (`READ_AHEAD_DEFAULT=false`); setter `set_read_ahead`; no app or CLI code writes it (grep of `crates/ app/`) | Yes in the driver (restarts stream calibration); not persisted, so a client must write it after every driver start | off. On: est. 1 to 9 ms saved per ring traversal (`ARCHITECTURE.md:50`, `LATENCY.md:66`); never run in a real coreaudiod; a short read gives a silent tail counted in `srov` | Hidden until a live run shows `srov` flat. Then "Lower loop delay (experimental)", off by default, app writes it on every driver (re)attach | fakehost + driver unit tests (`props.rs:353`); an `sr-audio::driver` setter needs a fake |
| Log file size and rotation | `logging.rs:7`: 2 MiB, one `.1` backup, rotated at launch and while running (`logging.rs:43-67`). Path `~/Library/Logs/SoundRouter/soundrouter.log` (`logging.rs:9`) | No | Fixed 2 MiB x 2 files (about 4 MiB) | No size control. Add "Show log in Finder" button (a path helper exists: `logging::log_path`) | `Rotating` tests (`logging.rs:155-215`) |
| Config path / presets path | `store::default_path` = `~/Library/Application Support/SoundRouter/config.json` (`store.rs:6`); `support_dir` in `sr-core/lock.rs:7`; `presets.json` beside it | No | Not user-selectable; `HOME` only | Read-only: "Show settings folder in Finder" | `store::load/save` tests with tempdirs |
| Instance lock / engine lock | `instance.lock`, `engine.lock` in the support dir (`lock.rs:14-19`); demo mode bypasses (`instance.rs:48`) | n/a | OS-released advisory locks, so stale files are harmless | Never | `second_lock_fails_until_first_dropped` (`lock.rs:48`) |
| Theme | `--theme dark|light` arg only, default System (`main.rs:34-57`, `set_theme` at `main.rs:127`) | Yes (egui `set_theme`) but not persisted | System / Light / Dark | Segmented "Appearance: System / Light / Dark", persisted (config or a sibling file); keeps `--theme` as override | `main.rs:247` arg tests |
| Reduce motion | Reads macOS "Reduce motion" per frame (`app.rs:103-123,1461`) | Yes (follows the OS) | Follow system | No control: show a note "Follows the macOS setting" if at all | `os.reduce_motion` hook (`app_tests.rs:3273`) |
| Window size | Fixed start 1180 x 700, min 1000 x 520 (`main.rs:112-113`); not saved | n/a | n/a | None | n/a |
| Sample rate | Driver offers 44.1 / 48 / 96 kHz, default 48 (`driver_proto.rs:24-25`); chosen by the client app, not us | n/a | n/a | None (delay math assumes 48 kHz, `model.rs:73`) | n/a |

## D. Session-only UI state (not persisted)

| State | Location | Behaviour today | Recommended |
|---|---|---|---|
| Summary card collapsed | `summary.rs:48` (`collapsed`), field in `App` (`app.rs:91`) | Resets to expanded each launch | Persist as "Show device summary" (UI state file, not config.json) |
| Banner dismissal | `health::Dismissed` (`health.rs:133`), key per problem and device (`silent:v1`, `hogged:..`) | Stays closed until quit; a different problem re-opens | Keep session-only. Optional "Show notices again" button in Settings = clear the set |
| Choose App: show background processes | `graph.rs:66,838`, plain `bool` | Resets each launch (checkbox under the list) | Persist; label "Show background processes in app list" |
| One-time "still running in the menu bar" hint | marker `.close-hint-seen` (`tray.rs:68-80`, set `app.rs:399`) | Shown once ever | "Reset tips" button deletes markers |
| Setup walkthrough seen | marker `.setup-seen` holds the version (`setup.rs:89-96`) | Reshown on each version bump unless finished | "Run setup again" button (the Permissions sheet already does most of it) |
| Menu-bar / background mode | `--background` arg (`main.rs:48`); tray built at launch (`app.rs:335`); the login agent passes the arg. Close hides the window (`tray.rs:41-56`), Quit really quits | Menu-bar item is always on once launched; no off switch | Not offered: close-to-hide needs the tray. Settings may say "Closing the window keeps SoundRouter running in the menu bar" |

## E. Menus today

| Menu | Contents (in order) | Evidence |
|---|---|---|
| Footer "..." (More actions) | Permissions, separator, Uninstall... (red). Shown only with a real driver; hidden in demo. Footer also has "Test setup" and the "Audio delay: <name>" chip (menu: four radio rows, Low Power hint, health line, "Measure delay...") to the left; status/toast text on the left | `app.rs:1096-1152`, `app.rs:198-270` |
| Presets button (title zone) | One row per preset, separator, "Save current as…", "Manage presets…" | `presets.rs:175-197` |
| Menu-bar item | "Open SoundRouter", status line (disabled: "No devices running" / "N devices running"), separator, Presets submenu (presets then "Save current as..."), Permissions, "Start at Login" (check), separator, "Quit SoundRouter". Hint title "SoundRouter is still running in the menu bar" for 8 s after first hide | `tray.rs:170-195`, `tray.rs:13-14` |
| Choose App... (source popup) | Search, Inputs, Apps, optional Background processes, Pass-Thru, Choose App..., "Show background processes" checkbox | `graph.rs:756-838` |

## F. Proposed Settings sheet, in order

1. Audio delay (radio, four names, Low Power hint). Moves the chip menu's list; the chip stays as shortcut.
2. Open SoundRouter at login (switch). Same state as the Permissions row and tray item; all three must re-read `login::is_enabled`.
3. Appearance: System / Light / Dark (needs persistence).
4. Show background processes in app list (persist); Show device summary (persist).
5. Buttons: Show log in Finder, Show settings folder in Finder, Show notices again.
6. Advanced (collapsed, off by default): app capture safety margin; lower loop delay (experimental, only after a live `srla` run); clock matching.

## G. Gaps and risks

- All env knobs are process-wide and read by `engine`; a UI value must flow through `Config` (version stays 1 with `#[serde(default)]` fields, unknown fields are dropped on save, `store.rs:65-69`) and into `tapagg::cushion_frames` / `engine.rs:2645` as parameters.
- No persisted app-preferences store exists apart from config.json and marker files; UI state (theme, collapsed, background list) should not enter `Config` because `sr-cli` and presets share it. A sibling `ui.json` with the same `fsio` atomic write and quarantine is the smallest fit.
- `srla` has no client code at all: enabling it needs a new `sr-audio::driver` setter, a re-write after every driver (re)attach, and a live run first.
- Do not expose `SR_TAP_CUSHION=0`, `SR_TAP_SPLIT=0`, `SR_MAIN_UID_OVERRIDE`: they remove margin or bypass the driver.
