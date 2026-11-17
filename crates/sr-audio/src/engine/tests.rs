use super::*;
use sr_core::config::{Monitor, MonitorWire, Source, Wire};
use sr_core::mixer::OutPort;

fn src(id: &str, kind: SourceKind) -> Source {
    Source {
        id: id.into(),
        kind,
        channels: 2,
        volume_db: 0.0,
        muted: false,
        mute_original: false,
    }
}
fn mon(uid: &str, enabled: bool) -> Monitor {
    Monitor {
        device_uid: uid.into(),
        enabled,
        volume_db: 0.0,
        wires: vec![],
    }
}
fn vdev(sources: Vec<Source>, wires: Vec<Wire>, monitors: Vec<Monitor>) -> VirtualDevice {
    VirtualDevice {
        id: "v1".into(),
        name: "n".into(),
        enabled: true,
        channels: 2,
        volume_db: 0.0,
        muted: false,
        sources,
        wires,
        monitors,
    }
}
fn wire(source: &str, src_ch: u16, out_ch: u16) -> Wire {
    Wire {
        source: source.into(),
        src_ch,
        out_ch,
    }
}
fn app(id: &str, bundle: &str) -> Source {
    src(
        id,
        SourceKind::App {
            bundle_id: bundle.into(),
        },
    )
}

#[test]
fn subs_deduped_and_filtered() {
    let dev = vdev(
        vec![
            src("a", SourceKind::Device { uid: "mic".into() }),
            src("b", SourceKind::Device { uid: "gone".into() }),
            src("c", SourceKind::PassThru),
        ],
        vec![],
        vec![mon("spk", true), mon("mic", true), mon("off", false)],
    );
    assert_eq!(plan_subs(&dev, &|u| u != "gone"), ["mic", "spk"]);
}

#[test]
fn own_devices_are_never_offered_as_hardware() {
    let vis = format!("{DEVICE_UID_PREFIX}v1");
    for own in [companion_uid(&vis), vis, format!("{AGG_UID_PREFIX}v1.3")] {
        assert!(is_own_uid(&own), "{own}");
    }
    for other in [
        "BuiltInMicrophoneDevice",
        "BlackHole2ch_UID",
        "AppleUSBAudioEngine:1:2",
    ] {
        assert!(!is_own_uid(other), "{other}");
    }
}

// ---- main_candidates ----

#[test]
fn main_candidates_order_and_presence() {
    let dev = vdev(vec![], vec![], vec![]);
    let vis = format!("{DEVICE_UID_PREFIX}v1");
    let comp = companion_uid(&vis);
    assert_eq!(
        main_candidates_in(&dev, None, &|_| true),
        [comp, vis.clone()]
    );
    assert_eq!(main_candidates_in(&dev, None, &|u| u == vis), [vis]);
    assert!(main_candidates_in(&dev, None, &|_| false).is_empty());
}

#[test]
fn main_candidates_override_wins_and_never_falls_back() {
    let dev = vdev(vec![], vec![], vec![]);
    assert_eq!(
        main_candidates_in(&dev, Some("BlackHole16ch_UID"), &|_| true),
        ["BlackHole16ch_UID"]
    );
    // Override names an absent device: no candidates, and our own devices are not tried.
    let only_ours = |u: &str| u != "BlackHole16ch_UID";
    assert!(main_candidates_in(&dev, Some("BlackHole16ch_UID"), &only_ours).is_empty());
    // An empty override is ignored.
    assert_eq!(main_candidates_in(&dev, Some(""), &|_| true).len(), 2);
}

// ---- layout_inputs / check_width ----

#[test]
fn layout_inputs_collects_roles() {
    let dev = vdev(
        vec![
            src("pt", SourceKind::PassThru),
            src("m1", SourceKind::Device { uid: "mic".into() }),
            src("m2", SourceKind::Device { uid: "mic".into() }),
            app("a", "com.a"),
        ],
        vec![],
        vec![mon("spk", true), mon("off", false)],
    );
    let li = layout_inputs(&dev);
    assert_eq!(li.passthru.as_deref(), Some("pt"));
    // Same uid twice: the first source wins.
    assert_eq!(li.device_sources["mic"], "m1");
    assert_eq!(li.device_sources.len(), 1);
    assert_eq!(li.monitors, HashSet::from(["spk".to_owned()]));
    assert_eq!(
        layout_inputs(&vdev(vec![], vec![], vec![])),
        LayoutInputs::default()
    );
}

