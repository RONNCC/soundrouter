# Why the delay cannot be cut further by faster code

Short answer: the delay is made of whole audio buffers and fixed safety offsets that the system sets. Our code is not in that sum, so making it faster (SIMD, Accelerate, GPU, Neural Engine) changes the delay by 0 ms. What does change it is in section 5.

All numbers are at 48 kHz. One frame is 0.0208 ms, so 64 / 128 / 256 frames are 1.33 / 2.67 / 5.33 ms. Measured on an Apple M3 Max (Mac15,9), macOS 14.7.1, 2026-11-21, commit cc0b804. "Measured here" means re-run for this note; "cited" means taken from [ARCHITECTURE](ARCHITECTURE.md#latency-model-verified), measured there with the installed driver (which was not installed on this machine today). Commands are in the appendix.

## 1. Where every millisecond goes

| Path | 64 | 128 | 256 | Made of |
|---|---|---|---|---|
| A. App writes to a virtual device, driver loops to the reader (no engine) | 2.7 | 5.3 | 10.7 | Two client buffers: the write lands one buffer ahead, the read is one buffer behind. 5.33 ms at 128, cited. |
| B. Pass-thru or mic through the engine (added by the engine) | 2.7 | 5.3 | 10.7 | Two engine buffers (input side and output side), cited. Full loop V to engine to V with 128-frame clients: 8.0 / 10.7 / 16.0 ms. |
| C. Captured app (process tap), engine-reported | 25.3 | 29.3 | 37.3 | 21.3 ms fixed HAL safety offset (1024 frames) plus 3 buffers: 21.3 + 3 x (1.33 / 2.67 / 5.33) matches all three. The 3 buffers are staleness at the tap, the ring cushion and the output lead, split from the cited text, not re-measured separately. |
| C. Same, end to end (impulse in, impulse out) | 46.7-48.0 | 50.5-52.2 | 59.5-60.1 | Includes the test's own two 128-frame client buffers (5.3 ms) and about 16 ms not attributed to a single stage. Cited. |

What was tried on path C and gave nothing (cited, all in ARCHITECTURE "Tap path experiments"): sub-tap `latency-in` / `latency-out` of -1024, -512 and +512 (37.3 ms before and after; the HAL accepts the key and ignores it), tap drift compensation on or off (37.3 vs 37.4 ms, same end to end), and an adaptive resampler (not built: the ring is one buffer and the 1024-frame offset is 57 % of the path, so it could only add delay and CPU). Cushion 0 saves 5.3 ms at 256 frames but one late cycle then drops a whole block, so the default is one block.

Hardware adds its own fixed part that we do not control. Measured here with `devices`: MacBook Pro speakers safety offset 74 + latency 70 frames (3.0 ms); MacBook Pro microphone 50 frames (1.0 ms); an LG monitor over DisplayPort 320 + 88 frames (8.5 ms). BlackHole and the Teams driver report 0.

## 2. Compute is not on the critical path

Time for one `Mixer::process` call, measured here (best of 5 rounds, 3 runs, worst run shown), against the time between callbacks:

| Frames | Period | Stereo mix | % of period | 64-channel mix | % of period |
|---|---|---|---|---|---|
| 64 | 1333 us | 0.12 us | 0.009 % | 8.0 us | 0.60 % |
| 128 | 2667 us | 0.21 us | 0.008 % | 9.9 us | 0.37 % |
| 256 | 5333 us | 0.40 us | 0.007 % | 17.0 us | 0.32 % |

Per-wire trim (NODES step 1) gives each tap a second ramped gain, `g.cur * s0 * wire.cur`; measured against the commit before it, same machine, back to back: stereo 114 vs 114 ns, 64-channel mix 7.6 vs 7.7 us at 64 frames, 8.3-9.7 vs 8.3 us at 128, 16.1 vs 16.4 us at 256, so within run-to-run noise.

Mixer nodes (NODES steps 3 and 4), measured the same way (best of 5 rounds, 3 runs, worst run shown). 16 stereo buses, the cap of 32 node channels, over the plain 32-channel mix, then every node also running mono, a 2-band EQ and a limiter, four of them (the 8 delay channels) a 5 ms delay as well:

| Frames | Period | 32-channel mix | + 16 buses | % of period | + mono, EQ, limiter, delay on 4 | % of period |
|---|---|---|---|---|---|---|
| 64 | 1333 us | 3.1 us | 4.3 us | 0.32 % | 15.2 us | 1.14 % |
| 128 | 2667 us | 4.5 us | 5.9 us | 0.22 % | 31.7 us | 1.19 % |
| 256 | 5333 us | 8.0 us | 10.8 us | 0.20 % | 65.1 us | 1.22 % |

The review gate of NODES section 1 is 2 % of the period at 64 frames (27 us): met at 1.14 %. The EQ is most of it. Each effect over one stereo node (`bench_effects`, ns per call, 64 / 128 / 256 frames): mono 14 / 15 / 25; limiter 127 / 242 / 536 below the knee and 526 / 741 / 1249 while clipping; EQ, 2 bands 518 / 1334 / 2963; delay 27 / 35 / 56 and 145 / 288 / 575 while retiming. The EQ on subnormal input costs the same as on normal input (911 ns against 911 ns at 64 frames), so the denormal sweep is for the tail and other targets, not for speed here.

The device asks for one buffer every period on its own clock and plays what we hand back at the next period. Work only has to finish before that deadline. Finishing at 17 us or at 0.017 us (1000x faster) delivers the same buffer at the same sample time: latency = buffer count x buffer length + fixed safety offsets, with no compute term. Even the worst case here uses under 1 % of the deadline, so the only thing faster math buys is headroom, as the changelog for e7d390d says ("0 ms by design").

## 3. Vectorization

- Already done. `ramp_add` (gain ramp with finite check and clamp) is auto-vectorised by LLVM: the release assembly of `Mixer::process` has 120 NEON `.4s` instructions, including `fmul.4s`, `fcmgt.4s` and `bsl` for the clamp and select. The meter peak is an 8-lane branch-free loop. Commit e7d390d made one mixer block 2x (stereo) to 2.4x (64 channels) cheaper (cited). A hand-chunked scratch kernel tried earlier by an agent was slower than the auto-vectorised loop (reported by that agent, not re-run).
- Accelerate / vDSP adds nothing at these sizes. Per channel, measured here with a C stand-in of the same operation (`vdsp`):

| Frames | Loop with finite + clamp | Plain loop, no clamp | `vDSP_vsma` | `vDSP_vclip` + `vsma` |
|---|---|---|---|---|
| 64 | 9-11 ns | 4.6-5.3 ns | 5.9-6.3 ns | 10 ns |
| 128 | 18 ns | 7.3 ns | 9.2-10.1 ns | 15 ns |
| 256 | 36-38 ns | 13.7-15.2 ns | 16.1-17.2 ns | 28-29 ns |

  vDSP is slower than a plain compiled loop (fixed call cost on 64 to 256 floats), and it has no "treat NaN or inf as silence" step, which the mixer needs so one bad source cannot poison the output. Even a perfect 2x on all 64 channels would save about 8 us at 256 frames (0.15 % of the period) and 0 ms of delay.
- AMX and SME: not measured, not worth it. They target matrices and large vectors; a 64 to 256 sample multiply-add is already a handful of NEON instructions, and entering a different execution unit costs more than the work.

## 4. GPU, Metal, Neural Engine

Round trip of one tiny Metal compute kernel (128 floats, encode, commit, wait for completion), measured here with `metal` (two runs, 5000 calls each):

| Case | Median | 99th percentile | Worst |
|---|---|---|---|
| Busy loop, 64 / 128 / 256 frames | 116-124 us | 299-667 us | 1.2-3.6 ms |
| After 50 ms idle, 128 frames | 0.6-1.0 ms | 1.0-10 ms | 10 ms |

That empty kernel already costs 15x the whole 64-channel CPU mix (8 us) and about 1000x the stereo mix, before any real math, and its worst cases exceed a 64-frame period (1.33 ms). The GPU also powers down between calls (the idle row), has no real-time scheduling guarantee, and shares the device with the window server; an IOProc that waits on it can miss its deadline and glitch. The Neural Engine is a batch inference path with the same kind of dispatch cost and no audio deadline support; not measured here. Neither can shorten the delay, because the delay is not compute (section 2).

## 5. What does reduce the delay, and what it costs

| Change | Saves | Cost or limit |
|---|---|---|
| Smaller engine buffer (footer **Delay** button; 64 is the minimum) | Path B: 10.7 ms at 256 to 2.7 ms at 64. Path C: 37.3 to 25.3 ms. | More wakeups, more CPU and battery, crackle risk, worse in Low Power Mode (the app reports it). 8 CPU burners gave 0 glitches at every size in the cited test; a busier real session may differ. |
| Higher sample rate (96 kHz) | Halves the ms per frame: a 64-frame buffer is 0.67 ms, the 1024-frame tap offset 10.7 ms. Reasoned from frame counts, not measured. | The device must support it; twice the CPU per second; every app on the route should match. |
| Devices with small safety offsets (class-compliant USB interfaces, the built-in mic and speakers) | Each device's own offset is in the `devices` listing above, 1 to 8.5 ms here. | You choose hardware; some devices report large offsets you cannot change. |
| Keep drift compensation at Low quality (already so) | Default quality adds 0.98 ms per hop, Low adds 0.17 ms, off adds 0 (cited). | Off was never shown safe between two real clocks. About 0.2 ms per compensated device remains. |
| Avoid taps for latency-critical sources: play that app straight into the virtual device | Path C to path A or B: roughly 25 to 37 ms down to 3 to 11 ms. | The app must let you pick its output device. |
| Driver read-ahead `srla` | Estimated 2 to 9 ms per round trip (one `ahead` per ring traversal). | Off by default, only checked in unit tests and the fake host, not live; an underrun costs a silent tail. |
| **Measure delay** button | Nothing by itself: it shows the reported distance so you can see what a change did. | None. |

"Mute original" is not a delay setting: it does not change the buffers or offsets in section 1.

## 6. What cannot be changed

- HAL safety offsets: the 1024-frame tap offset (21.3 ms) is fixed by CoreAudio and applies to every path through an aggregate that has a tap; latency keys do not move it (section 1).
- Device buffers and offsets reported by hardware (section 1 list); we can only pick a different device.
- Bluetooth: the codec and radio add roughly 100 to 200 ms or more on their own, outside the Mac (typical figure from the Bluetooth audio codecs, not measured here).
- USB and other transports: the device reports its own latency and safety offset (section 1 listing); we can read them but not change them.

## 7. Rule of thumb

Delay is roughly (number of buffers x buffer frames + safety offsets) / sample rate. To cut it: use fewer or smaller buffers, stay off taps for anything you play along to, and pick low-offset hardware. Faster code, SIMD or a GPU will not move it.

## Appendix: how each number was produced

Machine: Apple M3 Max, macOS 14.7.1, 2026-11-21, commit cc0b804, `PATH="$HOME/.cargo/bin:$PATH"`. Sources of the three small programs are in [latency-bench](latency-bench/).

```
cargo test --release -p sr-core --test bench_mix -- --ignored --nocapture    # section 2, run 3 times
cargo test --release -p sr-core --lib -- --ignored --nocapture bench_effects   # section 2, effects, run 2 times
cargo rustc -p sr-core --release --lib -- --emit asm -C codegen-units=1       # section 3; count .4s lines after the Mixer::process label in target/release/deps/sr_core-*.s
clang -O3 -o vdsp docs/latency-bench/vdsp.c -framework Accelerate && ./vdsp    # section 3 table, run 3 times
clang -O2 -fobjc-arc -o metal docs/latency-bench/metal.m -framework Metal -framework Foundation && ./metal   # section 4, run 2 times
clang -o devices docs/latency-bench/devices.c -framework CoreAudio -framework CoreFoundation && ./devices    # hardware offsets
```

Cited, not re-run today (needs the installed driver and a live audio session): path A and B numbers (`srctl loop soundrouter:v1 128` at engine buffers 512 / 256 / 128 / 64), path C engine-reported and end-to-end numbers (impulse method, `SR_TAP_SPLIT`, `SR_TAP_CUSHION`), drift cost through BlackHole 16ch, the burner glitch test. Their commands and methods are in [ARCHITECTURE](ARCHITECTURE.md#latency-model-verified). `srla` is untested live.
