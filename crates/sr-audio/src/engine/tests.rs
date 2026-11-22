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
        name: String::new(),
    }
}
fn mon(uid: &str, enabled: bool) -> Monitor {
    Monitor {
        device_uid: uid.into(),
        enabled,
        volume_db: 0.0,
        wires: vec![],
        name: String::new(),
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
    let visible = format!("{DEVICE_UID_PREFIX}{}", dev.id);
    let li = layout_inputs(&dev, &companion_uid(&visible));
    assert_eq!(li.passthru.as_deref(), Some("pt"));
    // Visible-device fallback: its input loops the engine's own output back, so no Pass-Thru.
    let fb = layout_inputs(&dev, &visible);
    assert_eq!(fb.passthru, None);
    assert_eq!(
        (fb.device_sources, fb.monitors),
        (li.device_sources.clone(), li.monitors.clone())
    );
    // Same uid twice: the first source wins.
    assert_eq!(li.device_sources["mic"], "m1");
    assert_eq!(li.device_sources.len(), 1);
    assert_eq!(li.monitors, HashSet::from(["spk".to_owned()]));
    assert_eq!(
        layout_inputs(&vdev(vec![], vec![], vec![]), "comp"),
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
    tap: TapInHandle,
    stats: Arc<TapStats>,
}
fn rig(dev: &VirtualDevice, lay: &IoLayout, ain: &[u16], aout: &[u16]) -> Rig {
    let meters = Arc::new(Meters::new());
    let (mixer, mut handle) = mixer(meters.clone());
    handle.set_plan(MixPlan::compile(dev, lay));
    let stats = Arc::new(TapStats::default());
    let (tap_cb, tap) = tap_in(stats.clone());
    let shared = Arc::new(Shared {
        expected: AtomicU64::new(layout::shape_key_of(ain, aout)),
        state: AtomicU64::new(1),
        delay: AtomicU32::new(NO_DELAY),
        lead: AtomicU32::new(NO_DELAY),
    });
    Rig {
        cb: Cb {
            mixer,
            shared,
            meters: meters.clone(),
            in_planar: Scratch::new(),
            out_planar: Scratch::new(),
            next_t: f64::NAN,
            tap: tap_cb,
        },
        meters,
        _handle: handle,
        tap,
        stats,
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
    let li = layout_inputs(dev, "comp");
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
    assert!(unsettled(&want, &all(8), false, &[]).is_empty());
    // Companion still reports the old channel count.
    let stale = |u: &str| {
        if u == companion_uid(&want[0].uid) {
            Some((2, 2))
        } else {
            Some((8, 8))
        }
    };
    let bad = unsettled(&want, &stale, false, &[]);
    assert_eq!(bad.len(), 1);
    assert!(bad[0].contains("2 in / 2 out, want 8"), "{bad:?}");
    // Input and output are checked separately.
    let half = |_: &str| Some((8, 2));
    assert_eq!(unsettled(&want, &half, false, &[]).len(), 2);
}

#[test]
fn unsettled_missing_devices_and_companion_grace() {
    let want = [want_dev("v1", 2)];
    let vis = want[0].uid.clone();
    let only_visible = |u: &str| (u == vis).then_some((2, 2));
    // Inside the grace period a missing companion keeps us waiting, afterwards it is tolerated.
    assert_eq!(unsettled(&want, &only_visible, false, &[]).len(), 1);
    assert!(unsettled(&want, &only_visible, true, &[]).is_empty());
    // The visible device itself is always required.
    let none = |_: &str| None;
    let bad = unsettled(&want, &none, true, &[]);
    assert_eq!(bad, [format!("{}: not present", want[0].uid)]);
}

#[test]
fn unsettled_waits_until_removed_devices_are_gone() {
    // Disable then enable: the HAL still lists the device the driver just dropped. Taking that as
    // settled lost it right after ("neither the hidden companion nor the visible device is present").
    let gone = want_dev("v1", 2).uid;
    let comp = companion_uid(&gone);
    let removed = [gone.clone()];
    let dying = |u: &str| (u == gone || u == comp).then_some((2, 2));
    let bad = unsettled(&[], &dying, true, &removed);
    assert_eq!(
        bad,
        [
            format!("{gone}: removed but still present"),
            format!("{comp}: removed but still present")
        ]
    );
    let hidden_left = |u: &str| (u == comp).then_some((2, 2));
    assert_eq!(unsettled(&[], &hidden_left, true, &removed).len(), 1);
    assert!(unsettled(&[], &|_| None, true, &removed).is_empty());
    // A kept device is not a removed one.
    let want = [want_dev("v2", 2)];
    let kept = |u: &str| u.contains("v2").then_some((2, 2));
    assert!(unsettled(&want, &kept, false, &removed).is_empty());
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
fn a_monitor_wire_into_the_input_the_device_reads_is_a_feedback_loop() {
    let w = |out_ch, dst_ch| MonitorWire { out_ch, dst_ch };
    let mut bh = mon("BlackHole16ch_UID", true);
    // 0 -> 0 closes the loop (mic wire BlackHole in 0 -> vdev 0); 0 -> 8 and 1 -> 1 do not: the source
    // is not wired from in 8 and vdev 1 gets nothing from BlackHole in 1.
    bh.wires = vec![w(0, 0), w(0, 8), w(1, 1)];
    let mut off = mon("BlackHole16ch_UID", false);
    off.wires = vec![w(0, 0)];
    let dev = vdev(
        vec![src(
            "bh",
            SourceKind::Device {
                uid: "BlackHole16ch_UID".into(),
            },
        )],
        vec![wire("bh", 0, 0), wire("bh", 1, 0)],
        vec![bh.clone(), off],
    );
    let virt = |u: &str| u == "BlackHole16ch_UID";
    let (clean, warnings) = without_feedback(&dev, &virt);
    assert_eq!(clean.monitors[0].wires, [w(0, 8), w(1, 1)]);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("feed itself") && warnings[0].contains("output channel 0"));
    assert_eq!(
        clean.monitors[1].wires,
        [w(0, 0)],
        "a disabled monitor is not in the aggregate"
    );
    // The same wiring on a real interface is a headphone monitor.
    assert_eq!(without_feedback(&dev, &|_| false), (dev.clone(), vec![]));
    // A different source device cannot loop through this one.
    let other = vdev(
        vec![src("mic", SourceKind::Device { uid: "mic".into() })],
        vec![wire("mic", 0, 0)],
        vec![bh],
    );
    assert_eq!(without_feedback(&other, &virt), (other.clone(), vec![]));
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
            ..Pending::default()
        }
    );
}

#[test]
fn new_events_land_in_pending() {
    let mut p = Pending::default();
    assert!(p.absorb(Event::Dead("mic".into())));
    assert!(p.absorb(Event::Buffer("v1".into())));
    assert!(!p.wake && !p.devs);
    assert!(p.absorb(Event::DefaultOutput));
    assert!(
        p.devs,
        "re-read the device list (acts only on a real change)"
    );
    assert!(p.absorb(Event::ServiceRestarted));
    assert!(
        p.wake,
        "coreaudiod restart: every id is stale, rebuild everything"
    );
    assert_eq!((p.dead, p.buffers), (set(&["mic"]), set(&["v1"])));
}

#[test]
fn a_dead_device_rebuilds_only_the_vdevs_built_on_it() {
    let (a, b) = (
        rates(&[("c1", 48e3), ("mic", 48e3)]),
        rates(&[("c2", 48e3)]),
    );
    let v: Vec<(&str, &[(String, f64)])> = vec![("v1", &a), ("v2", &b)];
    let alive = |u: &str| u != "mic";
    assert_eq!(dead_targets(&set(&["mic"]), &v, &alive), set(&["v1"]));
    // an IsAlive event for a device that is alive again (flapped) changes nothing
    assert!(dead_targets(&set(&["mic"]), &v, &|_| true).is_empty());
    assert!(dead_targets(&set(&[]), &v, &alive).is_empty());
}

#[test]
fn foreign_buffer_changes_are_set_back_a_few_times() {
    assert!(!should_reset_buffer(Some(256), 256, 0), "our own echo");
    assert!(should_reset_buffer(Some(512), 256, 0));
    assert!(should_reset_buffer(Some(512), 256, MAX_BUFFER_RESETS - 1));
    assert!(
        !should_reset_buffer(Some(512), 256, MAX_BUFFER_RESETS),
        "a size the HAL refuses cannot loop"
    );
    assert!(!should_reset_buffer(None, 256, 0), "unreadable");
}

#[test]
fn the_reset_count_is_a_loop_guard_not_a_lifetime_budget() {
    let s = Duration::from_secs;
    // Fresh set-backs keep counting: a size the HAL refuses stays capped within the window.
    assert_eq!(
        fresh_resets(MAX_BUFFER_RESETS, Some(s(5))),
        MAX_BUFFER_RESETS
    );
    assert!(!should_reset_buffer(
        Some(512),
        256,
        fresh_resets(MAX_BUFFER_RESETS, Some(s(59)))
    ));
    // A foreign change a minute after the last set-back (weeks into a run) is set back again.
    assert_eq!(fresh_resets(MAX_BUFFER_RESETS, Some(s(60))), 0);
    assert!(should_reset_buffer(
        Some(512),
        256,
        fresh_resets(MAX_BUFFER_RESETS, Some(s(3600)))
    ));
    assert_eq!(fresh_resets(0, None), 0, "none yet");
}

#[test]
fn used_devices_are_sources_and_enabled_monitors_of_enabled_devices_without_our_own() {
    let mut on = vdev(
        vec![
            src("a", SourceKind::Device { uid: "mic".into() }),
            src(
                "b",
                SourceKind::Device {
                    uid: format!("{DEVICE_UID_PREFIX}v2"),
                },
            ),
            src("c", SourceKind::PassThru),
        ],
        vec![],
        vec![mon("spk", true), mon("off", false), mon("mic", true)],
    );
    on.enabled = true;
    let mut off = vdev(
        vec![src(
            "d",
            SourceKind::Device {
                uid: "other".into(),
            },
        )],
        vec![],
        vec![],
    );
    off.enabled = false;
    let cfg = Config {
        devices: vec![on, off],
        ..Config::default()
    };
    assert_eq!(used_device_uids(&cfg), ["mic", "spk"]);
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
fn a_monitor_rate_change_in_the_start_batch_still_rebuilds() {
    // v1 monitors V2 and was built at 48 kHz. V2 moves to 96 kHz before v1's IOProc start is booked,
    // and the rate event lands in the same batch as that start.
    let built = rates(&[("c1", 48e3), ("V2", 48e3)]);
    let now = rates(&[("c1", 48e3), ("V2", 96e3)]);
    let changed = set(&["V2"]);
    let base = start_baseline(&built, now.clone(), &changed);
    assert_eq!(base, built, "the reported device keeps its build rate");
    let v: Vec<(&str, &[(String, f64)])> = vec![("v1", &base)];
    let rate_now = |u: &str| if u == "V2" { 96e3 } else { 48e3 };
    assert_eq!(
        rebuild_targets(false, &changed, &set(&[]), &v, &rate_now),
        set(&["v1"])
    );
    // Settled while starting with no event in this batch: the new rate is the baseline (an echo).
    assert_eq!(start_baseline(&built, now.clone(), &set(&[])), now);
    // A device gone since the build drops out; one with no build rate takes the current one.
    assert_eq!(
        start_baseline(&built, rates(&[("c1", 48e3)]), &changed),
        rates(&[("c1", 48e3)])
    );
    assert_eq!(start_baseline(&[], now.clone(), &changed), now);
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

#[test]
fn apply_keeps_the_newest_config_when_the_driver_does_not_settle() {
    let mut g = Inner::default();
    let old = Config {
        devices: vec![vdev(vec![], vec![], vec![])],
        ..Config::default()
    };
    g.cfg = Some(old.clone());
    let mut new = old.clone();
    new.devices[0].channels = 8;
    let slow: fn(&Config) -> Result<()> = |_| bail!("driver devices not ready");
    assert!(apply_locked(&mut g, &new, slow).is_err());
    // A background apply after the next device event must rebuild `new`, not re-apply (undo to) `old`.
    assert_eq!(g.cfg, Some(new));
    // An invalid config is refused before anything is kept.
    let mut bad = old.clone();
    bad.devices[0].channels = 3;
    assert!(apply_locked(&mut g, &bad, slow).is_err());
    assert_eq!(g.cfg.unwrap().devices[0].channels, 8);
}

#[test]
fn track_time_counts_only_skipped_audio() {
    let mut r = passthru_rig();
    // First cycle never counts, contiguous cycles (even with sub-frame jitter) do not either.
    for (t, frames) in [(1000.0, 512), (1512.0, 512), (2024.3, 512), (2536.3, 256)] {
        r.cb.track_time(t, frames);
    }
    assert_eq!(r.meters.glitch_count(), 0);
    // One buffer was skipped, then a restart that rewinds the clock.
    r.cb.track_time(2792.3 + 512.0, 512);
    assert_eq!(r.meters.glitch_count(), 1);
    r.cb.track_time(0.0, 512);
    assert_eq!(r.meters.glitch_count(), 2);
}

// ---- in-place reuse vs rebuild ----

fn spec(main: &str, subs: &[&str]) -> AggSpec {
    AggSpec {
        main: main.into(),
        subs: subs.iter().map(|s| (*s).to_owned()).collect(),
        channels: 2,
        buffer_frames: 256,
        split: true,
        readers: vec![],
    }
}

#[test]
fn keep_in_place_only_for_the_same_aggregate() {
    let (comp, vis) = ("soundrouter:v1~c".to_owned(), "soundrouter:v1".to_owned());
    let both = [comp.clone(), vis.clone()];
    let live = spec(&comp, &["mic"]);
    let keep = |l: &AggSpec, io, cands: &[String], subs: &[&str]| {
        let subs: Vec<String> = subs.iter().map(|s| (*s).to_owned()).collect();
        keep_in_place(l, &[], io, cands, &subs, 2, 256, true)
    };
    assert!(keep(&live, IoState::Running, &both, &["mic"]));
    assert!(keep(&live, IoState::Starting, &both, &["mic"]));
    assert!(!keep(&live, IoState::Running, &both, &[]));
    assert!(!keep(&live, IoState::Running, &both[1..], &["mic"]));
    let subs = ["mic".to_owned()];
    assert!(!keep_in_place(
        &live,
        &[],
        IoState::Running,
        &both,
        &subs,
        4,
        256,
        true
    ));
    assert!(!keep_in_place(
        &live,
        &[],
        IoState::Running,
        &both,
        &subs,
        2,
        128,
        true
    ));
    // Splitting the taps off (or back in) is a different aggregate layout.
    assert!(!keep_in_place(
        &live,
        &[],
        IoState::Running,
        &both,
        &subs,
        2,
        256,
        false
    ));
}

/// A start that failed (e.g. a monitor held in hog mode) is only retried by a new build; after the
/// background retries were spent, an apply reused the dead aggregate and the device stayed silent.
#[test]
fn a_failed_io_start_is_rebuilt_by_the_next_apply() {
    let live = spec("c", &[]);
    assert!(!keep_in_place(
        &live,
        &[],
        IoState::Failed(-50),
        &["c".into()],
        &[],
        2,
        256,
        true
    ));
}

/// A vdev built on the visible-device fallback (its companion not published yet, e.g. many devices
/// added at once) moves to the companion once it appears, else Pass-Thru stayed off for good. One whose
/// companion build failed stays on the fallback rather than rebuilding on every apply.
#[test]
fn a_fallback_main_moves_to_the_companion_once_present() {
    let (comp, vis) = ("c".to_owned(), "v".to_owned());
    let live = spec(&vis, &[]);
    let run = IoState::Running;
    assert!(keep_in_place(
        &live,
        &[],
        run,
        std::slice::from_ref(&vis),
        &[],
        2,
        256,
        true
    ));
    let both = [comp.clone(), vis.clone()];
    assert!(!keep_in_place(&live, &[], run, &both, &[], 2, 256, true));
    assert!(keep_in_place(&live, &[comp], run, &both, &[], 2, 256, true));
}

#[test]
fn apply_drops_disabled_devices_from_recovery_and_clears_background_errors() {
    let mut g = Inner::default();
    let mut cfg = Config {
        devices: vec![vdev(vec![], vec![], vec![])],
        ..Config::default()
    };
    cfg.devices[0].enabled = false;
    // A wake rebuild of v1 failed and its retries ran out ...
    g.recovering.insert("v1".into());
    g.bg_errors
        .insert(Op::Apply, "device 'v1': no companion".into());
    // ... then the user disables v1: the apply succeeds, so neither may linger in the status.
    apply_locked(&mut g, &cfg, |_| Ok(())).unwrap();
    assert!(g.recovering.is_empty());
    assert_eq!(background_message(&g.bg_errors), None);
}

/// Measured live: a monitor whose stream layout changed (2 -> 4 channels, no device-list or rate change)
/// left the whole vdev silent (every cycle a shape mismatch) with the status still Running.
#[test]
fn a_sub_with_new_stream_buffers_rebuilds_its_vdev() {
    let subs = [sub("main", true, &[2], &[2]), sub("mon", false, &[], &[2])];
    let changed = set(&["mon"]);
    let now = |o: &'static [u16]| move |uid: &str| (uid == "mon").then(|| (vec![], o.to_vec()));
    assert!(reshaped(&subs, &changed, &now(&[4])));
    assert!(reshaped(&subs, &changed, &now(&[2, 2])));
    // The same buffers (an echo, or a rate-only change), a device not in the batch, or one gone.
    assert!(!reshaped(&subs, &changed, &now(&[2])));
    assert!(!reshaped(&subs, &set(&["main"]), &now(&[4])));
    assert!(!reshaped(&subs, &changed, &|_| None));
}

#[test]
fn an_identical_device_list_is_still_pushed_to_the_driver() {
    let dev = |id: &str, channels| DriverDevice {
        uid: format!("{DEVICE_UID_PREFIX}{id}"),
        name: id.into(),
        channels,
    };
    let want = vec![dev("v1", 2), dev("v2", 8)];
    // Same list: still set (the driver's pass re-asks a side whose format change the host aborted), nothing removed.
    let mut pushed = vec![];
    let removed = push_devices(&want, want.clone(), |l| {
        pushed.push(l.to_vec());
        Ok(())
    })
    .unwrap();
    assert_eq!((pushed, removed), (vec![want.clone()], vec![]));
    // A changed list: set, and the dropped uid is reported for the settle wait.
    let old = vec![dev("v1", 2), dev("gone", 2)];
    let removed = push_devices(&want, old, |_| Ok(())).unwrap();
    assert_eq!(removed, vec![format!("{DEVICE_UID_PREFIX}gone")]);
    // A refused set is an error.
    assert!(push_devices(&want, vec![], |_| Err(anyhow::anyhow!("refused"))).is_err());
}

#[test]
fn in_out_distance_is_the_frame_gap_or_nothing() {
    assert_eq!(in_out_distance(1000.0, 1512.0), Some(512));
    assert_eq!(in_out_distance(1000.0, 1000.4), Some(0));
    assert_eq!(in_out_distance(1000.0, 999.0), None);
    assert_eq!(in_out_distance(f64::NAN, 5.0), None);
    assert_eq!(in_out_distance(0.0, f64::INFINITY), None);
}

#[test]
fn the_ioproc_publishes_only_valid_sample_times() {
    let lay = IoLayout {
        inputs: vec![],
        outputs: vec![],
    };
    let r = rig(&vdev(vec![], vec![], vec![]), &lay, &[], &[]);
    let stamp = |t: f64, flags| {
        // SAFETY: AudioTimeStamp is plain data; all-zero is a valid (empty) stamp.
        let mut s: AudioTimeStamp = unsafe { std::mem::zeroed() };
        s.mSampleTime = t;
        s.mFlags = flags;
        s
    };
    let valid = AudioTimeStampFlags::SampleTimeValid;
    let none = AudioTimeStampFlags(0);
    let read = || r.cb.shared.delay.load(Relaxed);
    let lead = || r.cb.shared.lead.load(Relaxed);
    assert_eq!((read(), lead()), (NO_DELAY, NO_DELAY));
    let now = stamp(0.0, valid);
    r.cb.track_delay(&now, &stamp(10.0, valid), &stamp(20.0, none));
    assert_eq!(read(), NO_DELAY, "output time not valid");
    assert_eq!(lead(), NO_DELAY);
    r.cb.track_delay(&now, &stamp(10.0, valid), &stamp(522.0, valid));
    assert_eq!((read(), lead()), (512, 522));
    r.cb.track_delay(&now, &stamp(10.0, valid), &stamp(-5.0, valid));
    assert_eq!(
        (read(), lead()),
        (512, 522),
        "a negative gap keeps the last good one"
    );
}

// ---- split vdev: taps arrive through the ring ----

const SB: usize = 64;

/// Pass-thru (2 main input channels) plus one app tap of 2 channels that is not in the aggregate's
/// buffers but in the ring; both go to the 2 output channels.
fn split_dev() -> VirtualDevice {
    vdev(
        vec![src("pt", SourceKind::PassThru), app("a", "com.a")],
        vec![
            wire("pt", 0, 0),
            wire("pt", 1, 1),
            wire("a", 0, 0),
            wire("a", 1, 1),
        ],
        vec![],
    )
}

fn split_layout(dev: &VirtualDevice, taps: &[String]) -> IoLayout {
    let li = layout_inputs(dev, "comp");
    let spec = LayoutSpec {
        subs: &[sub("comp", true, &[2], &[2])],
        passthru: li.passthru.as_deref(),
        device_sources: &li.device_sources,
        monitors: &li.monitors,
        taps,
    };
    let ain: Vec<u16> = [2].into_iter().chain(taps.iter().map(|_| 2)).collect();
    layout::build_layout(&spec, &ain, &[2]).unwrap()
}

fn split_rig() -> Rig {
    let dev = split_dev();
    let lay = split_layout(&dev, &["a".to_owned()]);
    rig(&dev, &lay, &[2], &[2])
}

/// `SB` frames of a tap: channel 0 counts up from `start`, channel 1 is its negative.
fn push_tap(tx: &mut crate::tapring::TapTx, start: usize) {
    for f in 0..SB {
        tx.stage()[f * 2] = (start + f) as f32 / 1024.0;
        tx.stage()[f * 2 + 1] = -((start + f) as f32) / 1024.0;
    }
    tx.push(SB);
}

fn run_split(r: &mut Rig, pt: impl Fn(usize) -> f32) -> Bl {
    let mut inp = Bl::new(&[2], SB);
    let mut out = Bl::new(&[2], SB);
    inp.fill(0, pt);
    out.fill(0, |_| 9.0);
    unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
    out
}

#[test]
fn split_taps_mix_in_from_the_ring_next_to_the_main_inputs() {
    let mut r = split_rig();
    let (mut tx, rx) = crate::tapring::channel(2, SB, r.stats.clone());
    r.tap.publish(rx);
    // Prefill: a block plus the cushion, then audio flows in order.
    push_tap(&mut tx, 0);
    let out = run_split(&mut r, |_| 0.0);
    assert!(out.all_zero(), "still prefilling: silent, not a mismatch");
    push_tap(&mut tx, SB);
    let out = run_split(&mut r, |i| if i % 2 == 0 { 0.25 } else { 0.0 });
    for f in 0..SB {
        assert_eq!(out.data[0][f * 2], 0.25 + f as f32 / 1024.0, "L f{f}");
        assert_eq!(out.data[0][f * 2 + 1], -(f as f32) / 1024.0, "R f{f}");
    }
    push_tap(&mut tx, 2 * SB);
    let out = run_split(&mut r, |_| 0.0);
    assert_eq!(out.data[0][0], SB as f32 / 1024.0, "next block follows");
    assert_eq!(r.meters.mismatch_count(), 0);
    assert_eq!(r.stats.counters(), crate::tapring::TapCounters::default());
}

#[test]
fn split_main_paths_run_without_any_tap_ring() {
    // The plan wants the tap's channels but no ring was published yet (tap aggregate still starting,
    // e.g. while the permission prompt is open): pass-thru must not notice.
    let mut r = split_rig();
    let out = run_split(&mut r, |i| sample(i / 2, i % 2));
    for f in 0..SB {
        assert_eq!(out.data[0][f * 2], sample(f, 0));
        assert_eq!(out.data[0][f * 2 + 1], sample(f, 1));
    }
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn a_ring_the_plan_does_not_expect_is_silent_not_a_mismatch() {
    let mut r = split_rig();
    let (mut tx, rx) = crate::tapring::channel(4, SB, r.stats.clone()); // 2 taps; the plan has 1
    r.tap.publish(rx);
    for _ in 0..3 {
        tx.stage()[..SB * 4].fill(1.0);
        tx.push(SB);
    }
    let out = run_split(&mut r, |i| sample(i / 2, i % 2));
    assert_eq!(out.data[0][0], sample(0, 0));
    assert_eq!(out.data[0][3], sample(1, 1));
    assert_eq!(r.meters.mismatch_count(), 0);
}

#[test]
fn plan_and_ring_swap_in_either_order_without_a_mismatch() {
    let mut r = split_rig();
    let (mut tx, rx) = crate::tapring::channel(2, SB, r.stats.clone());
    r.tap.publish(rx);
    push_tap(&mut tx, 0);
    push_tap(&mut tx, SB);
    run_split(&mut r, |_| 0.0);
    // The app quits: the plan loses the tap's inputs before the (empty) ring replaces the old one.
    let dev = split_dev();
    r._handle
        .set_plan(MixPlan::compile(&dev, &split_layout(&dev, &[])));
    let out = run_split(&mut r, |i| sample(i / 2, i % 2));
    assert_eq!(out.data[0][0], sample(0, 0), "main paths unchanged");
    r.tap.publish(TapRx::none(r.stats.clone()));
    let out = run_split(&mut r, |i| sample(i / 2, i % 2));
    assert_eq!(out.data[0][1], sample(0, 1));
    // It comes back: the ring first, the plan after.
    let (mut tx, rx) = crate::tapring::channel(2, SB, r.stats.clone());
    r.tap.publish(rx);
    push_tap(&mut tx, 0);
    push_tap(&mut tx, SB);
    let out = run_split(&mut r, |_| 0.0);
    assert!(out.all_zero(), "ring without a plan for it is ignored");
    r._handle.set_plan(MixPlan::compile(
        &dev,
        &split_layout(&dev, &["a".to_owned()]),
    ));
    let out = run_split(&mut r, |_| 0.0);
    assert_eq!(out.data[0][2], 1.0 / 1024.0);
    assert_eq!(r.meters.mismatch_count(), 0);
}

// ---- the tap IOProc's copy into the ring ----

fn tap_cb(
    in_bufs: &[u16],
    out_bufs: &[u16],
    n_taps: usize,
    cushion: usize,
) -> (tapagg::TapCb, crate::tapring::TapRx) {
    let stats = Arc::new(TapStats::default());
    let ch = in_bufs[in_bufs.len() - n_taps..]
        .iter()
        .map(|&c| usize::from(c))
        .sum();
    let (tx, rx) = crate::tapring::channel(ch, cushion, stats.clone());
    let shared = Arc::new(Shared {
        expected: AtomicU64::new(layout::shape_key_of(in_bufs, out_bufs)),
        state: AtomicU64::new(1),
        delay: AtomicU32::new(NO_DELAY),
        lead: AtomicU32::new(NO_DELAY),
    });
    (
        tapagg::TapCb {
            tx,
            shared,
            stats,
            n_tap_bufs: n_taps,
        },
        rx,
    )
}

#[test]
fn the_tap_ioproc_copies_the_tap_buffers_frame_major_and_skips_the_companion_input() {
    let (ins, outs) = ([2u16, 2, 2], [2u16]);
    let (mut cb, mut rx) = tap_cb(&ins, &outs, 2, 0);
    let frames = 40;
    let mut inp = Bl::new(&ins, frames);
    let mut out = Bl::new(&outs, frames);
    inp.fill(0, |_| 7.0); // the companion's own input: not copied
    inp.fill(1, |i| i as f32); // tap 1: L R interleaved
    inp.fill(2, |i| 1000.0 + i as f32);
    unsafe { tapagg::tap_run(&mut cb, inp.ptr(), out.ptr()) };
    assert_eq!(cb.stats.counters(), TapCounters::default());
    let mut dst = vec![0.0; 4 * MAX_FRAMES];
    rx.pull(frames, &mut dst, 0);
    for f in 0..frames {
        assert_eq!(dst[f], (f * 2) as f32, "tap 1 L f{f}");
        assert_eq!(dst[MAX_FRAMES + f], (f * 2 + 1) as f32, "tap 1 R f{f}");
        assert_eq!(
            dst[2 * MAX_FRAMES + f],
            1000.0 + (f * 2) as f32,
            "tap 2 L f{f}"
        );
        assert_eq!(
            dst[3 * MAX_FRAMES + f],
            1000.0 + (f * 2 + 1) as f32,
            "tap 2 R f{f}"
        );
    }
}

#[test]
fn the_tap_ioproc_skips_a_cycle_of_the_wrong_shape_and_counts_it() {
    let (mut cb, mut rx) = tap_cb(&[2, 2], &[2], 1, 0);
    let mut out = Bl::new(&[2], 16);
    let mut wrong = Bl::new(&[2, 2, 2], 16); // a second tap appeared that the key does not know
    unsafe { tapagg::tap_run(&mut cb, wrong.ptr(), out.ptr()) };
    assert_eq!(cb.stats.counters().mismatches, 1);
    let mut dst = vec![9.0; 2 * MAX_FRAMES];
    rx.pull(16, &mut dst, 0);
    assert!(dst[..16].iter().all(|&x| x == 0.0), "nothing was pushed");
    // No input list at all (the HAL passes none while the aggregate has no input streams).
    unsafe { tapagg::tap_run(&mut cb, std::ptr::null_mut(), out.ptr()) };
    assert_eq!(cb.stats.counters().mismatches, 1);
}

#[test]
fn tap_copy_chunks_cycles_longer_than_max_frames() {
    // Two cycles, a cushion of one cycle: the first one-frame read drops the surplus oldest frames
    // and leaves exactly the second cycle, so the reads below see its two chunks in order.
    let frames = MAX_FRAMES + 100;
    let (mut cb, mut rx) = tap_cb(&[2, 2], &[2], 1, frames);
    let mut inp = Bl::new(&[2, 2], frames);
    let mut out = Bl::new(&[2], frames);
    inp.fill(1, |i| i as f32);
    for _ in 0..2 {
        unsafe { tapagg::tap_run(&mut cb, inp.ptr(), out.ptr()) };
    }
    assert_eq!(cb.stats.counters(), TapCounters::default());
    let mut dst = vec![0.0; 2 * MAX_FRAMES];
    rx.pull(1, &mut dst, 0);
    assert_eq!(dst[0], ((frames - 1) * 2) as f32, "last frame of cycle 1");
    rx.pull(MAX_FRAMES - 10, &mut dst, 0);
    assert_eq!(dst[0], 0.0, "cycle 2 starts");
    rx.pull(99, &mut dst, 0);
    assert_eq!(
        dst[10],
        (MAX_FRAMES * 2) as f32,
        "frame MAX_FRAMES is the first of the second chunk"
    );
    assert_eq!(dst[98], ((MAX_FRAMES + 88) * 2) as f32);
}

// ---- split vdev bookkeeping ----

#[test]
fn split_setting_reads_the_env_value() {
    use tapagg::split_setting;
    assert!(!split_setting(Some("0"), true));
    assert!(split_setting(Some("1"), false));
    assert!(split_setting(None, true));
    assert!(!split_setting(None, false));
    assert!(
        split_setting(Some("yes"), true),
        "anything else is the default"
    );
}

#[test]
fn a_failed_tap_start_fails_the_vdev_and_a_waiting_one_only_the_apps() {
    use DeviceState as D;
    use IoState::*;
    assert_eq!(combine_io(Running, Some(Starting)), Running);
    assert_eq!(combine_io(Running, Some(Failed(-1))), Failed(-1));
    assert_eq!(combine_io(Failed(-2), Some(Failed(-1))), Failed(-2));
    assert_eq!(combine_io(Starting, None), Starting);
    let denied = TccStatus::Denied;
    // The permission prompt holds the tap aggregate only: a running main aggregate is Waiting for apps.
    assert_eq!(split_state(Starting, None, denied), D::Starting);
    assert_eq!(split_state(Running, None, denied), D::Running);
    assert_eq!(
        split_state(Running, Some(Starting), denied),
        D::WaitingForPermission
    );
    assert_eq!(
        split_state(Running, Some(Starting), TccStatus::Granted),
        D::Starting
    );
    assert_eq!(split_state(Running, Some(Running), denied), D::Running);
    assert!(matches!(
        split_state(Running, Some(Failed(-1)), denied),
        D::Failed(_)
    ));
    assert!(matches!(
        split_state(Failed(-1), None, denied),
        D::Failed(_)
    ));
}

// ---- the audio threads never allocate ----

mod no_alloc {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! { static ALLOCS: Cell<usize> = const { Cell::new(0) }; }

    struct Counting;
    // SAFETY: delegates to System; the counter is a const-initialised thread-local Cell (no allocation).
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            ALLOCS.with(|c| c.set(c.get() + 1));
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            ALLOCS.with(|c| c.set(c.get() + 1));
            unsafe { System.realloc(p, l, n) }
        }
    }
    #[global_allocator]
    static A: Counting = Counting;

    #[test]
    fn tap_copy_and_ring_pop_do_not_allocate() {
        let mut r = split_rig();
        let (mut tcb, rx) = tap_cb(&[2, 2], &[2], 1, 32);
        r.tap.publish(rx);
        let mut tap_in = Bl::new(&[2, 2], SB);
        let mut tap_out = Bl::new(&[2], SB);
        tap_in.fill(1, |i| i as f32 / 1024.0);
        let mut inp = Bl::new(&[2], SB);
        let mut out = Bl::new(&[2], SB);
        inp.fill(0, |i| i as f32);
        // Warm up (prefill, first plan pick-up), then count: steady state, an underrun, a pile-up
        // (trim), a ring swap and a long cycle in chunks.
        for _ in 0..4 {
            unsafe { tapagg::tap_run(&mut tcb, tap_in.ptr(), tap_out.ptr()) };
            unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) };
        }
        let (fresh_tx, fresh_rx) = crate::tapring::channel(2, 32, r.stats.clone());
        let mut fresh = tapagg::TapCb {
            tx: fresh_tx,
            shared: tcb.shared.clone(),
            stats: r.stats.clone(),
            n_tap_bufs: 1,
        };
        let big_in = Bl::new(&[2, 2], MAX_FRAMES + 100);
        let mut big_in = big_in;
        let mut big_out = Bl::new(&[2], MAX_FRAMES + 100);
        r.tap.publish(fresh_rx);
        let before = ALLOCS.with(Cell::get);
        for i in 0..1000 {
            unsafe { tapagg::tap_run(&mut fresh, tap_in.ptr(), tap_out.ptr()) };
            match i % 50 {
                0 => {} // a cycle without a pop: the ring piles up, then trims
                1..=5 => unsafe {
                    tapagg::tap_run(&mut fresh, tap_in.ptr(), tap_out.ptr());
                    run(&mut r.cb, inp.ptr(), out.ptr());
                },
                _ => unsafe { run(&mut r.cb, inp.ptr(), out.ptr()) },
            }
        }
        unsafe { tapagg::tap_run(&mut fresh, big_in.ptr(), big_out.ptr()) };
        assert_eq!(ALLOCS.with(Cell::get) - before, 0);
        let c = r.stats.counters();
        assert!(c.underruns > 0 || c.trims > 0, "the paths ran: {c:?}");
        r.tap.flush();
    }
}