fn layout_of(n_in: usize, n_out: usize) -> IoLayout {
    IoLayout {
        inputs: vec![None; n_in],
        outputs: (0..n_out as u16)
            .map(|c| Some(OutPort::Device(c)))
            .collect(),
    }
}

#[test]
fn check_width_names_limit_and_counts() {
    assert!(check_width(&layout_of(MAX_CH, MAX_CH)).is_ok());
    // Bug 2: a 64-channel device plus one app tap (2 more inputs) is over the limit.
    let e = check_width(&layout_of(MAX_CH + 2, MAX_CH))
        .unwrap_err()
        .to_string();
    assert!(e.contains("66 input / 64 output"), "{e}");
    assert!(e.contains("at most 64"), "{e}");
    let e = check_width(&layout_of(2, MAX_CH + 8))
        .unwrap_err()
        .to_string();
    assert!(e.contains("2 input / 72 output"), "{e}");
}

// ---- tap_plan ----

fn tap(id: u32, source: &str, bundle: &str, objs: &[u32]) -> Tap {
    Tap {
        source_id: source.into(),
        bundle: bundle.into(),
        id,
        uid: format!("tap{id}"),
        objs: objs.to_vec(),
        mute: false,
    }
}
fn group(bundle: &str, objs: &[u32]) -> AppGroup {
    AppGroup {
        bundle_id: bundle.into(),
        name: bundle.into(),
        running_output: true,
        regular: true,
        objs: objs.to_vec(),
        pids: vec![],
        members: vec![],
    }
}
fn one_app() -> VirtualDevice {
    vdev(vec![app("s1", "com.a")], vec![], vec![])
}

#[test]
fn tap_plan_steady_state_keeps() {
    let cur = [tap(1, "s1", "com.a", &[5])];
    let p = tap_plan(&cur, &one_app(), &[group("com.a", &[5])]);
    assert_eq!(
        p,
        TapPlan {
            keep: vec![0],
            ..TapPlan::default()
        }
    );
}

#[test]
fn tap_plan_app_quit_removes_then_relaunch_creates() {
    let cur = [tap(1, "s1", "com.a", &[5])];
    let p = tap_plan(&cur, &one_app(), &[]);
    assert_eq!(
        p,
        TapPlan {
            remove: vec![0],
            ..TapPlan::default()
        }
    );
    // Running again with a new pid: the (already removed) tap is created afresh.
    let p = tap_plan(&[], &one_app(), &[group("com.a", &[9])]);
    assert_eq!(
        p,
        TapPlan {
            create: vec![("s1".into(), "com.a".into(), vec![9], false)],
            ..TapPlan::default()
        }
    );
}

#[test]
fn tap_plan_relaunch_new_pid_updates_in_place() {
    // Quit and relaunch between two syncs: same source, other process object.
    let cur = [tap(1, "s1", "com.a", &[5])];
    let p = tap_plan(&cur, &one_app(), &[group("com.a", &[9])]);
    assert_eq!(
        p,
        TapPlan {
            update: vec![(0, vec![9])],
            ..TapPlan::default()
        }
    );
}

#[test]
fn tap_plan_helper_process_added() {
    let cur = [tap(1, "s1", "com.a", &[5])];
    let p = tap_plan(&cur, &one_app(), &[group("com.a", &[5, 6])]);
    assert_eq!(
        p,
        TapPlan {
            update: vec![(0, vec![5, 6])],
            ..TapPlan::default()
        }
    );
}

