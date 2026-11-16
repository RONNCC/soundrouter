# SoundRouter — FINAL execution plan (OpusPlan)

Supersedes agent://AdvisorPlan. Driver ABI details, tap/TCC details and GUI crate pick are owned by OpusDriver / OpusCapture / OpusGui; where they report, their findings win on those points. Defaults below are what slices use if they have not reported.

## 1. Architecture (decided)

```
 Safari ──tap──┐                                   Zoom/OBS pick "Loopback Audio" as MIC
 Mic ──────────┤  private aggregate per virtual device   ▲
               ├─► ONE IOProc (sr-audio) ─ mixer ─► vdev OUTPUT ─driver loops─► vdev INPUT
 Speakers ◄────┘     (clock = vdev, others drift=1)  └─► monitor device outputs
```
- **HAL plug-in is required.** Only a HAL plug-in can publish a device that other apps select as an input and that carries audio we mixed. Process taps produce sources; they are not a sink. Proven: a private tap is dropped from a *public* aggregate (spike: public agg in=[16], private agg in=[16,2]), so the "driverless public aggregate" trick cannot even pass a tap through.
- **Driver = dumb N-channel loopback devices** (output → ring → input, BlackHole-like). No mixing in the driver. No audio IPC.
- **Engine = one private aggregate per enabled virtual device:** main/clock = the vdev; subdevices = vdev + each physical-input source device + each monitor device (`drift: 1`); taps = one private tap per app source (`drift: 1`). One `AudioDeviceCreateIOProcID` callback reads all inputs and writes vdev output + monitor outputs. HAL drift compensation replaces the old plan's rtrb rings + rubato SRC.
- **Buffer order (proven):** aggregate input buffers = subdevices in list order, then taps; output = subdevices in order. That is how `IoLayout` is built.
- **Pass-thru:** apps that play directly into the vdev are looped to its input by the driver (identity routing, always on). The engine ignores the vdev input buffer, which would otherwise feed back. Routed or muted pass-thru is out of scope (shortcut).
- **Control plane:** a custom property `'srdv'` on the plug-in object. Its value is a CFString of JSON `[DriverDevice]`. The app finds the plug-in via `kAudioHardwarePropertyTranslateBundleIDToPlugIn`. The driver diffs the list and adds or removes devices plus `PropertiesChanged(kAudioPlugInPropertyDeviceList)`. Channel-count changes go through `RequestDeviceConfigurationChange`. The driver persists the list via host `WriteToStorage`/`CopyFromStorage`, so devices survive reboot with the app closed. This needs no coreaudiod restart. The custom data types `'cfst'`/`'plst'` and the storage/PropertiesChanged/RequestDeviceConfigurationChange host calls were confirmed in `AudioServerPlugIn.h`. libASPL README documents runtime `AddDevice` + Storage API. Ignore the vtable `CreateDevice`; it is not needed.
- **Engine lives in the GUI process.** Routing stops when the app quits; devices remain and act as plain loopbacks. Background helper (LaunchAgent/SMAppService) is a later step, not M0–M6.

