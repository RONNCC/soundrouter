//! Live stress of the real `Engine` against the INSTALLED driver, silent by design: a 1 kHz tone
//! (`packaging/srtone.c`: L 0.2, R 0.1) plays into the virtual device `soundrouter:v2`, the engine taps
//! that process into `soundrouter:v1`, and random config edits are applied while the tone runs.
//! After every edit settles it checks: no background error and every enabled device `Running`, the
//! signal on v1's input and in `Engine::meters` matches the config (silence when muted, the gain-scaled
//! tone otherwise), the monitor onto BlackHole 16ch (when installed) carries the same level, our private
//! aggregates match the live vdevs and tap aggregates (no leak), and the process's fd and thread counts
//! stay flat.
//!
//! Needs the driver, no running SoundRouter.app (it changes the driver's device list, left as `[]`),
//! and microphone + System Audio Recording permission for the terminal.
//!   `cargo run -p sr-audio --example stress -- edits [N=200] [SEED]`
//!   `cargo run -p sr-audio --example stress -- replug-v1|replug-v2 [N=20] [SEED]`
//!   `cargo run -p sr-audio --example stress -- mono` (monitor onto a 1-channel loopback output)
//! `replug-*` removes the device from the driver list and re-adds it with the same uid within 100 ms,
//! N times, while routing; the engine must rebuild and the signal must return.
//! `SR_TAP_SPLIT=0` runs the unsplit engine (no tap aggregates).
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sr_audio::engine::AGG_UID_PREFIX;
use sr_audio::loopback::{Probe, engine_holds_companion};
use sr_audio::{Engine, IoState, driver, hal};
use sr_core::backend::{Backend, DeviceState};
use sr_core::config::*;
use sr_core::driver_proto::DriverDevice;

const AMP: [f32; 2] = [0.2, 0.1];
const V1: &str = "soundrouter:v1";
const V2: &str = "soundrouter:v2";
const WINDOW: Duration = Duration::from_millis(120);
const SETTLE: Duration = Duration::from_secs(20);

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// What the config means for the signal; everything the edits change.
#[derive(Clone)]
struct Model {
    v1_on: bool,
    v3_on: bool,
    app: bool,
    app_muted: bool,
    app_db: f32,
    dev_muted: bool,
    dev_db: f32,
    wires: Vec<(u16, u16)>,   // (src_ch, out_ch) of the app source
    mon: Option<(bool, f32)>, // (enabled, volume_db) of the monitor onto BlackHole
}

impl Model {
    fn new() -> Model {
        Model {
            v1_on: true,
            v3_on: false,
            app: true,
            app_muted: false,
            app_db: 0.0,
            dev_muted: false,
            dev_db: 0.0,
            wires: vec![(0, 0), (1, 1)],
            mon: None,
        }
    }

    fn config(&self, bh_uid: Option<&str>) -> Config {
        let pt = Source {
            id: "pt".into(),
            kind: SourceKind::PassThru,
            channels: 2,
            volume_db: 0.0,
            muted: false,
            mute_original: false,
            name: String::new(),
        };
        let mut sources = vec![pt];
        let mut wires = vec![];
        if self.app {
            sources.push(Source {
                id: "a".into(),
                kind: SourceKind::App {
                    bundle_id: format!("path:{}", player_path().display()),
                },
                channels: 2,
                volume_db: self.app_db,
                muted: self.app_muted,
                mute_original: false,
                name: String::new(),
            });
            wires = self
                .wires
                .iter()
                .map(|&(s, o)| Wire::new("a", s, o))
                .collect();
        }
        let monitors = match (self.mon, bh_uid) {
            (Some((enabled, volume_db)), Some(uid)) => vec![Monitor {
                device_uid: uid.into(),
                enabled,
                volume_db,
                wires: (0..2)
                    .map(|c| MonitorWire {
                        out_ch: c,
                        dst_ch: c,
                    })
                    .collect(),
                name: String::new(),
            }],
            _ => vec![],
        };
        let vdev = |id: &str, name: &str, enabled, sources, wires, monitors, volume_db, muted| {
            VirtualDevice {
                id: id.into(),
                name: name.into(),
                enabled,
                channels: 2,
                volume_db,
                muted,
                sources,
                wires,
                monitors,
                nodes: Vec::new(),
            }
        };
        Config {
            devices: vec![
                vdev(
                    "v1",
                    "Loopback Audio",
                    self.v1_on,
                    sources,
                    wires,
                    monitors,
                    self.dev_db,
                    self.dev_muted,
                ),
                vdev(
                    "v2",
                    "SR Test Player",
                    true,
                    vec![],
                    vec![],
                    vec![],
                    0.0,
                    false,
                ),
                vdev(
                    "v3",
                    "Spare",
                    self.v3_on,
                    vec![],
                    vec![],
                    vec![],
                    0.0,
                    false,
                ),
            ],
            ..Config::default()
        }
    }