#[test]
fn tap_plan_config_changes() {
    let dev = vdev(vec![app("s1", "com.b"), app("s2", "com.c")], vec![], vec![]);
    let cur = [
        tap(1, "s1", "com.a", &[5]),
        tap(2, "s3", "com.d", &[6]),
        tap(3, "s2", "com.c", &[7]),
    ];
    let groups = [
        group("com.a", &[5]),
        group("com.b", &[8]),
        group("com.c", &[7]),
        group("com.d", &[6]),
    ];
    let p = tap_plan(&cur, &dev, &groups);
    // s1 now points at another bundle: old tap removed, new one created; s3 left the config.
    assert_eq!(p.remove, [0, 1]);
    assert_eq!(p.keep, [2]);
    assert!(p.update.is_empty());
    assert_eq!(
        p.create,
        [("s1".to_owned(), "com.b".to_owned(), vec![8], false)]
    );
}

#[test]
fn tap_plan_skips_sources_whose_app_is_not_running() {
    let p = tap_plan(&[], &one_app(), &[group("com.other", &[1])]);
    assert_eq!(p, TapPlan::default());
    // Same with no processes at all: an idle source, no tap, nothing to remove.
    assert_eq!(tap_plan(&[], &one_app(), &[]), TapPlan::default());
}

#[test]
fn tap_plan_mute_flip_rebuilds_tap() {
    let mut dev = one_app();
    let cur = [tap(1, "s1", "com.a", &[5])];
    let groups = [group("com.a", &[5])];
    dev.sources[0].mute_original = true;
    let p = tap_plan(&cur, &dev, &groups);
    assert_eq!(p.remove, [0]);
    assert!(p.keep.is_empty() && p.update.is_empty());
    assert_eq!(
        p.create,
        [("s1".to_owned(), "com.a".to_owned(), vec![5], true)]
    );
    // A tap built with mute stays while the flag stays on.
    let mut muted = tap(2, "s1", "com.a", &[5]);
    muted.mute = true;
    let p = tap_plan(&[muted], &dev, &groups);
    assert_eq!(p.keep, [0]);
    assert!(p.remove.is_empty() && p.create.is_empty());
}

// ---- run() with hand-built buffer lists ----

const BUFS_OFF: usize = std::mem::offset_of!(AudioBufferList, mBuffers);

/// An `AudioBufferList` laid out like the HAL's (8-byte header, 16-byte buffers).
struct Bl {
    mem: Vec<u64>,
    data: Vec<Vec<f32>>,
}
impl Bl {
    fn new(chs: &[u16], frames: usize) -> Bl {
        let mut data: Vec<Vec<f32>> = chs
            .iter()
            .map(|&c| vec![0.0; frames * usize::from(c)])
            .collect();
        let mut mem = vec![0u64; 1 + 2 * chs.len()];
        let l = mem.as_mut_ptr().cast::<AudioBufferList>();
        for (i, &c) in chs.iter().enumerate() {
            let b = AudioBuffer {
                mNumberChannels: u32::from(c),
                mDataByteSize: (data[i].len() * 4) as u32,
                mData: data[i].as_mut_ptr().cast(),
            };
            unsafe {
                l.cast::<u8>()
                    .add(BUFS_OFF)
                    .cast::<AudioBuffer>()
                    .add(i)
                    .write(b)
            };
        }
        unsafe { (*l).mNumberBuffers = chs.len() as u32 };
        Bl { mem, data }
    }
    fn ptr(&mut self) -> *mut AudioBufferList {
        self.mem.as_mut_ptr().cast()
    }
    fn buf(&mut self, i: usize) -> &mut AudioBuffer {
        unsafe {
            &mut *self
                .ptr()
                .cast::<u8>()
                .add(BUFS_OFF)
                .cast::<AudioBuffer>()
                .add(i)
        }
    }
    fn fill(&mut self, i: usize, f: impl Fn(usize) -> f32) {
        self.data[i]
            .iter_mut()
            .enumerate()
            .for_each(|(k, x)| *x = f(k));
    }
    fn all_zero(&self) -> bool {
        self.data.iter().flatten().all(|&x| x == 0.0)
    }
}