## 2. Evidence (run on this Mac, 14.7.1 arm64, CLT 16.1)
| Claim | Command | Result |
|---|---|---|
| notarize w/o Xcode | `xcrun --find notarytool stapler` | both in `/Library/Developer/CommandLineTools/usr/bin` ✅; `pkgbuild productbuild productsign hdiutil lipo` present |
| ad-hoc HAL load plausible | `codesign -d --entitlements - /usr/sbin/coreaudiod` | has `com.apple.private.security.clear-library-validation` (loads non-Apple-team code). Final proof needs sudo install = M0 |
| BlackHole ships Developer ID + hardened runtime | `codesign -dv /Library/Audio/Plug-Ins/HAL/BlackHole16ch.driver` | Team Q5C99V536K, flags=runtime |
| BlackHole license | `gh api repos/ExistentialAudio/BlackHole/contents/LICENSE` | **GPL-3.0** → never copy its code; libASPL = MIT |
| tap + aggregate from Rust | spike `/tmp/sr-opusplan/aggspike` (objc2-core-audio 0.3.2) | `AudioHardwareCreateProcessTap` st=0 without any TCC prompt; private agg [BH16 clock, speakers drift] + tap → in=[16,2] out=[16,2] @48k; destroy st=0 |
| objc2 coverage | grep crate src | has taps, `CATapDescription`, aggregates, IOProc, `TranslateBundleIDToPlugIn`, `ProcessObjectList`, `IOProcStreamUsage`. **Lacks** `AudioServerPlugInDriverInterface`/`HostInterface`/`IOCycleInfo` → driver hand-writes repr(C) ABI or uses `coreaudio-sys` `audio_server_plugin` |
| CI cost | GitHub docs/changelog 2025-12 | standard hosted runners (incl. macOS) free on public repos |
| toolchain | `rustc -V`; `rustup run stable rustc -V` | PATH cargo = Homebrew rustc **1.89** (no extra targets); rustup stable = 1.86 |