    /// Expected linear peak of each v1 output channel.
    fn out(&self) -> [f32; 2] {
        let mut o = [0.0; 2];
        if self.v1_on && self.app && !self.app_muted && !self.dev_muted {
            for &(s, c) in &self.wires {
                o[c as usize] += AMP[s as usize] * gain(self.app_db) * gain(self.dev_db);
            }
        }
        o
    }

    /// Expected peaks on BlackHole channels 0 and 1.
    fn bh(&self) -> [f32; 2] {
        match self.mon {
            Some((true, db)) => self.out().map(|x| x * gain(db)),
            _ => [0.0; 2],
        }
    }

    /// Our private aggregates that must exist: one per running vdev, plus v1's tap aggregate.
    fn aggregates(&self, split: bool) -> usize {
        1 + usize::from(self.v1_on)
            + usize::from(self.v3_on)
            + usize::from(split && self.v1_on && self.app)
    }

    fn enabled(&self) -> Vec<&'static str> {
        let mut v = vec![];
        if self.v1_on {
            v.push("v1");
        }
        v.push("v2");
        if self.v3_on {
            v.push("v3");
        }
        v
    }
}

fn player_path() -> PathBuf {
    PathBuf::from(std::env::var("SR_STRESS_TONE").expect("SR_STRESS_TONE is set by main"))
}

fn close(obs: &[f32], want: &[f32]) -> bool {
    obs.iter().zip(want).all(|(&o, &w)| {
        if w < 1e-6 {
            o < 2e-4 // -74 dBFS: the mute ramp is long over
        } else {
            (20.0 * (o.max(1e-9) / w).log10()).abs() < 0.6
        }
    })
}

fn fds() -> usize {
    // SAFETY: a size query with a null buffer.
    let n = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDLISTFDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    n as usize / std::mem::size_of::<libc::proc_fdinfo>()
}

fn threads() -> usize {
    // SAFETY: `ti` is a plain C struct of the size passed.
    unsafe {
        let mut ti: libc::proc_taskinfo = std::mem::zeroed();
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut ti).cast(),
            std::mem::size_of::<libc::proc_taskinfo>() as i32,
        );
        ti.pti_threadnum as usize
    }
}

fn aggregates() -> usize {
    hal::all_devices()
        .into_iter()
        .filter(|&d| hal::device_uid(d).starts_with(AGG_UID_PREFIX))
        .count()
}

/// Uids of the engine's own aggregates of vdev `id` (the numbering changes with every rebuild).
fn vdev_aggregates(id: &str) -> Vec<String> {
    let prefix = format!("{AGG_UID_PREFIX}{id}.");
    let mut v: Vec<String> = hal::all_devices()
        .into_iter()
        .map(hal::device_uid)
        .filter(|u| u.starts_with(&prefix) && !u.contains(".tap."))
        .collect();
    v.sort();
    v
}

struct Player(Option<Child>);
impl Player {
    fn start() -> Player {
        let mut child = Command::new(player_path())
            .args([V2, "3000", &AMP[0].to_string(), &AMP[1].to_string()])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn srtone");
        let mut banner = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut banner,
        )
        .unwrap();
        assert!(
            banner.contains(&format!("({V2}) ch 0-1")),
            "player: {banner:?}"
        );
        Player(Some(child))
    }
    fn stop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Rig {
    e: Engine,
    model: Model,
    bh_uid: Option<String>,
    split: bool,
    v1: Option<Probe>,
    bh: Option<Probe>,
    player: Player,
    failures: Vec<String>,
}

impl Rig {
    fn apply(&mut self) -> Duration {
        let t0 = Instant::now();
        let c = self.model.config(self.bh_uid.as_deref());
        c.validate().expect("model config validates");
        self.e.apply(&c).expect("apply");
        t0.elapsed()
    }