struct Rig {
    cb: Cb,
    meters: Arc<Meters>,
    _handle: MixerHandle,
}
fn rig(dev: &VirtualDevice, lay: &IoLayout, ain: &[u16], aout: &[u16]) -> Rig {
    let meters = Arc::new(Meters::new());
    let (mixer, mut handle) = mixer(meters.clone());
    handle.set_plan(MixPlan::compile(dev, lay));
    let shared = Arc::new(Shared {
        expected: AtomicU64::new(layout::shape_key_of(ain, aout)),
        state: AtomicU64::new(1),
    });
    Rig {
        cb: Cb {
            mixer,
            shared,
            meters: meters.clone(),
            in_planar: vec![0.0; SCRATCH_LEN],
            out_planar: vec![0.0; SCRATCH_LEN],
        },
        meters,
        _handle: handle,
    }
}
/// Exactly representable in f32, distinct per (frame, channel).
fn sample(frame: usize, ch: usize) -> f32 {
    (frame as f32) * 0.25 + ch as f32 * 100.0 + 1.0
}
fn sub(uid: &str, main: bool, i: &[u16], o: &[u16]) -> SubDesc {
    SubDesc {
        uid: uid.into(),
        is_main: main,
        in_bufs: i.to_vec(),
        out_bufs: o.to_vec(),
    }
}
fn build(dev: &VirtualDevice, subs: &[SubDesc], ain: &[u16], aout: &[u16]) -> Rig {
    let li = layout_inputs(dev);
    let spec = LayoutSpec {
        subs,
        passthru: li.passthru.as_deref(),
        device_sources: &li.device_sources,
        monitors: &li.monitors,
        taps: &[],
    };
    let lay = layout::build_layout(&spec, ain, aout).unwrap();
    rig(dev, &lay, ain, aout)
}

const PT_IN: [u16; 1] = [2];
const PT_OUT: [u16; 1] = [2];
/// Pass-through device on one input and one output buffer.
fn passthru_rig() -> Rig {
    let dev = vdev(
        vec![src("pt", SourceKind::PassThru)],
        vec![wire("pt", 0, 0), wire("pt", 1, 1)],
        vec![],
    );
    build(&dev, &[sub("comp", true, &[2], &[2])], &PT_IN, &PT_OUT)
}

const TB_IN: [u16; 3] = [2, 1, 1];
const TB_OUT: [u16; 3] = [2, 4, 4];
/// Aggregate of main (in 2 / out 2), a mic split over two mono buffers, and an 8-ch monitor in two
/// 4-ch buffers: 3 input and 3 output buffers.
fn three_buffer_rig() -> Rig {
    let mut dev = vdev(
        vec![src("mic", SourceKind::Device { uid: "mic".into() })],
        vec![wire("mic", 0, 0), wire("mic", 1, 1)],
        vec![mon("spk", true)],
    );
    dev.monitors[0].wires = vec![
        MonitorWire {
            out_ch: 0,
            dst_ch: 0,
        },
        MonitorWire {
            out_ch: 1,
            dst_ch: 7,
        },
    ];
    let subs = [
        sub("comp", true, &[2], &[2]),
        sub("mic", false, &[1, 1], &[]),
        sub("spk", false, &[], &[4, 4]),
    ];
    build(&dev, &subs, &TB_IN, &TB_OUT)
}

fn fill_three(inp: &mut Bl) {
    inp.fill(1, |f| sample(f, 0)); // mic L
    inp.fill(2, |f| sample(f, 1)); // mic R
}