#[test]
fn only_the_main_and_device_sources_are_read() {
    let d = vdev(
        vec![
            src("a", SourceKind::Device { uid: "mic".into() }),
            src("p", SourceKind::PassThru),
            src("b", SourceKind::Device { uid: "mic".into() }),
            src(
                "c",
                SourceKind::App {
                    bundle_id: "x".into(),
                },
            ),
        ],
        vec![],
        vec![mon("headset", true), mon("mic", true)],
    );
    assert_eq!(
        input_readers(&d, "c1"),
        ["c1", "mic"],
        "monitors are written, not read"
    );
}

#[test]
fn unread_devices_lose_their_input_streams_in_stream_order() {
    let sub = |uid: &str, i: &[u16]| SubDesc {
        uid: uid.into(),
        is_main: uid == "c1",
        in_bufs: i.to_vec(),
        out_bufs: vec![2],
    };
    let subs = [
        sub("c1", &[2]),
        sub("headset", &[1]),
        sub("mic", &[1, 1]),
        sub("out", &[]),
    ];
    let readers = ["c1".to_owned(), "mic".to_owned()];
    assert_eq!(stream_flags(&subs, &readers, 0), [true, false, true, true]);
    assert_eq!(
        stream_flags(&subs, &readers, 2),
        [true, false, true, true, true, true],
        "an unsplit aggregate's tap streams stay on"
    );
    let all = ["c1".to_owned(), "headset".to_owned(), "mic".to_owned()];
    assert!(
        stream_flags(&subs, &all, 0).iter().all(|&b| b),
        "nothing to switch off"
    );
}

