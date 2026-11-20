# Architecture

| Part | Role |
|------|------|
| `crates/sr-core` | Pure Rust: config, validation, mixer, meters, `Backend` trait, driver protocol. |
| `crates/sr-audio` | CoreAudio engine: taps, aggregates, IOProc, driver control client. |
| `crates/sr-cli` | Headless tool over the engine. |
| `driver` | AudioServerPlugIn cdylib (`SoundRouter.driver`). |
| `app` | eframe/egui GUI; talks to a `Backend` on a worker thread. |

## Companion-device design

For each virtual device V (what apps pick) the driver also publishes a hidden companion C (uid = V's uid + `COMPANION_SUFFIX`, `.engine`). Two rings connect them:

- V.out -> ring A -> C.in: what apps play into V.
- C.out -> ring B -> V.in: what apps record from V.

The engine uses C, never V, so it can read the apps' mix and write the routed mix without feeding back into itself. While no client runs I/O on C (engine not attached), V's input reads ring A instead, so apps that play into V hear themselves on V's input (Soundflower-style); once C runs, V's input is ring B. The switch is decided per I/O cycle from C's client count.

### Volume and mute on V

V (not C) publishes a volume and a mute control on each scope (4 control objects per slot, listed in `ControlList` and `OwnedObjects`; ids start at `CTRL_BASE`, after the device/stream ids). Volume is a `'vlme'` level control with a -64..0 dB range; scalar is linear in dB (0 -> -64 dB, 1 -> 0 dB, i.e. `convert` is exact and reversible) and the bottom of the range is exact silence. Defaults are 0 dB, unmuted; a slot reused for a new device resets them. Values are not persisted.

The gain is applied when reading, in `do_io`, from one precomputed `f32` per scope (the dB-to-gain conversion runs on the property-set path, so the real-time path does one atomic load and one multiply, no allocation). Samples are stored in the rings untouched:

| Read | Gain |
|---|---|
| C.in (engine, ring A) | V output level |
| V.in, C idle (direct loop, ring A) | V output level x V input level |
| V.in, engine running (ring B) | V input level |

Unity (0 dB, unmuted) skips the multiply, so the default path stays bit-exact; mute writes exact zeros. C has no controls and no gain.

### Overload counter

`WriteMix` calls that arrive late (scheduled before the device's current sample time) or larger than half the ring are counted per device in a relaxed atomic (no logging on the real-time path). A `ReadInput` whose read-ahead (next section) reached past the written frames is counted on the reading device too. The count is exposed read-only as custom property `'srov'` (CFNumber, 64-bit) on V and on C and resets when the slot is reused. Late writes are still written.

### Loop read-ahead (`'srla'`, off by default)

Through a ring a frame waits about two buffers: the writer stores it at output time ~now+B and the reader asks for it at input time ~now-B on the same sample timeline, so the frame has been sitting in the ring for up to two buffers when it is read. With read-ahead the reader reads `ahead` frames later on the timeline (`ring.read(ch, t + ahead, buf)`) and the app or engine gets each frame `ahead` frames sooner. Every ring traversal is shortened by `ahead`; the engine path (V.out -> C.in, C.out -> V.in) has two, the direct V loop one. Blackhole, BGM and eqMac read at `mInputTime` with no read-ahead.

How it stays safe (`ring::ReadAhead`, one per reading device; `do_io` calls `plan` once per `ReadInput`):

- **Measure.** `Ring::slack(t, frames)` = `end - (t + frames)`: written frames past the end of this read. Over the first 100 read cycles of a stream (more when writer and reader buffers differ: `writer_frames / gcd(writer_frames, frames)`, the cycles after which the two positions repeat; beats over 1024 cycles get no read-ahead) the reader reads at `t` (no read-ahead) and keeps the smallest slack. The writer's buffer size is the `frames` of its last `write`, stored in the ring.
- **Freeze.** `ahead = clamp(min_slack - guard, 0, 4 * max(frames, writer_frames))` with `guard = max(16, frames / 8)`, then it never changes: a moving `ahead` replays or drops audio.
- **Shortfall.** A read whose `t + ahead + frames` is past `end` gets silence for the frames beyond it (the ring only ever returns frames below its release-stored `end`) and is counted in `'srov'`. A second shortfall without 65536 clean cycles in between halves `ahead` (the one back-off step); a lone one, such as a writer thread stalled once, changes nothing.
- **Restart.** Calibration starts over (`ahead` back to 0) when I/O stops on either side of the slot, when the reader's or writer's buffer size changes, when `t` jumps (not 0..4 cycles forward), when the ring is empty or was reset (format changes and slot reuse reset it), and when the switch is flipped.
- **Order of calls.** The ring is only read below `end`, so reading ahead cannot return data that was never written. What the writer's phase can change is slack: when V's and C's threads wake together, the reader sometimes runs before and sometimes after the writer's cycle lands (slack differs by one writer buffer). Calibration normally sees the lower of the two (reader first) within 100 cycles and sizes `ahead` for it. If the order flips only rarely, calibration can miss the lower slack: then the first flip is a shortfall (a silent tail of up to `ahead - slack` frames, counted), the second halves `ahead` below the lower slack and the stream is clean afterwards (never more than two in the model, `ring.rs` tests).
- **Real-time rules.** `plan` and `do_io` use relaxed atomics only: no allocation, locks or logging.

The switch is the custom property `'srla'` on the plug-in object (`'plst'` CFNumber, settable, nonzero = on; `READ_AHEAD_DEFAULT` = off, not persisted, so whoever wants it writes it after every driver start). Flipping it restarts every calibration. It is off because it has only run against the HAL model in the unit tests and `packaging/fakehost.c`, not in a real coreaudiod: turn the default on after a live run shows `'srov'` staying put.

Frames saved per traversal with equal reader and writer buffers `B`: `B - guard` (fakehost, impulse through the HAL model: V loop 2B -> B + guard, engine loop 4B -> 2B + 2 * guard, the same with the engine's read racing V's write):

| B (frames) | per traversal | engine loop (2 traversals) | engine loop at 48 kHz, before -> after |
|---|---|---|---|
| 64 | 48 (1.0 ms) | 96 (2.0 ms) | 5.33 -> 3.33 ms |
| 128 | 112 (2.3 ms) | 224 (4.7 ms) | 10.67 -> 6.00 ms |
| 256 | 224 (4.7 ms) | 448 (9.3 ms) | 21.33 -> 12.00 ms |
| 512 | 448 (9.3 ms) | 896 (18.7 ms) | 42.67 -> 24.00 ms |
| 1024 | 896 (18.7 ms) | 1792 (37.3 ms) | 85.33 -> 48.00 ms |

Different reader and writer buffers (V's size is the client app's, the engine's the Audio delay setting) save whatever the measured slack leaves; the model above assumes equal ones. Read-ahead does not touch the aggregate's own offsets (process-tap safety offset, drift compensation). The driver must be reinstalled to get any of this.

### Clock and zero timestamps

V and C of a slot share one timeline: the same `anchor` (set when the first client on either side starts) and the same `seed`, and the same `ClockDomain`. Each device then derives its sample time from its *own* nominal rate (`GetZeroTimeStamp` must stay consistent with the rate the HAL configured on that device, so a 96 kHz V cannot report 48 kHz-period timestamps). Consequence: V and C report identical zero timestamps exactly while their rates agree. They differ only while a rate change has landed on one side; the driver asks both devices to follow, and until both do the pair is silent (`Slot::in_sync`). The HAL may run the per-device `PerformDeviceConfigurationChange` calls late and in any order, so a Perform applies the last requested rate (`rate_want`), never the rate carried in its request, and every new rate asks both devices again (`Slot::want`, `Slot::perform`; channel counts work the same way). Tests: `v_and_c_report_identical_zero_timestamps_at_one_rate` (identical at one rate), `zero_timestamp_uses_the_devices_own_rate` (diverge at different rates), `format_converges_to_the_last_set` (any Perform order, rates and channel counts), and the fakehost zero-timestamp and rapid-rate checks.

### Default-device flags

- V: `CanBeDefaultDevice` = 1 (both scopes; users and apps choose it as input/output), `CanBeDefaultSystemDevice` = 0. Alerts and sound effects should not be swallowed by a loopback nobody may be listening to; choosing V as the system device in Sound settings is left out on purpose.
- C: both 0, and C is hidden: it is the engine's private endpoint and must never become a default or appear in pickers.

```mermaid
flowchart LR
  Apps -->|play| V[Virtual device V]
  V -->|ring A| C[Hidden companion C]
  subgraph E[Engine, one per device]
    direction LR
    AGG[Main aggregate:<br/>C + input devices + monitors] --> IO[IOProc] --> MIX[Mixer]
    TAGG[Tap aggregate:<br/>C as clock + process taps] --> TIO[Tap IOProc] -->|SPSC ring| IO
  end
  C --> AGG
  C --> TAGG
  MIX -->|C.out, ring B| V2[V input]
  MIX --> Mon[Monitor outputs]
  V2 -->|record| Rec[Zoom, OBS, ...]
```

## Aggregate, IOProc, mixer

Per enabled virtual device the engine builds a private aggregate: the companion is the clock source; input devices and monitors join with drift compensation. One IOProc reads all input buffers (subdevices in list order), the mixer applies routing, gain and mute, and it writes C's output plus the monitor outputs. Shape mismatches are counted, not trusted (`layout_inputs`, `check_width`); layouts wider than 64 channels per direction fail the check.

### Tap aggregate (split)

Each app source adds a process tap, and any process tap in an aggregate raises that aggregate's safety offset to 1024 frames in each direction for every path in it (see the latency model). So the taps live in a second private aggregate of the same device (`engine/tapagg.rs`): only the companion (main, clock, drift off: the same clock as the main aggregate) and the taps. Its IOProc copies the tap buffers (the last N input buffers of its list, frame-major, all taps' channels back to back) into a lock-free SPSC ring (`tapring.rs`, `rtrb`, preallocated, 8 cushions deep, at least `MAX_FRAMES` frames) and writes nothing to C's output. The main IOProc pops from that ring into the mixer's tap input slots, behind its own input channels: it asks the mixer's current plan how many inputs it expects (`Mixer::in_len`) and supplies exactly that many, silence for the tap slots whenever the ring in hand has another channel count. So the plan and the ring can swap in either order, and mic, pass-thru and monitor paths never see a mismatch or a gap while taps come and go.

- **Cushion and counters.** The first read waits until a block plus a cushion (one IO buffer; `SR_TAP_CUSHION=<frames>` overrides it) is buffered, then free-runs: both IOProcs run on the companion's clock, so there is no drift to correct. An empty ring gives one block of silence, counts an underrun and waits for the cushion again; more than three blocks beyond the cushion drops the oldest frames down to it (a trim); a full ring makes the producer drop the block (`dropped`). `Engine::tap_counters` / `sr-cli run` show them next to `glitches`.
- **Rebuild.** Adding or removing a tap rebuilds the tap aggregate only (`rebuild_tap_agg`): the new one is built and started, its ring is published to the main IOProc through a one-slot mailbox (the mixer plan's pattern, retired rings come back on a queue), the layout is republished, then the old aggregate is stopped and destroyed together with the taps that left. The tapped apps are silent from that swap until the new aggregate runs (tens of ms); everything else keeps running. A process that only gains helper processes just updates the tap description in place.
- **Start and permission.** `AudioDeviceStart` of the tap aggregate blocks while the System Audio Recording prompt is open; it runs on its own starter thread. The main aggregate starts at once, so a mic or pass-thru route is not held by the prompt; the device reports `WaitingForPermission` only while its tap aggregate is the one waiting. A failed tap start rebuilds the whole vdev (bounded like any failed start).
- **Teardown order.** tap IOProc stopped, main IOProc stopped, tap aggregate destroyed, main aggregate destroyed, then the taps. A tap aggregate still starting is torn down on a reaper thread.
- **Rates.** The tap aggregate follows the companion's rate; a rate or layout change of the companion or any sub-device rebuilds the whole vdev as before.
- **Switch.** `SR_TAP_SPLIT=0` builds the old single aggregate with the taps inside (A/B); `SR_TAP_SPLIT=1` forces the split. The split is part of the reuse key, so changing it rebuilds the vdev.

### Audio delay setting

The engine path adds two of the aggregate's IO buffers (input and output side; added latency = two buffers) on top of the driver's own loop. `Config::buffer_frames` (one of 64, 128, 256 or 512; default 256, older configs without the field load as 256) is set on each aggregate with `kAudioDevicePropertyBufferFrameSize` right before its IOProc is created, after checking `kAudioDevicePropertyBufferFrameSizeRange`; a size outside the range or a HAL error keeps the aggregate's default and is logged at info. Changing it rebuilds the aggregates (a short dropout). The app sets it from the footer's **Delay** button (an **Audio delay** menu of Lowest, Low, Balanced (recommended) and Safest = 64, 128, 256 and 512 frames, each showing the added delay, two buffers: 2.7, 5.3, 10.7 or 21.3 ms at 48 kHz), `sr-cli run --buffer N` overrides the config. Smaller buffers wake the IOProc more often: more CPU and battery, and less headroom before a busy system makes the HAL skip a cycle (counted by `Engine::glitches`).

Measured on an M3 Max (macOS 14.7.1, 48 kHz, installed driver, Pass-Thru route, `srctl loop soundrouter:v1 128` = V.out -> companion -> engine -> V.in): the driver alone loops in 5.33 ms; with the engine at 512 / 256 / 128 / 64 frames the loop takes 26.67 / 16.00 / 10.67 / 8.00 ms, i.e. the engine adds two of its buffers (input and output side). With 8 `yes` CPU burners running, `Engine::glitches` stayed at 0 at every size.

### Latency model (verified)

What the engine adds to a route is the distance its IOProc reports: `output_time.mSampleTime - input_time.mSampleTime` of the same cycle, stored per cycle in `Shared::delay` (one relaxed store) and exposed as `Engine::delay(id)` and `EngineStatus::delays` (frames plus the main device's rate, `Delay::ms`). `sr-cli run` prints it ("delay 5.3 ms") and the app can show it from the status; nothing is estimated from the buffer setting. On an M3 Max at 48 kHz with the installed driver, a route from V through the engine and back to V satisfies `loop = B_V + added`, where `B_V` is the buffer the client app asked V for and `added` is the reported distance (Pass-Thru route, engine at 128 / 512 / 64 frames: loop 8.00 / 32.01 / 4.00 ms with `B_V` = 128 / 512 / 64 frames, reported 5.3 / 21.3 / 2.7 ms).

- **Buffers.** Without process taps `added` is two engine buffers (input and output side), as above. A buffer size requested from the engine process for V or C changes neither a client's IO cycle (the HAL keeps one per client: a client that never asks gets 512) nor the loop, so the engine only sets its aggregate's size and `B_V` stays the client app's choice.
- **Process taps.** Any process tap in an aggregate adds a fixed 1024-frame safety offset to the input side and another to the output side of that aggregate, whatever the buffer size or rate, and it applies to every path through it (mic, pass-thru, monitors too, not only the tapped app). Measured with one app source in the single aggregate (`SR_TAP_SPLIT=0`): the reported distance is 48.0 ms at 128 frames, 45.3 ms at 64 and 53.3 ms at 256, against 5.3 / 2.7 / 10.7 ms without the tap (+42.7 ms at 48 kHz).
- **Split taps.** With the taps in their own aggregate (see "Tap aggregate (split)") the main aggregate keeps the no-tap numbers while apps are tapped: pass-thru, mic and monitor paths report 5.3 / 2.7 / 10.7 ms at 128 / 64 / 256 frames, and `srctl loop soundrouter:v1 <B>` with a tapped player running takes 10.66 ms at 128 (53.34 ms unsplit) and 5.33 ms at 64 (48.00 unsplit). The tapped apps still carry one safety offset (on the tap aggregate's input side only) plus the ring: `EngineStatus::tap_delays` / `Engine::tap_delay(id)` report it as how stale the audio is when the tap IOProc gets it (`now - input time`), plus the frames left in the ring after the last read, plus how far the main output lies ahead of the clock (`Shared::lead`): 29.3 / 25.4 / 37.3 ms at 128 / 64 / 256 frames, i.e. 1024 + 3 buffers (1408 frames at 128). It leaves out the wait for the main IOProc's phase (up to one buffer) and the driver's own hops. End to end, an impulse played by a tapped process into `soundrouter:v2` and detected on `soundrouter:v1`'s input (host timestamps of both buffers) takes 50.7 / 47.6 / 59.1 ms split against 68.7 / 67.3 / 71.3 ms unsplit at 128 / 64 / 256 frames: the tapped route gains 17.9 / 19.8 / 12.2 ms, every other route 42.7 ms. `Engine::delay(id)` and `EngineStatus::delays` report the longer of the two paths (what the slowest source costs), `tap_delays` only the app one, so a UI can say "Mic and Pass-Thru: X ms, apps: Y ms".
- **Drift compensation.** Input devices and monitors join with drift compensation, which has a fixed resampler delay per hop and direction. The sub-device dict sets `kAudioSubDeviceDriftCompensationQualityKey` to `kAudioAggregateDriftCompensationLowQuality` (0x20): measured one way through BlackHole 16ch as a monitor and as an input device, drift off / default quality / Low quality add 0 / 0.98 / 0.17 ms (about 47 / 8 frames), so Low saves 0.8 ms per direction and costs 0.17 ms against no compensation. The reported distance does not include this resampler delay (it is not in the timestamps): add about 0.2 ms per drift-compensated device on the path.
- **`drift` must be a CFNumber.** The HAL stores a CFBoolean `drift` value as 0 (read back from `kAudioAggregateDevicePropertyComposition`), so before this the aggregates never compensated drift at all. The engine now passes `NSNumber::new_i32`. The tap dict's `kAudioSubTapDriftCompensationKey` still passes a CFBoolean (unmeasured: needs a tapped process).
- **Low Power Mode and App Nap.** `EngineStatus::low_power_mode` reports `NSProcessInfo.isLowPowerModeEnabled`, because lower clocks make the small buffers glitch sooner. While any virtual device's IOProc exists the engine holds one `NSProcessInfo` activity (`NSActivityUserInitiatedAllowingIdleSystemSleep | NSActivityLatencyCritical`, reason "SoundRouter routing", `activity.rs`) so App Nap cannot throttle the process; it begins with the first device and ends with the last one dropped.

## Control property `srcf`

The app sets a custom property `'srcf'` on the plug-in object (type `'plst'`): a CFArray of `{uid, name, channels}`, at most 16 entries; every uid must start with `soundrouter:`. Any local process can set the property (the driver does not check the caller), so it validates everything and keeps the list out of other devices' uid namespace; a hostile process can still remove or rename the user's virtual devices, but cannot read audio (the Microphone permission gates their input). The driver diffs the list, adds or removes devices at runtime (no coreaudiod restart), requests configuration changes for channel changes (both devices; each Perform sets its own device to the count the list wants now, `ch_want`, never the count in its request, so late or reordered Performs such as 2 → 8 → 16 → 8 still end at 8 on V and C; until both land the pair is silent), and persists the list through host storage so devices survive reboot. A second read-only property reports the protocol version. See `crates/sr-core/src/driver_proto.rs`, `sr-audio/src/driver.rs`.

## Threading rules

- IOProc (realtime): no allocation, locks, logging or ObjC. State arrives through atomics and a newest-plan mailbox; a test asserts the mixer does not allocate.
- `AudioDeviceStart` can block on the TCC prompt, so it runs on a starter thread; dropping a device joins that thread first, and `retire()` keeps callers from blocking.
- GUI thread never touches CoreAudio: a worker thread owns the backend and posts status and meters back.
- Driver entry points abort on panic and never block on the audio path.