#[test]
fn run_one_buffer_roundtrip() {
    let mut r = passthru_rig();
    let mut inp = Bl::new(&PT_IN, 64);
    let mut out = Bl::new(&PT_OUT, 64);
    inp.fill(0, |i| sample(i / 2, i % 2));
    out.fill(0, |_| 9.0);
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    assert_eq!(out.data[0], inp.data[0]);
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn run_three_buffers_stereo_and_8ch_monitor() {
    let mut r = three_buffer_rig();
    let frames = 100;
    let mut inp = Bl::new(&TB_IN, frames);
    let mut out = Bl::new(&TB_OUT, frames);
    fill_three(&mut inp);
    (0..3).for_each(|b| out.fill(b, |_| 9.0));
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    for f in 0..frames {
        assert_eq!(out.data[0][f * 2], sample(f, 0), "main L f{f}");
        assert_eq!(out.data[0][f * 2 + 1], sample(f, 1), "main R f{f}");
        // Monitor dst ch 0 = first buffer ch 0; dst ch 7 = second buffer ch 3; the rest silent.
        assert_eq!(out.data[1][f * 4], sample(f, 0), "mon ch0 f{f}");
        assert_eq!(out.data[2][f * 4 + 3], sample(f, 1), "mon ch7 f{f}");
        for c in 1..4 {
            assert_eq!(out.data[1][f * 4 + c], 0.0);
            assert_eq!(out.data[2][f * 4 + c - 1], 0.0);
        }
    }
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn run_more_than_max_frames_runs_in_chunks() {
    let mut r = three_buffer_rig();
    let frames = MAX_FRAMES + 904; // 5000
    let mut inp = Bl::new(&TB_IN, frames);
    let mut out = Bl::new(&TB_OUT, frames);
    fill_three(&mut inp);
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    for f in [0, MAX_FRAMES - 1, MAX_FRAMES, frames - 1] {
        assert_eq!(out.data[0][f * 2], sample(f, 0), "f{f}");
        assert_eq!(out.data[2][f * 4 + 3], sample(f, 1), "f{f}");
    }
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn run_wrong_shape_zeroes_output_and_counts() {
    let mut r = three_buffer_rig();
    // Same total channels, different buffer split: the key differs.
    let mut inp = Bl::new(&[2, 2], 32);
    let mut out = Bl::new(&TB_OUT, 32);
    (0..3).for_each(|b| out.fill(b, |_| 9.0));
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    assert!(out.all_zero());
    assert_eq!(r.meters.mismatch_count(), 1);
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    assert_eq!(r.meters.mismatch_count(), 2);
}

#[test]
fn run_expected_zero_is_silent_without_counting() {
    let mut r = three_buffer_rig();
    r.cb.shared.expected.store(0, Relaxed);
    let mut inp = Bl::new(&TB_IN, 16);
    let mut out = Bl::new(&TB_OUT, 16);
    fill_three(&mut inp);
    out.fill(0, |_| 9.0);
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    assert!(out.all_zero());
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn run_uneven_frame_counts_are_a_mismatch() {
    let mut r = three_buffer_rig();
    let mut inp = Bl::new(&TB_IN, 16);
    let mut out = Bl::new(&TB_OUT, 16);
    inp.buf(1).mDataByteSize = 15 * 4; // one frame short
    out.fill(0, |_| 9.0);
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    assert!(out.all_zero());
    assert_eq!(r.meters.mismatch_count(), 1);
}

#[test]
fn run_null_mdata_is_tolerated() {
    let mut r = three_buffer_rig();
    let mut inp = Bl::new(&TB_IN, 32);
    let mut out = Bl::new(&TB_OUT, 32);
    fill_three(&mut inp);
    inp.buf(2).mData = std::ptr::null_mut(); // mic R missing: treated as silence
    out.buf(1).mData = std::ptr::null_mut(); // first monitor buffer not provided: skipped
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    assert_eq!(out.data[0][2], sample(1, 0));
    assert_eq!(out.data[0][3], 0.0);
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn run_null_input_list() {
    // An output-only aggregate: a null input list matches an empty expected input shape.
    let dev = vdev(vec![], vec![], vec![]);
    let mut r = rig(&dev, &layout_of(0, 2), &[], &[2]);
    let mut out = Bl::new(&[2], 16);
    out.fill(0, |_| 9.0);
    unsafe { run(&mut r.cb, std::ptr::null_mut(), out.ptr()) };
    assert!(out.all_zero());
    assert_eq!(r.meters.mismatch_count(), 0);
    // The same null list against a plan that expects inputs is a shape mismatch.
    let mut r = three_buffer_rig();
    let mut out = Bl::new(&TB_OUT, 16);
    out.fill(0, |_| 9.0);
    unsafe { run(&mut r.cb, std::ptr::null_mut(), out.ptr()) };
    assert!(out.all_zero());
    assert_eq!(r.meters.mismatch_count(), 1);
}

// ---- driver settle (F1b) ----

fn want_dev(id: &str, channels: u16) -> DriverDevice {
    DriverDevice {
        uid: format!("{DEVICE_UID_PREFIX}{id}"),
        name: id.into(),
        channels,
    }
}

#[test]
fn unsettled_waits_for_channel_change_on_visible_and_companion() {
    let want = [want_dev("v1", 8)];
    let vis = want[0].uid.clone();
    let comp = companion_uid(&vis);
    let all = |c: u16| {
        let (vis, comp) = (vis.clone(), comp.clone());
        move |u: &str| (u == vis || u == comp).then_some((c, c))
    };
    assert!(unsettled(&want, &all(8), false).is_empty());
    // Companion still reports the old channel count.
    let stale = |u: &str| {
        if u == companion_uid(&want[0].uid) {
            Some((2, 2))
        } else {
            Some((8, 8))
        }
    };
    let bad = unsettled(&want, &stale, false);
    assert_eq!(bad.len(), 1);
    assert!(bad[0].contains("2 in / 2 out, want 8"), "{bad:?}");
    // Input and output are checked separately.
    let half = |_: &str| Some((8, 2));
    assert_eq!(unsettled(&want, &half, false).len(), 2);
}

#[test]
fn unsettled_missing_devices_and_companion_grace() {
    let want = [want_dev("v1", 2)];
    let vis = want[0].uid.clone();
    let only_visible = |u: &str| (u == vis).then_some((2, 2));
    // Inside the grace period a missing companion keeps us waiting, afterwards it is tolerated.
    assert_eq!(unsettled(&want, &only_visible, false).len(), 1);
    assert!(unsettled(&want, &only_visible, true).is_empty());
    // The visible device itself is always required.
    let none = |_: &str| None;
    let bad = unsettled(&want, &none, true);
    assert_eq!(bad, [format!("{}: not present", want[0].uid)]);
}

// ---- device-event filter and backoff (F4) ----

#[test]
fn uid_set_ignores_own_aggregates_and_order() {
    let a = |u: &str| u.to_owned();
    let before = uid_set([a("mic"), a("spk"), a("soundrouter.agg.v1.0")]);
    let echo = uid_set([
        a("spk"),
        a("soundrouter.agg.v1.0"),
        a("soundrouter.agg.v1.1"),
        a("mic"),
        a("mic"),
    ]);
    assert_eq!(before, ["mic", "spk"]);
    assert_eq!(
        before, echo,
        "creating/destroying our aggregates is not a hot-plug"
    );
    let plugged = uid_set([a("mic"), a("spk"), a("usb")]);
    assert_ne!(before, plugged);
}

#[test]
fn backoff_grows_and_is_capped() {
    assert_eq!(backoff(0), EVENT_DEBOUNCE);
    assert_eq!(backoff(1), EVENT_DEBOUNCE * 2);
    assert_eq!(backoff(3), EVENT_DEBOUNCE * 8);
    assert_eq!(backoff(5), MAX_BACKOFF);
    assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
}

// ---- monitor wire warnings (bug 7) ----

#[test]
fn monitor_wire_beyond_hardware_channels_is_reported() {
    let mut m = mon("spk", true);
    m.wires = vec![
        MonitorWire {
            out_ch: 0,
            dst_ch: 1,
        },
        MonitorWire {
            out_ch: 1,
            dst_ch: 2,
        },
        MonitorWire {
            out_ch: 0,
            dst_ch: 9,
        },
    ];
    let mut off = mon("off", false);
    off.wires = vec![MonitorWire {
        out_ch: 0,
        dst_ch: 50,
    }];
    let mut gone = mon("unplugged", true);
    gone.wires = vec![MonitorWire {
        out_ch: 0,
        dst_ch: 50,
    }];
    let dev = vdev(vec![], vec![], vec![m, off, gone]);
    let subs = [
        sub("comp", true, &[2], &[2]),
        sub("spk", false, &[], &[1, 1]),
        sub("off", false, &[], &[2]),
    ];
    let w = monitor_warnings(&dev, &subs);
    assert_eq!(w.len(), 2, "{w:?}");
    assert!(w[0].contains("output channel 2") && w[0].contains("has 2 output channels"));
    assert!(w[1].contains("output channel 9"));
    // In range: nothing to report.
    assert!(monitor_warnings(&dev, &[sub("spk", false, &[], &[16])]).is_empty());
}

#[test]
fn retry_delay_schedule_and_give_up() {
    assert_eq!(retry_delay(0), None);
    assert_eq!(retry_delay(1), Some(EVENT_DEBOUNCE));
    assert_eq!(retry_delay(2), Some(EVENT_DEBOUNCE * 2));
    assert_eq!(retry_delay(MAX_RETRIES), Some(backoff(MAX_RETRIES - 1)));
    assert_eq!(retry_delay(MAX_RETRIES + 1), None);
    assert_eq!(retry_delay(u32::MAX), None);
}

#[test]
fn background_errors_are_keyed() {
    let mut e = BTreeMap::new();
    assert_eq!(background_message(&e), None);
    e.insert(Op::Apply, "a".to_string());
    e.insert(Op::Retap, "r".to_string());
    e.remove(&Op::Retap);
    assert_eq!(background_message(&e).as_deref(), Some("a"));
}

#[test]
fn retry_reruns_with_backoff_then_surfaces_the_error_once() {
    let mut r = Retry::default();
    let mut errs = BTreeMap::new();
    assert_eq!(r.wait(), None, "idle: block until an event");
    for n in 1..=MAX_RETRIES {
        r.record(Op::Apply, Err(anyhow!("no device")), &mut errs);
        assert_eq!(r.op, Some(Op::Apply));
        assert_eq!(r.wait(), retry_delay(n));
        assert!(errs.is_empty(), "retries stay quiet");
    }
    // Out of retries: surfaced, and nothing reruns until a new event.
    r.record(Op::Apply, Err(anyhow!("no device")), &mut errs);
    assert_eq!((r.op, r.wait()), (None, None));
    assert_eq!(background_message(&errs).as_deref(), Some("no device"));
    // A new event's attempt that fails the same way changes nothing.
    r.record(Op::Apply, Err(anyhow!("no device")), &mut errs);
    assert_eq!((r.op, errs.len()), (None, 1));
    // A different failure replaces the message.
    r.record(Op::Apply, Err(anyhow!("still no device")), &mut errs);
    assert_eq!(errs[&Op::Apply], "still no device");
    // Success resets the budget and clears everything.
    errs.insert(Op::Retap, "tap".into());
    r.record(Op::Apply, Ok(()), &mut errs);
    assert_eq!(r, Retry::default());
    assert!(errs.is_empty());
}

#[test]
fn a_good_retap_clears_only_its_own_error() {
    let mut r = Retry {
        fails: 3,
        op: Some(Op::Retap),
    };
    let mut errs = BTreeMap::from([(Op::Apply, "a".into()), (Op::Retap, "r".into())]);
    r.record(Op::Retap, Ok(()), &mut errs);
    assert_eq!(r, Retry::default());
    assert_eq!(background_message(&errs).as_deref(), Some("a"));
}

#[test]
fn retry_choose_prefers_apply_and_reruns_the_failed_op() {
    let idle = Retry::default();
    assert_eq!(idle.choose(false, false, false, false), None);
    assert_eq!(idle.choose(false, false, false, true), Some(Op::Retap));
    for (devs, wake, rebuild) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        assert_eq!(idle.choose(devs, wake, rebuild, true), Some(Op::Apply));
    }
    let retap = Retry {
        fails: 1,
        op: Some(Op::Retap),
    };
    assert_eq!(retap.choose(false, false, false, false), Some(Op::Retap));
    assert_eq!(retap.choose(true, false, false, false), Some(Op::Apply));
    let apply = Retry {
        fails: 1,
        op: Some(Op::Apply),
    };
    assert_eq!(apply.choose(false, false, false, true), Some(Op::Apply));
}

// ---- sleep/wake and rate recovery ----

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn rates(items: &[(&str, f64)]) -> Vec<(String, f64)> {
    items.iter().map(|(u, r)| ((*u).to_owned(), *r)).collect()
}

#[test]
fn settle_waits_longer_after_wake() {
    assert_eq!(settle(false, 0), EVENT_DEBOUNCE);
    assert_eq!(settle(true, 0), WAKE_SETTLE);
    assert_eq!(settle(true, 8), MAX_BACKOFF);
}

#[test]
fn pending_absorbs_events_until_shutdown() {
    let mut p = Pending::default();
    assert!(p.absorb(Event::Devices));
    assert!(p.absorb(Event::Wake));
    assert!(p.absorb(Event::Rate("mic".into())));
    assert!(p.absorb(Event::Rate("mic".into())));
    assert!(p.absorb(Event::IoStart {
        id: "v1".into(),
        build: 7,
        ok: false
    }));
    assert!(!p.absorb(Event::Shutdown));
    assert_eq!(
        p,
        Pending {
            devs: true,
            procs: false,
            wake: true,
            rates: set(&["mic"]),
            io: vec![("v1".into(), 7, false)],
        }
    );
}

#[test]
fn wake_rebuilds_every_vdev() {
    let (a, b) = (rates(&[("a", 48e3)]), rates(&[("b", 48e3)]));
    let v: Vec<(&str, &[(String, f64)])> = vec![("v1", &a), ("v2", &b)];
    let t = rebuild_targets(true, &set(&[]), &set(&[]), &v, &|_| 48e3);
    assert_eq!(t, set(&["v1", "v2"]));
}

#[test]
fn rate_change_rebuilds_only_vdevs_built_on_that_device() {
    let (a, b) = (
        rates(&[("c1", 48e3), ("mic", 48e3)]),
        rates(&[("c2", 48e3)]),
    );
    let v: Vec<(&str, &[(String, f64)])> = vec![("v1", &a), ("v2", &b)];
    let now = |u: &str| if u == "mic" { 44.1e3 } else { 48e3 };
    let t = rebuild_targets(false, &set(&["mic"]), &set(&[]), &v, &now);
    assert_eq!(t, set(&["v1"]));
}

#[test]
fn rate_event_matching_the_built_rate_is_an_echo() {
    let a = rates(&[("mic", 48e3)]);
    let v: Vec<(&str, &[(String, f64)])> = vec![("v1", &a)];
    // Changed and changed back before the worker looked; or our own aggregate touched it.
    assert!(rebuild_targets(false, &set(&["mic"]), &set(&[]), &v, &|_| 48e3).is_empty());
    // A device that vanished reads 0: the device-list path handles that, not a rate rebuild.
    assert!(rebuild_targets(false, &set(&["mic"]), &set(&[]), &v, &|_| 0.0).is_empty());
    // Nothing reported: nothing rebuilt, even if a rate drifted.
    assert!(rebuild_targets(false, &set(&[]), &set(&[]), &v, &|_| 44.1e3).is_empty());
}

#[test]
fn failed_start_rebuilds_that_vdev() {
    let (a, b) = (rates(&[]), rates(&[]));
    let v: Vec<(&str, &[(String, f64)])> = vec![("v1", &a), ("v2", &b)];
    let t = rebuild_targets(false, &set(&[]), &set(&["v2", "gone"]), &v, &|_| 48e3);
    assert_eq!(t, set(&["v2"]));
}

#[test]
fn io_retry_is_bounded_and_reset_by_success() {
    let mut f = HashMap::new();
    let id = vec!["v1".to_string()];
    for _ in 0..MAX_RETRIES {
        assert_eq!(io_retry(&mut f, &id, &[]), set(&["v1"]));
    }
    // The next failure exceeds the budget: stay Failed instead of rebuilding forever.
    assert!(io_retry(&mut f, &id, &[]).is_empty());
    // A start that works gives the vdev a fresh budget.
    assert!(io_retry(&mut f, &[], &id).is_empty());
    assert_eq!(io_retry(&mut f, &id, &[]), set(&["v1"]));
}