#[test]
fn default_tap_cushion_is_one_buffer() {
    use tapagg::default_cushion;
    assert_eq!(
        [64, 128, 256, 512].map(default_cushion),
        [64, 128, 256, 512],
        "one tap block: a late tap cycle still finds a whole block"
    );
}

#[test]
fn taps_run_only_when_the_main_and_the_tap_aggregate_both_run() {
    use IoState::{Failed, Running, Starting};
    assert!(taps_running(Running, None), "unsplit: the one aggregate");
    assert!(taps_running(Running, Some(Running)));
    assert!(
        !taps_running(Running, Some(Starting)),
        "tap aggregate still starting"
    );
    assert!(
        !taps_running(Starting, Some(Running)),
        "main aggregate still starting"
    );
    assert!(!taps_running(Running, Some(Failed(-1))));
    assert!(!taps_running(Starting, None));
}

#[test]
fn a_device_shared_by_two_vdevs_combines_their_builds_in_any_order() {
    // vdev A only plays to a headset (2 out, input unread), vdev B reads its microphone (0 out).
    let (a, b) = ((2, false), (0, true));
    assert_eq!(combine_builds([a, b].into_iter()), Some((2, true)));
    assert_eq!(combine_builds([b, a].into_iter()), Some((2, true)));
    assert_eq!(combine_builds([a].into_iter()), Some((2, false)));
    assert_eq!(combine_builds(std::iter::empty()), None);
}