## 3. Crates (frozen; MSRV 1.89, edition 2024)
`objc2 0.6`, `objc2-foundation 0.3`, `objc2-core-foundation 0.3`, `objc2-core-audio 0.3.2` (feat `AudioHardware`), `objc2-core-audio-types 0.3`, `serde 1` (+derive), `serde_json 1`, `rtrb 0.4.1`, `anyhow 1`, `clap 4` (derive), `eframe 0.33.3` + `egui-snarl 0.9.0` (MSRV 1.88). Newer pairs: snarl 0.12 needs `egui ^0.36` and rust 1.95. eframe is now 0.37 (old plan's 0.36.2 is stale). Upgrade later with `brew upgrade rust`. **Dropped:** `coreaudio-rs`, `cpal`, `rubato` (aggregate drift handles clocks; shortcut: aggregate needs subdevices at one nominal rate, so the engine sets them to the vdev rate, and real SRC comes later only if a device refuses).

## 4. Repo layout + ownership
```
Cargo.toml                 workspace (Main)  members: crates/sr-core crates/sr-audio crates/sr-cli driver app
crates/sr-core/            pure Rust, no OS deps
  src/lib.rs config.rs backend.rs driver_proto.rs   ← FROZEN by Main before fan-out
  src/mixer.rs meters.rs                            ← Slice B
crates/sr-audio/           CoreAudio engine (Slice C)
crates/sr-cli/             headless tool, bin `sr-cli` (Slice C)
driver/                    package `soundrouter-driver`, cdylib, Info.plist (Slice A)
app/                       package `soundrouter-app`, eframe GUI, bin `SoundRouter` (Slice D)
packaging/ .github/workflows/ (Slice E)
```

## 5. Frozen contracts (Main commits these verbatim before fan-out)

`crates/sr-core/src/config.rs`
```rust
use serde::{Deserialize, Serialize};
pub const MAX_CH: usize = 64;
pub const MAX_FRAMES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Config { pub version: u32, pub devices: Vec<VirtualDevice> } // version = 1

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VirtualDevice {
    pub id: String,            // stable short id, e.g. "v1"; driver UID = DEVICE_UID_PREFIX + id
    pub name: String,
    pub enabled: bool,
    pub channels: u16,         // 2..=MAX_CH, even
    pub volume_db: f32,
    pub muted: bool,
    pub sources: Vec<Source>,
    pub wires: Vec<Wire>,
    pub monitors: Vec<Monitor>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source { pub id: String, pub kind: SourceKind, pub channels: u16, pub volume_db: f32, pub muted: bool }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceKind { App { bundle_id: String }, Device { uid: String } }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Wire { pub source: String, pub src_ch: u16, pub out_ch: u16 }        // source ch -> vdev ch
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Monitor { pub device_uid: String, pub enabled: bool, pub volume_db: f32, pub wires: Vec<MonitorWire> }
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MonitorWire { pub out_ch: u16, pub dst_ch: u16 }                    // vdev ch -> monitor ch
impl Config { pub fn validate(&self) -> Result<(), String>; } // ids unique, channels in range, wires reference existing ids/chs
```
Example `~/Library/Application Support/SoundRouter/config.json`:
```json
{"version":1,"devices":[{"id":"v1","name":"Loopback Audio","enabled":true,"channels":2,"volume_db":0.0,"muted":false,
 "sources":[{"id":"s1","kind":{"type":"app","bundle_id":"com.apple.Safari"},"channels":2,"volume_db":0.0,"muted":false},
            {"id":"s2","kind":{"type":"device","uid":"BuiltInMicrophoneDevice"},"channels":1,"volume_db":0.0,"muted":false}],
 "wires":[{"source":"s1","src_ch":0,"out_ch":0},{"source":"s1","src_ch":1,"out_ch":1},
          {"source":"s2","src_ch":0,"out_ch":0},{"source":"s2","src_ch":0,"out_ch":1}],
 "monitors":[{"device_uid":"BuiltInSpeakerDevice","enabled":true,"volume_db":-6.0,
              "wires":[{"out_ch":0,"dst_ch":0},{"out_ch":1,"dst_ch":1}]}]}]}
```

`crates/sr-core/src/driver_proto.rs` (driver and app both depend on sr-core)
```rust
pub const PLUGIN_BUNDLE_ID: &str = "com.ronncc.soundrouter.driver";
pub const APP_BUNDLE_ID: &str = "com.ronncc.soundrouter";
pub const DEVICE_UID_PREFIX: &str = "soundrouter:";
pub const MANUFACTURER: &str = "SoundRouter";
/// Custom property on the plug-in object. Global scope, element main. Data = CFStringRef holding JSON Vec<DriverDevice>.
/// Get returns the current list. Set replaces it. Advertised in kAudioObjectPropertyCustomPropertyInfoList as 'cfst'.
pub const PROP_DEVICES: u32 = u32::from_be_bytes(*b"srdv");
pub const STORAGE_KEY: &str = "devices";              // host WriteToStorage key, same JSON
pub const SAMPLE_RATES: [f64; 3] = [44_100.0, 48_000.0, 96_000.0];
pub const DEFAULT_RATE: f64 = 48_000.0;
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DriverDevice { pub uid: String, pub name: String, pub channels: u16 }
```

`crates/sr-core/src/backend.rs` (GUI codes against this; sr-audio implements it)
```rust
use crate::config::Config;
#[derive(Clone, Debug)] pub struct HwDevice { pub uid: String, pub name: String, pub in_ch: u16, pub out_ch: u16 }
#[derive(Clone, Debug)] pub struct AppInfo { pub bundle_id: String, pub name: String, pub running_output: bool }
#[derive(Clone, Debug, Default)]
pub struct MeterReading { pub sources: Vec<(String, Vec<f32>)>, pub channels: Vec<f32> } // linear peak since last read
pub trait Backend {
    fn apply(&mut self, cfg: &Config) -> anyhow::Result<()>; // idempotent diff: driver devices, aggregates, taps, plans
    fn hardware(&self) -> Vec<HwDevice>;                      // excludes our own vdevs and aggregates
    fn apps(&self) -> Vec<AppInfo>;                           // processes known to the HAL, deduped by bundle_id
    fn meters(&self, device_id: &str) -> Option<MeterReading>;
}
```

Mixer (Slice B implements; signatures frozen):
```rust
pub enum OutPort { Device(u16), Monitor { uid: String, ch: u16 } }
pub struct IoLayout { pub inputs: Vec<Option<(String, u16)>>, pub outputs: Vec<Option<OutPort>> } // index = aggregate planar ch; None = ignore (e.g. vdev input)
pub struct MixPlan { /* precomputed sparse taps: (in_idx, out_idx, gain) + per-output gain */ }
impl MixPlan { pub fn compile(dev: &VirtualDevice, layout: &IoLayout) -> MixPlan; }
pub struct Meters { /* [AtomicU32; MAX_CH] in + out, f32 bits */ }
impl Meters { pub fn take(&self, layout: &IoLayout, dev: &VirtualDevice) -> MeterReading; }
pub struct Mixer;      // audio-thread side; zero alloc in process
pub struct MixerHandle; // control side
pub fn mixer(meters: std::sync::Arc<Meters>) -> (Mixer, MixerHandle);
impl MixerHandle { pub fn set_plan(&mut self, p: MixPlan); } // rtrb push Box<MixPlan>; old boxes come back on a 2nd rtrb and drop here
impl Mixer { pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize); } // planar; zero outputs first; no plan = silence
```
Semantics: out = Σ in·10^(src_vol/20) over wires (muted src = 0). Vdev ch gain = 10^(vol/20), with mute applied. Monitor ch = Σ vdev ch × monitor vol over MonitorWires; disabled monitor = silence.

## 6. Milestones
| M | Deliverable | Acceptance (exact) |
|---|---|---|
| M0 | Workspace + frozen contracts; driver loads with 1 hard-coded 2ch device | `cargo test -p sr-core` green; `packaging/build-driver.sh` → `codesign --verify` ok; **user**: `sudo packaging/install-driver-dev.sh` → `system_profiler SPAudioDataType \| grep -A3 SoundRouter` shows device, 2 in/2 out |
| M1 | Loopback I/O + `'srdv'` control | `sr-cli loopback-test soundrouter:v1` plays 1 kHz into output, reads input, RMS > −20 dBFS, 0 dropouts in 60 s. `sr-cli driver set devices.json` adds an 8ch device **without** coreaudiod restart. List survives `sudo killall coreaudiod` |
| M2 | sr-core mixer + sr-audio engine (device sources, monitors) | mixer unit tests (wire gain, mute, monitor map, plan swap, no-alloc check). `sr-cli run config.json` with mic→v1 ch1-2: `sr-cli meter v1` shows > −40 dBFS while speaking; monitor plays to speakers |
| M3 | App (tap) sources | Safari plays → `sr-cli meter v1` nonzero on s1; app relaunch (new PID) re-taps within 2 s; one TCC prompt from the `.app` |
| M4 | GUI | create/delete vdev, add Safari+mic, drag wires, monitor to speakers, meters move at ≥30 Hz; quit/relaunch restores config; device appears/disappears in Audio MIDI Setup live |
| M5 | Robustness | hot-plug monitor/mic (engine rebuilds aggregate, no crash), vdev rate change 48k→44.1k, 8 vdevs × 16ch idle CPU < 5% |
| M6 | Packaging | `SoundRouter.app` (Info.plist with `NSMicrophoneUsageDescription` + `NSAudioCaptureUsageDescription`), driver `.pkg` with postinstall `killall coreaudiod`, DMG. Distribution: Developer ID sign + `xcrun notarytool submit --wait` + `stapler staple` (CLT works) |

## 7. Parallel slices (after Main lands §5 + workspace skeleton)
Slices A, B, D, E start together. C starts together too and calls B's mixer by its frozen signature; it integrates once B lands.

### Slice A — Driver (owns `driver/**`, `packaging/build-driver.sh`, `packaging/install-driver-dev.sh`)
```
Build a macOS AudioServerPlugIn in Rust: crate `soundrouter-driver` at driver/, crate-type cdylib, bundle SoundRouter.driver.
Contracts: read crates/sr-core/src/driver_proto.rs (do not edit it). Plug-in bundle id PLUGIN_BUNDLE_ID. Device UID = DriverDevice.uid. Manufacturer MANUFACTURER.
Requirements:
1. Export `extern "C" fn SoundRouterDriverFactory(alloc: CFAllocatorRef, type_uuid: CFUUIDRef) -> *mut c_void`. Return the driver interface only when type_uuid == kAudioServerPlugInTypeUUID. Info.plist: CFPlugInFactories {<new UUID>: SoundRouterDriverFactory}, CFPlugInTypes {443ABAB8-E7B3-491A-B985-BEB9187030DB: [<UUID>]}, CFBundleIdentifier=PLUGIN_BUNDLE_ID.
2. ABI: hand-write #[repr(C)] AudioServerPlugInDriverInterface / HostInterface / IOCycleInfo from /Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks/CoreAudio.framework/Headers/AudioServerPlugIn.h (objc2-core-audio 0.3.2 lacks them; its constants are OK to use), or use coreaudio-sys 0.2.18 feature audio_server_plugin. No panic may cross FFI.
3. Objects: plug-in (1), N devices, each with 1 input + 1 output stream of `channels` Float32 interleaved, SAMPLE_RATES, DEFAULT_RATE, zero-timestamp period 16384 from host clock. Each device is a loopback: WriteMix → ring → ReadInput. Output from all clients is mixed by HAL. Clear the ring when IO stops.
4. Custom property PROP_DEVICES (CFString JSON Vec<DriverDevice>) on the plug-in object: list it in kAudioObjectPropertyCustomPropertyInfoList as 'cfst'. Get = current list. Set = diff → add/remove devices + host PropertiesChanged(kAudioPlugInPropertyDeviceList). Channel change → RequestDeviceConfigurationChange/PerformDeviceConfigurationChange. Persist via host WriteToStorage(STORAGE_KEY); restore in Initialize. Empty storage → one device {uid:"soundrouter:v1", name:"Loopback Audio", channels:2}.
5. RT path (DoIOOperation): no alloc, no locks except try-lock, no logging.
6. build-driver.sh: cargo build --release -p soundrouter-driver, assemble target/SoundRouter.driver (Contents/Info.plist, Contents/MacOS/SoundRouter), `codesign --force --sign "${SIGN_ID:--}" `. install-driver-dev.sh (user runs with sudo): copy to /Library/Audio/Plug-Ins/HAL, `killall coreaudiod`.
Never copy BlackHole (GPL). Apple headers + docs + libASPL (MIT) only.
Acceptance: M0 + M1 rows of the plan. You cannot sudo: run build, codesign --verify, `nm -gU` shows the factory symbol, and unit-test the ring + JSON diff. List the sudo steps for the user.
Do not edit outside your files.
```

### Slice B — Core (owns `crates/sr-core/src/{mixer,meters}.rs`, `crates/sr-core/tests/**`; config.rs/backend.rs/driver_proto.rs are frozen)
```
Implement MixPlan/Mixer/MixerHandle/Meters exactly as specified in local://soundrouter-plan.md §5. Also implement Config::validate in config.rs. You may add a body only; signatures are frozen.
Pure Rust: deps serde, serde_json, rtrb 0.4.1, anyhow. No OS deps.
Mixer::process is called from a CoreAudio realtime thread: no alloc, no locks, no panics on bad sizes (clamp to MAX_CH/MAX_FRAMES). Plan swap via rtrb queue capacity 4, old plans returned on a second rtrb for drop on the control thread. Meters: per-channel peak |x| as AtomicU32 f32 bits, fetch_max-style update; take() swaps to 0 and maps indices to names via IoLayout.
Tests: config JSON round-trip of the example; validate rejects dangling wire, dup id, ch out of range; mixer: unity wire, -6 dB ≈ 0.501, mute, fan-out 1→2, monitor map + volume, disabled monitor silent, None layout slots ignored, plan swap applies at the next block; an alloc-counting #[global_allocator] test asserts 0 allocs in 1000 process() calls.
Acceptance: `cargo test -p sr-core` and `cargo clippy -p sr-core -- -D warnings` clean.
```

### Slice C — Engine + CLI (owns `crates/sr-audio/**`, `crates/sr-cli/**`)
```
Implement the CoreAudio engine (crate sr-audio) and the CLI (crate sr-cli, bin sr-cli). Use objc2-core-audio 0.3.2 [AudioHardware], objc2-foundation/-core-foundation 0.3. Do NOT use coreaudio-rs or cpal.
Engine implements sr_core::backend::Backend:
- apply(cfg): (1) push driver list: find plug-in via kAudioHardwarePropertyTranslateBundleIDToPlugIn(PLUGIN_BUNDLE_ID), set PROP_DEVICES with CFString JSON of enabled devices; wait ≤2 s for each UID to appear (kAudioHardwarePropertyTranslateUIDToDevice). (2) For each enabled vdev, build one private aggregate {uid:"soundrouter.agg.<id>", private:1, master+clock: vdev uid, subdevices:[vdev, each Device source uid (drift 1), each enabled monitor uid (drift 1)] deduped, taps:[one private CATapDescription per App source (drift 1)], tapautostart:0}. Input buffers = subdevices in order then taps; outputs = subdevices in order (proven). Build IoLayout from kAudioDevicePropertyStreamConfiguration; the vdev input slot = None. (3) One IOProc per aggregate: deinterleave into preallocated planar scratch, Mixer::process, interleave back. Rebuild the aggregate only when its member set changes; otherwise just MixerHandle::set_plan.
- App sources: resolve bundle_id → process objects via kAudioHardwarePropertyProcessObjectList + kAudioProcessPropertyBundleID. Tap = initStereoMixdownOfProcesses(all PIDs of that bundle), private, muteBehavior CATapUnmuted (the default; the user still hears the app on its own output). Muted-when-tapped is a later per-source option. Listen for process-list changes and re-tap.
- Listeners for device-list changes → re-apply. Destroy all aggregates and taps on Drop.
CLI (clap): `list` (hw + apps), `driver get|set <json>`, `loopback-test <uid> [--secs]`, `run <config.json>` (applies, prints meters at 4 Hz until Ctrl-C), `meter <device_id>`.
Acceptance: M1 loopback-test, M2 and M3 rows (needs installed driver + TCC → document the manual steps). Hardware tests are #[ignore]. `cargo build -p sr-cli` and clippy are clean in CI without a driver.
```

### Slice D — GUI (owns `app/**`)
```
eframe 0.33.3 + egui-snarl 0.9.0 (or the pair OpusGui recommends) app bin "SoundRouter". Depends on sr-core (Backend, Config) and sr-audio (Engine::new()). `--demo` flag uses an in-app fake Backend with synthetic meters, so the UI is testable without the driver.
Layout as in Rogue Amoeba Loopback: left sidebar = virtual devices (enable toggle, name editable, volume slider, mute, +/− buttons). Main pane = node graph for the selected device: Source nodes (left; one output pin per channel; add via picker listing Backend::apps() + hardware() inputs), one "Output Channels" node (middle; channels pins in + out; +/− channel pairs), Monitor nodes (right; one input pin per hw channel; picker from hardware() outputs). Wires = bezier pin-to-pin, mapped to Wire / MonitorWire. Per-pin dB meters (peak, −60..0, 30 Hz repaint, peak-hold 1 s) from Backend::meters.
Every edit → Config::validate → Backend::apply → save atomically to ~/Library/Application Support/SoundRouter/config.json (write tmp + rename). Load on start. Closing the window quits the app and stops routing (shortcut: no background helper until after M6).
Acceptance: M4 row. `cargo run -p soundrouter-app -- --demo` works with no driver. Screenshot it.
```

### Slice E — Packaging + CI (owns `packaging/{bundle-app.sh,make-pkg.sh,make-dmg.sh,Info-app.plist}`, `.github/workflows/ci.yml`, `README.md` build section)
```
1. bundle-app.sh: cargo build --release -p soundrouter-app -p sr-cli → target/SoundRouter.app (Contents/MacOS/SoundRouter, sr-cli; Info.plist with CFBundleIdentifier com.ronncc.soundrouter, LSMinimumSystemVersion 14.2, NSMicrophoneUsageDescription, NSAudioCaptureUsageDescription). codesign --force --options runtime --sign "${SIGN_ID:--}" (--options runtime only when SIGN_ID set; hardened runtime then needs entitlement com.apple.security.device.audio-input).
2. make-pkg.sh: pkgbuild of SoundRouter.driver → /Library/Audio/Plug-Ins/HAL with postinstall `killall coreaudiod`; productbuild combined with app → /Applications. productsign when INSTALLER_ID is set.
3. make-dmg.sh: hdiutil create. notarize.sh: `xcrun notarytool submit --keychain-profile "$NOTARY_PROFILE" --wait` + `xcrun stapler staple` (CLT has both).
4. ci.yml: runs-on macos-14. Steps: fmt --check, clippy -D warnings, cargo test --workspace, build-driver.sh, bundle-app.sh, codesign --verify. Upload app/driver artifacts. Optional job `driver-smoke` (continue-on-error): sudo install driver, killall coreaudiod, system_profiler grep (hosted runners have passwordless sudo; whether coreaudiod loads plug-ins there is UNVERIFIED).
Acceptance: workflow green on a PR. Scripts run locally ad-hoc without SIGN_ID.
```

## 8. Needs the user
1. **sudo** (each driver install/update): `sudo cp -R target/SoundRouter.driver /Library/Audio/Plug-Ins/HAL/ && sudo killall coreaudiod`. Remove with `sudo rm -rf /Library/Audio/Plug-Ins/HAL/SoundRouter.driver && sudo killall coreaudiod`.
2. **TCC:** Microphone (any input, including the vdev input in the aggregate) + "System Audio Recording" (taps), granted to SoundRouter.app on first run. Ad-hoc re-signing changes the code identity, so grants reset on every rebuild. Fix: create a self-signed "Code Signing" cert in Keychain Access (no sudo, free) and export `SIGN_ID="SoundRouter Dev"`.
3. **Distribution only:** Apple Developer Program ($99/yr), Developer ID Application + Installer certs, `xcrun notarytool store-credentials`. Full Xcode is **not** needed.
4. **Intel/universal builds:** Homebrew rust has only aarch64. Use `rustup` toolchain + `rustup target add x86_64-apple-darwin` (no sudo), or ship arm64-only.
5. Optional: `brew upgrade rust` (≥1.95) to move to eframe 0.36/egui-snarl 0.12.

## 9. Corrections to the old plan
- Changed: tap default `muteBehavior` muted-when-tapped → unmuted, because muting silences the app's own output unless the user opts in. Mute becomes an explicit later per-source option.
- Wrong framing: "apps cannot pick a new output device" → the real need is a selectable **input** carrying our mix.
- Over-scoped: rtrb audio rings, rubato, coreaudio-rs AudioUnits, dense 64×64 matrix → a single aggregate IOProc + sparse wire list.
- Missing: mic TCC, pass-thru feedback, per-device graph schema (old schema was flat/global), driver persistence via host storage, private-tap-needs-private-aggregate, objc2 lacks the plug-in vtable, Homebrew rust lacks x86_64, eframe version stale, GPL on BlackHole.
- "CreateDevice reachable?" is moot: the custom property + PropertiesChanged path is used instead.

## 10. Risks
| Risk | Gate | Fallback |
|---|---|---|
| ad-hoc driver refused by coreaudiod | M0 | self-signed cert. Then Developer ID |
| client Set of plug-in custom property not forwarded | M1 | the driver reads only its storage + a fixed 8-device pool with enable flags set by property on each *device* object; last resort is a config file + coreaudiod restart |
| aggregate with taps + many subdevices glitches / sample-rate mismatch | M2/M5 | one aggregate per source group + rtrb + SRC (the old plan's design) |
| taps from `sr-cli` in Terminal not permitted (TCC attribution) | M3 | run sr-cli from inside SoundRouter.app/Contents/MacOS |
| engine only runs with the app open | by design | LaunchAgent helper post-M6 |