    fn fail(&mut self, what: String) {
        eprintln!("FAIL {what}");
        self.failures.push(what);
    }

    /// Wait until v1 is built on its companion, then attach the probe.
    fn attach_v1(&mut self) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < SETTLE {
            if self.e.io_state("v1") == Some(IoState::Running) && engine_holds_companion(V1) {
                match Probe::start(V1, false) {
                    Ok(p) => {
                        self.v1 = Some(p);
                        return true;
                    }
                    Err(e) => eprintln!("probe v1: {e:#}"),
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Polls windows until v1's input, the engine meters and the monitor all match the model for 3
    /// windows in a row; returns the time that took, or the last observation.
    fn signal(&mut self) -> Result<Duration, String> {
        let want = self.model.out();
        let want_bh = self.model.bh();
        let t0 = Instant::now();
        let (mut ok_run, mut last) = (0, String::new());
        if let Some(p) = &self.v1 {
            p.stats.take_peaks(2);
        }
        if let Some(p) = &self.bh {
            p.stats.take_peaks(2);
        }
        let _ = self.e.meters("v1");
        let mut first_ok = None;
        while t0.elapsed() < SETTLE {
            std::thread::sleep(WINDOW);
            let v1 = self.v1.as_ref().map(|p| p.stats.take_peaks(2));
            let bh = self.bh.as_ref().map(|p| p.stats.take_peaks(2));
            let m = self.e.meters("v1");
            let mut good = true;
            last = String::new();
            if let Some(v) = &v1 {
                good &= close(v, &want);
                last += &format!("v1 in {v:?} want {want:?}; ");
            }
            if let Some(b) = &bh
                && self.model.v1_on
            {
                good &= close(&b[..2], &want_bh);
                last += &format!("bh {:?} want {want_bh:?}; ", &b[..2]);
            }
            if let Some(m) = &m {
                good &= close(&m.channels, &want);
                last += &format!("meters.out {:?}; ", m.channels);
            }
            if good {
                first_ok.get_or_insert(t0.elapsed());
                ok_run += 1;
                if ok_run >= 3 {
                    return Ok(first_ok.unwrap());
                }
            } else {
                ok_run = 0;
                first_ok = None;
            }
        }
        Err(last)
    }

    /// Everything but the signal: status, vdev states, aggregate count (zombies get time to go).
    fn state(&mut self) -> Result<(), String> {
        let t0 = Instant::now();
        loop {
            let st = self.e.status();
            let mut ids: Vec<&str> = st.per_device.iter().map(|(i, _)| i.as_str()).collect();
            ids.sort();
            let all_running = st
                .per_device
                .iter()
                .all(|(_, s)| *s == DeviceState::Running);
            let aggs = aggregates();
            let want_aggs = self.model.aggregates(self.split);
            let err = if let Some(b) = st.background_error {
                Some(format!("background error: {b}"))
            } else if ids != self.model.enabled() {
                Some(format!("devices {ids:?}, want {:?}", self.model.enabled()))
            } else if !all_running {
                Some(format!("not all running: {:?}", st.per_device))
            } else if aggs != want_aggs {
                Some(format!("aggregates {aggs}, want {want_aggs}"))
            } else {
                None
            };
            match err {
                None => return Ok(()),
                Some(e) if t0.elapsed() > SETTLE => return Err(e),
                Some(e)
                    if e.starts_with("background error")
                        && t0.elapsed() > Duration::from_secs(3) =>
                {
                    return Err(e);
                }
                Some(_) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

fn setup(seed_note: &str) -> Rig {
    let dir = std::env::temp_dir().join(format!("sr-stress-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tone = dir.canonicalize().unwrap().join("srtone"); // the tap keys the process by its real path
    let src_c = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packaging/srtone.c");
    let built = Command::new("clang")
        .args([
            "-O2",
            "-framework",
            "CoreAudio",
            "-framework",
            "CoreFoundation",
            "-o",
        ])
        .arg(&tone)
        .arg(src_c)
        .status()
        .unwrap();
    assert!(built.success(), "clang failed");
    // SAFETY: single-threaded here; no engine thread exists yet.
    unsafe { std::env::set_var("SR_STRESS_TONE", &tone) };
    let e = Engine::new().unwrap();
    let bh_uid = e
        .hardware()
        .into_iter()
        .find(|d| d.name == "BlackHole 16ch")
        .map(|d| d.uid);
    let split = std::env::var("SR_TAP_SPLIT").as_deref() != Ok("0");
    println!("{seed_note}; split={split}; blackhole={bh_uid:?}");
    Rig {
        e,
        model: Model::new(),
        bh_uid,
        split,
        v1: None,
        bh: None,
        player: Player(None),
        failures: vec![],
    }
}

/// The initial config, player and probes; returns once the signal is flowing.
fn start(r: &mut Rig) {
    r.apply();
    r.player = Player::start();
    assert!(r.attach_v1(), "v1 never ran");
    r.bh = r.bh_uid.as_deref().map(|u| Probe::start(u, false).unwrap());
    r.state().expect("initial state");
    let t = r.signal().expect("initial signal");
    println!("signal after {t:?}; fds {} threads {}", fds(), threads());
}

fn edits(n: usize, seed: u64) -> Rig {
    let mut r = setup(&format!("edits n={n} seed={seed}"));
    start(&mut r);
    let mut rng = Rng(seed);
    let mut by_kind: std::collections::BTreeMap<&str, Vec<Duration>> = Default::default();
    let (mut fd0, mut th0, mut fd_max, mut th_max) = (0, 0, 0, 0);
    for i in 0..n {
        let before = r.model.clone();
        let kind: &str = loop {
            let m = &mut r.model;
            match rng.below(12) {
                0 if m.app => {
                    m.app_muted = !m.app_muted;
                    break "source on/off";
                }
                1 => {
                    m.dev_muted = !m.dev_muted;
                    break "device mute";
                }
                2 if m.app => {
                    m.app_db = [0.0, -3.0, -6.0, -12.0, -20.0][rng.below(5)];
                    break "source volume";
                }
                3 => {
                    m.dev_db = [0.0, -3.0, -6.0, -12.0][rng.below(4)];
                    break "device volume";
                }
                4 if m.app => {
                    let all = [(0, 0), (1, 1), (0, 1), (1, 0)];
                    let free: Vec<_> = all.iter().filter(|w| !m.wires.contains(w)).collect();
                    if !free.is_empty() {
                        m.wires.push(*free[rng.below(free.len())]);
                        break "add wire";
                    }
                }
                5 if m.app && !m.wires.is_empty() => {
                    m.wires.remove(rng.below(m.wires.len()));
                    break "remove wire";
                }
                6 if r.bh_uid.is_some() => {
                    m.mon = if m.mon.is_some() {
                        None
                    } else {
                        Some((true, 0.0))
                    };
                    break "add/remove monitor";
                }
                7 if m.mon.is_some() => {
                    let (en, db) = m.mon.unwrap();
                    m.mon = if rng.below(2) == 0 {
                        Some((!en, db))
                    } else {
                        Some((en, [0.0, -6.0, -12.0][rng.below(3)]))
                    };
                    break "monitor edit";
                }
                8 => {
                    m.v3_on = !m.v3_on;
                    break "enable/disable spare device";
                }
                9 if rng.below(3) == 0 => {
                    m.v1_on = !m.v1_on;
                    break "enable/disable v1";
                }
                10 | 11 => {
                    m.app = !m.app;
                    m.app_muted = false;
                    m.wires = if m.app { vec![(0, 0), (1, 1)] } else { vec![] };
                    break "add/remove app source";
                }
                _ => {}
            }
        };
        // The probe's device goes away with v1: let go of it first.
        if before.v1_on && !r.model.v1_on {
            r.v1 = None;
        }
        let took = r.apply();
        by_kind.entry(kind).or_default().push(took);
        let _ = (r.e.apps(), r.e.hardware()); // what the app's worker does between edits
        if !before.v1_on && r.model.v1_on && !r.attach_v1() {
            r.fail(format!("#{i} {kind}: v1 never ran again"));
            continue;
        }
        if let Err(e) = r.state() {
            r.fail(format!("#{i} {kind}: state: {e}"));
        }
        match r.signal() {
            Ok(t) => println!("#{i:3} {kind:<28} apply {took:>9.1?} signal {t:>8.1?}"),
            Err(e) => r.fail(format!("#{i} {kind}: signal: {e}")),
        }
        if r.failures.len() > 10 {
            eprintln!("too many failures, stopping");
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
        let (fd, th) = (fds(), threads());
        if i == 19 {
            (fd0, th0) = (fd, th);
        }
        if i >= 19 {
            fd_max = fd_max.max(fd);
            th_max = th_max.max(th);
        }
    }
    println!("\nedit kind                        n   apply p50      max");
    for (k, v) in &mut by_kind {
        v.sort();
        println!(
            "{k:<28} {:>5} {:>10.1?} {:>8.1?}",
            v.len(),
            v[v.len() / 2],
            v[v.len() - 1]
        );
    }
    println!(
        "fds {fd0} -> max {fd_max} -> end {}; threads {th0} -> max {th_max} -> end {}",
        fds(),
        threads()
    );
    if fds() > fd0 + 2 {
        r.fail(format!("fd count grew {fd0} -> {}", fds()));
    }
    if threads() > th0 + 4 {
        r.fail(format!("thread count grew {th0} -> {}", threads()));
    }
    r
}

fn replug(target: &str, n: usize, seed: u64) -> Rig {
    let mut r = setup(&format!("replug {target} n={n} seed={seed}"));
    r.model.v3_on = false;
    start(&mut r);
    let plugin = driver::require_plugin().unwrap();
    let mut rng = Rng(seed);
    let uid = format!("soundrouter:{target}");
    let full = vec![
        DriverDevice {
            uid: V1.into(),
            name: "Loopback Audio".into(),
            channels: 2,
        },
        DriverDevice {
            uid: V2.into(),
            name: "SR Test Player".into(),
            channels: 2,
        },
    ];
    let without: Vec<DriverDevice> = full.iter().filter(|d| d.uid != uid).cloned().collect();
    let (fd0, th0) = (fds(), threads());
    let mut changed = 0;
    for i in 0..n {
        let old = hal::device_by_uid(&uid);
        let aggs0 = vdev_aggregates(target);
        if target == "v1" {
            r.v1 = None;
        } else {
            r.player.stop(); // its device is about to vanish; srtone does not follow it
        }
        let t0 = Instant::now();
        driver::set_devices(plugin, &without).unwrap();
        std::thread::sleep(Duration::from_millis(rng.below(100) as u64));
        driver::set_devices(plugin, &full).unwrap();
        let gap = t0.elapsed();
        let t1 = Instant::now();
        while hal::device_by_uid(&uid).is_none() && t1.elapsed() < SETTLE {
            std::thread::sleep(Duration::from_millis(20));
        }
        let new = hal::device_by_uid(&uid);
        changed += usize::from(new != old);
        // The engine learns of the replug after its debounce (seconds): a player started before it
        // rebuilds the device would play into the object that is about to go. Wait for the rebuild.
        let t3 = Instant::now();
        while vdev_aggregates(target) == aggs0 && t3.elapsed() < SETTLE {
            std::thread::sleep(Duration::from_millis(50));
        }
        if vdev_aggregates(target) == aggs0 {
            r.fail(format!("#{i}: the engine never rebuilt {target}"));
        }
        // The engine must be back on the new device: every vdev running, then the signal.
        let t2 = Instant::now();
        while !(r.e.io_state("v1") == Some(IoState::Running)
            && r.e.io_state("v2") == Some(IoState::Running)
            && engine_holds_companion(V1))
        {
            if t2.elapsed() > SETTLE {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if target == "v2" {
            r.player = Player::start();
        } else if !r.attach_v1() {
            r.fail(format!("#{i}: v1 never ran again"));
            continue;
        }
        if let Err(e) = r.state() {
            r.fail(format!("#{i}: state: {e}"));
        }
        match r.signal() {
            Ok(t) => println!(
                "#{i:2} gap {gap:>6.0?} object {old:?} -> {new:?} running again {:>6.1?} signal {t:>7.1?}",
                t2.elapsed()
            ),
            Err(e) => r.fail(format!("#{i}: signal: {e}")),
        }
    }
    println!(
        "objects changed in {changed}/{n} cycles; fds {fd0} -> {}; threads {th0} -> {}",
        fds(),
        threads()
    );
    if fds() > fd0 + 2 {
        r.fail(format!("fd count grew {fd0} -> {}", fds()));
    }
    if threads() > th0 + 4 {
        r.fail(format!("thread count grew {th0} -> {}", threads()));
    }
    r
}

/// A monitor onto a 1-channel output (Microsoft Teams Audio when installed): one wire carries that
/// channel at full level, two wires onto the same channel average (no +6 dB). Read back from the
/// device's own input, which that driver loops from its output.
fn mono() -> Rig {
    let mut r = setup("mono monitor");
    let Some(mono) =
        r.e.hardware()
            .into_iter()
            .find(|d| d.out_ch == 1 && d.in_ch >= 1 && d.uid.contains("Loopback"))
    else {
        println!("SKIP: no 1-channel loopback output device");
        return r;
    };
    println!("mono output: {} ({})", mono.name, mono.uid);
    r.model.mon = None;
    start(&mut r);
    let probe = Probe::start(&mono.uid, false).unwrap();
    for (wires, want) in [
        (vec![(0, 0)], 0.2),
        (vec![(1, 0)], 0.1),
        (vec![(0, 0), (1, 0)], 0.15),
        (vec![(0, 0)], 0.2),
    ] {
        let mut c = r.model.config(None);
        c.devices[0].monitors = vec![Monitor {
            device_uid: mono.uid.clone(),
            enabled: true,
            volume_db: 0.0,
            wires: wires
                .iter()
                .map(|&(o, d)| MonitorWire {
                    out_ch: o,
                    dst_ch: d,
                })
                .collect(),
            name: String::new(),
        }];
        r.e.apply(&c).unwrap();
        let t0 = Instant::now();
        let mut last;
        let ok = loop {
            probe.stats.take_peaks(1);
            std::thread::sleep(WINDOW);
            last = probe.stats.take_peaks(1)[0];
            if close(&[last], &[want]) {
                break true;
            }
            if t0.elapsed() > SETTLE {
                break false;
            }
        };
        println!(
            "monitor wires {wires:?}: peak {last:.4}, want {want} -> {}",
            if ok { "ok" } else { "FAIL" }
        );
        if !ok {
            r.fail(format!("mono monitor {wires:?}: peak {last}, want {want}"));
        }
        if let Some(st) = r.e.status().background_error {
            r.fail(format!("mono monitor {wires:?}: background error {st}"));
        }
    }
    r
}

/// Which of the driver's devices the HAL lists and which only translate by uid (the hidden companions).
fn devices() -> Rig {
    let mut r = setup("devices");
    r.apply();
    std::thread::sleep(Duration::from_secs(3));
    let listed: Vec<String> = hal::all_devices()
        .into_iter()
        .map(hal::device_uid)
        .collect();
    for u in [
        "soundrouter:v1",
        "soundrouter:v1.engine",
        "soundrouter:v2",
        "soundrouter:v2.engine",
    ] {
        println!(
            "{u}: listed {}, translates {:?}",
            listed.iter().any(|l| l == u),
            hal::device_by_uid(u)
        );
    }
    r
}

fn main() {
    // RUST_LOG=sr_audio=debug shows what the engine does on each edit.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"))
        .format_timestamp_millis()
        .init();
    let mut a = std::env::args().skip(1);
    let mode = a.next().expect("edits | replug-v1 | replug-v2");
    let n: Option<usize> = a.next().and_then(|s| s.parse().ok());
    let seed: u64 = a
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5eed_1234_abcd);
    let mut r = match mode.as_str() {
        "edits" => edits(n.unwrap_or(200), seed),
        "replug-v1" => replug("v1", n.unwrap_or(20), seed),
        "replug-v2" => replug("v2", n.unwrap_or(20), seed),
        "mono" => mono(),
        "devices" => devices(),
        m => panic!("unknown mode {m}"),
    };
    // Leave nothing behind: players, probes, then every vdev off.
    r.player.stop();
    r.v1 = None;
    r.bh = None;
    r.model = Model::new();
    r.model.v1_on = false;
    r.model.app = false;
    r.model.wires = vec![];
    let mut c = r.model.config(None);
    c.devices.clear();
    let _ = r.e.apply(&c);
    drop(r.e);
    if r.failures.is_empty() {
        println!("PASS");
    } else {
        println!("{} FAILURE(S):", r.failures.len());
        r.failures.iter().for_each(|f| println!("  {f}"));
        std::process::exit(1);
    }
}
