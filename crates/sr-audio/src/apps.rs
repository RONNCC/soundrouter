//! HAL process objects grouped by the *responsible* app (helpers such as `com.apple.WebKit.GPU`
//! or `Chrome.helper` play audio on behalf of their owning app).
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{AcqRel, Acquire, Release},
};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use objc2_app_kit::{NSApplicationActivationPolicy, NSRunningApplication};
use objc2_core_audio::*;
use sr_core::backend::AppInfo;

use crate::hal::{self, SYSTEM};

#[derive(Clone, Debug)]
pub struct ProcInfo {
    pub obj: u32,
    pub pid: i32,
    /// The process's own bundle id (`pbid`), may be empty.
    pub bundle_id: String,
    pub running_output: bool,
}

/// Maps pids to apps; injectable for tests.
pub trait Resolver {
    /// Pid of the responsible process (may equal `pid`; <= 0 when unknown).
    fn responsible_pid(&self, pid: i32) -> i32;
    /// The running app with this pid: bundle id, display name, and whether it is a regular GUI app
    /// (activation policy `.regular`, as opposed to a daemon or agent).
    fn app_of_pid(&self, pid: i32) -> Option<(String, String, bool)>;
    /// Executable path of the process, used as a source key (`path:<exe>`) for processes with no bundle id.
    fn exe_path(&self, pid: i32) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppGroup {
    pub bundle_id: String,
    pub name: String,
    pub running_output: bool,
    /// The owning app is a regular GUI app.
    pub regular: bool,
    pub objs: Vec<u32>,
    pub pids: Vec<i32>,
    /// Own bundle ids of the member processes (helpers), sorted, deduped.
    pub members: Vec<String>,
}

pub fn group(procs: &[ProcInfo], r: &dyn Resolver) -> Vec<AppGroup> {
    let mut map: BTreeMap<String, AppGroup> = BTreeMap::new();
    // Our own output is the engine's mix: tapping it would feed the mix back into itself.
    let me = std::process::id() as i32;
    for p in procs.iter().filter(|p| p.pid != me) {
        let resp = r.responsible_pid(p.pid);
        let app = [resp, p.pid]
            .into_iter()
            .filter(|&x| x > 0)
            .find_map(|pid| r.app_of_pid(pid));
        let (bundle_id, name, regular) = match app {
            Some(a) => a,
            None if !p.bundle_id.is_empty() => (
                p.bundle_id.clone(),
                p.bundle_id.rsplit('.').next().unwrap_or("").to_owned(),
                false,
            ),
            // Command-line players (afplay, sox, ffplay, python) have no bundle id: key them by executable.
            None => match r.exe_path(p.pid) {
                Some(exe) => {
                    let name = exe.rsplit('/').next().unwrap_or(&exe).to_owned();
                    (format!("path:{exe}"), name, false)
                }
                None => continue,
            },
        };
        let g = map.entry(bundle_id.clone()).or_insert_with(|| AppGroup {
            bundle_id,
            name,
            running_output: false,
            regular,
            objs: vec![],
            pids: vec![],
            members: vec![],
        });
        g.running_output |= p.running_output;
        g.objs.push(p.obj);
        g.pids.push(p.pid);
        if !p.bundle_id.is_empty() && !g.members.contains(&p.bundle_id) {
            g.members.push(p.bundle_id.clone());
        }
    }
    let mut v: Vec<AppGroup> = map.into_values().collect();
    v.iter_mut().for_each(|g| g.members.sort());
    v
}

pub fn app_infos(groups: &[AppGroup]) -> Vec<AppInfo> {
    groups
        .iter()
        .map(|g| AppInfo {
            bundle_id: g.bundle_id.clone(),
            name: g.name.clone(),
            running_output: g.running_output,
            regular: g.regular,
        })
        .collect()
}

/// The app list the UI shows, refreshed off the caller's thread. A listing is several HAL round trips
/// per process and took a second or more while coreaudiod was busy: on the backend worker that froze
/// the meters and held back the user's next click. `get_with` returns the last list at once and runs the
/// next listing on a helper thread; only the very first call has nothing to return and waits.
#[derive(Default)]
pub struct AppListCache {
    list: Mutex<Option<Vec<AppInfo>>>,
    busy: AtomicBool,
}

impl AppListCache {
    pub fn get_with(
        self: &Arc<Self>,
        lister: impl FnOnce() -> Vec<AppInfo> + Send + 'static,
    ) -> Vec<AppInfo> {
        let cached = self
            .list
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(cached) = cached else {
            let first = lister();
            *self.list.lock().unwrap_or_else(PoisonError::into_inner) = Some(first.clone());
            return first;
        };
        if !self.busy.swap(true, AcqRel) {
            let me = self.clone();
            let spawned = std::thread::Builder::new()
                .name("sr-audio-apps".into())
                .spawn(move || {
                    let fresh = lister();
                    *me.list.lock().unwrap_or_else(PoisonError::into_inner) = Some(fresh);
                    me.busy.store(false, Release);
                });
            if spawned.is_err() {
                self.busy.store(false, Release);
            }
        }
        cached
    }
}

/// Live system resolver.
pub struct SystemResolver;

unsafe extern "C" {
    fn proc_pidpath(pid: i32, buf: *mut c_void, size: u32) -> i32;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

fn responsibility_fn() -> Option<unsafe extern "C" fn(i32) -> i32> {
    static F: OnceLock<Option<usize>> = OnceLock::new();
    let addr = (*F.get_or_init(|| {
        const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
        let name: &CStr = c"responsibility_get_pid_responsible_for_pid";
        let p = unsafe { dlsym(RTLD_DEFAULT, name.as_ptr()) };
        (!p.is_null()).then_some(p as usize)
    }))?;
    Some(unsafe { std::mem::transmute::<usize, unsafe extern "C" fn(i32) -> i32>(addr) })
}

impl Resolver for SystemResolver {
    fn responsible_pid(&self, pid: i32) -> i32 {
        responsibility_fn().map_or(pid, |f| unsafe { f(pid) })
    }
    fn exe_path(&self, pid: i32) -> Option<String> {
        let mut buf = [0u8; 4096];
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
        (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
    }
    fn app_of_pid(&self, pid: i32) -> Option<(String, String, bool)> {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        let bundle = app.bundleIdentifier()?.to_string();
        let name = app
            .localizedName()
            .map_or_else(|| bundle.clone(), |n| n.to_string());
        let regular = app.activationPolicy() == NSApplicationActivationPolicy::Regular;
        Some((bundle, name, regular))
    }
}

/// Pid and bundle id of each process object: neither changes for the life of an object, and each
/// is one HAL round trip (milliseconds, tens while coreaudiod is busy) per process, which made every
/// listing take a second with a few dozen audio processes.
struct ProcCache {
    ids: Mutex<BTreeMap<u32, (i32, String)>>,
    /// Bumped (under `ids`) by [`ProcCache::forget`]: a listing that began before it must not store
    /// what it read, or object ids reused after a coreaudiod restart would map to the old apps.
    generation: AtomicU64,
}

static PROCS: ProcCache = ProcCache {
    ids: Mutex::new(BTreeMap::new()),
    generation: AtomicU64::new(0),
};

/// What a listing reads from the HAL; injectable for tests.
trait ProcSource {
    fn objects(&self) -> Vec<u32>;
    /// Pid and bundle id of a process object; `None` when the pid is unreadable.
    fn identity(&self, obj: u32) -> Option<(i32, String)>;
    fn running_output(&self, obj: u32) -> bool;
}

struct HalProcs;

impl ProcSource for HalProcs {
    fn objects(&self) -> Vec<u32> {
        hal::get_u32s(SYSTEM, kAudioHardwarePropertyProcessObjectList).unwrap_or_default()
    }
    fn identity(&self, obj: u32) -> Option<(i32, String)> {
        let pid: i32 = hal::get_global(obj, kAudioProcessPropertyPID).ok()?;
        let bundle = hal::get_string(obj, kAudioProcessPropertyBundleID).unwrap_or_default();
        Some((pid, bundle))
    }
    fn running_output(&self, obj: u32) -> bool {
        hal::get_global::<u32>(obj, kAudioProcessPropertyIsRunningOutput).unwrap_or(0) != 0
    }
}

impl ProcCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, (i32, String)>> {
        self.ids.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn forget(&self) {
        let mut ids = self.lock();
        self.generation.fetch_add(1, AcqRel);
        ids.clear();
    }

    /// `with_output` also reads `kAudioProcessPropertyIsRunningOutput`, which changes and so is read
    /// every time (the slowest of the three reads); only the app list shows it.
    fn read(&self, hal: &dyn ProcSource, with_output: bool) -> Vec<ProcInfo> {
        let generation = self.generation.load(Acquire);
        let objs = hal.objects();
        let mut ids = {
            let mut c = self.lock();
            c.retain(|o, _| objs.contains(o));
            c.clone()
        };
        let before = ids.len();
        let mut fresh = BTreeMap::new();
        let list = objs
            .into_iter()
            .filter_map(|obj| {
                let (pid, bundle_id) = match ids.get(&obj) {
                    Some(known) => known.clone(),
                    None => {
                        let read = hal.identity(obj)?;
                        ids.insert(obj, read.clone());
                        fresh.insert(obj, read.clone());
                        read
                    }
                };
                Some(ProcInfo {
                    obj,
                    pid,
                    bundle_id,
                    running_output: with_output && hal.running_output(obj),
                })
            })
            .collect();
        if ids.len() != before {
            // Merged into the live map, and only if nothing was forgotten since this listing began.
            let mut live = self.lock();
            if self.generation.load(Acquire) == generation {
                live.extend(fresh);
            }
        }
        list
    }
}

/// Object ids restart when coreaudiod does: forget what was read.
pub fn forget_processes() {
    PROCS.forget();
}

fn read_processes(with_output: bool) -> Vec<ProcInfo> {
    PROCS.read(&HalProcs, with_output)
}

pub fn process_list() -> Vec<ProcInfo> {
    read_processes(true)
}

pub fn list_groups() -> Vec<AppGroup> {
    group(&process_list(), &SystemResolver)
}

/// [`list_groups`] without `running_output` (always false): what tapping needs, at about a third of
/// the HAL calls.
pub fn list_groups_for_taps() -> Vec<AppGroup> {
    group(&read_processes(false), &SystemResolver)
}

/// Where `installed_apps` looks. shortcut: top level and `Utilities` only, no nested folders.
pub const APP_DIRS: [&str; 4] = [
    "/Applications",
    "/Applications/Utilities",
    "/System/Applications",
    "/System/Applications/Utilities",
];

/// Installed `.app` bundles directly inside `dirs`, sorted by name, one entry per bundle id, so a user
/// can wire an app that is not running. The id comes from `Info.plist` via `plutil` (reads binary
/// and XML plists; ~1.5 ms per app). Bundles without an id are skipped.
pub fn installed_apps_in(dirs: &[&std::path::Path]) -> Vec<AppInfo> {
    let mut out: Vec<AppInfo> = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for path in rd.filter_map(|e| Some(e.ok()?.path())) {
            if path.extension().is_none_or(|e| e != "app") {
                continue;
            }
            let Some(name) = path.file_stem().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            let id = std::process::Command::new("/usr/bin/plutil")
                .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
                .arg(path.join("Contents/Info.plist"))
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                .filter(|id| !id.is_empty());
            if let Some(bundle_id) = id
                && !out.iter().any(|a| a.bundle_id == bundle_id)
            {
                out.push(AppInfo {
                    bundle_id,
                    name,
                    running_output: false,
                    regular: true,
                });
            }
        }
    }
    out.sort_by_cached_key(|a| a.name.to_lowercase());
    out
}

pub fn installed_apps() -> Vec<AppInfo> {
    let dirs: Vec<&std::path::Path> = APP_DIRS.iter().map(std::path::Path::new).collect();
    installed_apps_in(&dirs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Fake(
        HashMap<i32, i32>,
        HashMap<i32, (&'static str, &'static str, bool)>,
    );
    impl Resolver for Fake {
        fn responsible_pid(&self, pid: i32) -> i32 {
            *self.0.get(&pid).unwrap_or(&pid)
        }
        fn exe_path(&self, pid: i32) -> Option<String> {
            (pid == 50 || pid == 51).then(|| "/usr/bin/afplay".to_owned())
        }
        fn app_of_pid(&self, pid: i32) -> Option<(String, String, bool)> {
            self.1
                .get(&pid)
                .map(|(b, n, r)| (b.to_string(), n.to_string(), *r))
        }
    }
    fn p(obj: u32, pid: i32, b: &str, out: bool) -> ProcInfo {
        ProcInfo {
            obj,
            pid,
            bundle_id: b.into(),
            running_output: out,
        }
    }

    #[test]
    fn groups_helpers_under_responsible_app() {
        let r = Fake(
            HashMap::from([(20, 10), (21, 10), (30, -1)]),
            HashMap::from([
                (10, ("com.google.Chrome", "Chrome", true)),
                (40, ("com.apple.Safari", "Safari", true)),
                (60, ("com.apple.CoreSpeech", "CoreSpeech", false)),
            ]),
        );
        let procs = [
            p(1, 10, "com.google.Chrome", false),
            p(2, 20, "com.google.Chrome.helper", true),
            p(3, 21, "com.google.Chrome.helper", false),
            p(4, 30, "com.example.daemon", false), // no GUI app: own bundle id
            p(5, 31, "", false),                   // unidentifiable: skipped
            p(7, 50, "", true),                    // afplay: no bundle id, keyed by path
            p(8, 51, "", false),                   // second afplay joins the same group
            p(6, 40, "com.apple.WebKit.GPU", false), // pid itself is an app
            p(9, 60, "com.apple.CoreSpeech", false), // an agent: not a regular app
        ];
        let g = group(&procs, &r);
        let chrome = g
            .iter()
            .find(|g| g.bundle_id == "com.google.Chrome")
            .unwrap();
        assert_eq!(chrome.objs, [1, 2, 3]);
        assert!(chrome.running_output);
        assert_eq!(
            chrome.members,
            ["com.google.Chrome", "com.google.Chrome.helper"]
        );
        assert_eq!(chrome.name, "Chrome");
        assert!(
            g.iter()
                .any(|g| g.bundle_id == "com.example.daemon" && g.name == "daemon")
        );
        assert!(g.iter().any(|g| g.bundle_id == "com.apple.Safari"));
        let ap = g
            .iter()
            .find(|g| g.bundle_id == "path:/usr/bin/afplay")
            .unwrap();
        assert_eq!((ap.name.as_str(), ap.objs.clone()), ("afplay", vec![7, 8]));
        assert!(ap.running_output && ap.members.is_empty());
        assert_eq!(g.len(), 5);
        // Only GUI apps are `regular`: agents, bundle-id-only daemons and CLI players are not.
        let regular: Vec<_> = app_infos(&g)
            .into_iter()
            .filter(|a| a.regular)
            .map(|a| a.bundle_id)
            .collect();
        assert_eq!(regular, ["com.apple.Safari", "com.google.Chrome"]);
    }

    /// Our own process plays the engine's mix; a tap on it would feed that mix back into itself
    /// (measured live: the own-process tap carries the engine output).
    #[test]
    fn own_process_is_never_a_source() {
        let me = std::process::id() as i32;
        let r = Fake(
            HashMap::new(),
            HashMap::from([(me, ("com.ronncc.soundrouter", "SoundRouter", true))]),
        );
        assert!(group(&[p(1, me, "com.ronncc.soundrouter", true)], &r).is_empty());
    }

    #[test]
    fn installed_apps_reads_bundle_ids_and_skips_the_rest() {
        let dir = std::env::temp_dir().join(format!("sr-installed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let plist = |name: &str, body: &str| {
            let c = dir.join(format!("{name}.app/Contents"));
            std::fs::create_dir_all(&c).unwrap();
            std::fs::write(
                c.join("Info.plist"),
                format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>{body}</dict></plist>"
                ),
            )
            .unwrap();
        };
        let id = |s: &str| format!("<key>CFBundleIdentifier</key><string>{s}</string>");
        plist("Zed", &id("dev.zed.Zed"));
        plist("alpha", &id("com.x.alpha"));
        plist("Dup", &id("com.x.alpha"));
        plist("NoId", "<key>CFBundleName</key><string>n</string>");
        std::fs::create_dir_all(dir.join("Folder")).unwrap();
        let got = installed_apps_in(&[dir.as_path(), std::path::Path::new("/nonexistent-dir")]);
        std::fs::remove_dir_all(&dir).unwrap();
        let ids: Vec<_> = got
            .iter()
            .map(|a| (a.bundle_id.as_str(), a.name.as_str()))
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert!(ids.contains(&("dev.zed.Zed", "Zed")));
        assert!(ids.iter().any(|(b, _)| *b == "com.x.alpha"));
        assert!(got.iter().all(|a| !a.running_output && a.regular));
        assert!(got[0].name.to_lowercase() < got[1].name.to_lowercase());
    }

    struct FakeHal<'a> {
        objs: Vec<u32>,
        apps: HashMap<u32, (i32, &'static str)>,
        /// Runs while the first identity is being read: a coreaudiod restart mid-listing.
        during: Option<&'a dyn Fn()>,
    }

    impl ProcSource for FakeHal<'_> {
        fn objects(&self) -> Vec<u32> {
            self.objs.clone()
        }
        fn identity(&self, obj: u32) -> Option<(i32, String)> {
            if let Some(f) = self.during {
                f();
            }
            self.apps.get(&obj).map(|&(p, b)| (p, b.to_owned()))
        }
        fn running_output(&self, _: u32) -> bool {
            false
        }
    }

    /// A listing that began before `forget` must not store what it read: after a coreaudiod restart the
    /// object ids are reused, and a stale id -> (pid, bundle) would tap the wrong app.
    #[test]
    fn a_listing_that_began_before_a_forget_does_not_restore_stale_ids() {
        let cache = ProcCache {
            ids: Mutex::new(BTreeMap::new()),
            generation: AtomicU64::new(0),
        };
        let apps = HashMap::from([(7, (100, "old.app"))]);
        let forget = || cache.forget();
        let hal = FakeHal {
            objs: vec![7],
            apps,
            during: Some(&forget),
        };
        // The listing itself still reports what it read.
        assert_eq!(cache.read(&hal, false)[0].bundle_id, "old.app");
        assert!(cache.lock().is_empty(), "stale entry restored");
        // After the restart object 7 is another app: the next listing reads it afresh.
        let hal = FakeHal {
            objs: vec![7],
            apps: HashMap::from([(7, (200, "new.app"))]),
            during: None,
        };
        let l = cache.read(&hal, false);
        assert_eq!((l[0].pid, l[0].bundle_id.as_str()), (200, "new.app"));
        assert_eq!(cache.lock().get(&7), Some(&(200, "new.app".to_owned())));
    }

    /// New entries are merged into the live map, not written over it, and vanished objects drop out.
    #[test]
    fn a_listing_merges_into_the_cache_and_drops_vanished_objects() {
        let cache = ProcCache {
            ids: Mutex::new(BTreeMap::from([
                (1, (10, "a".to_owned())),
                (9, (90, "gone".to_owned())),
            ])),
            generation: AtomicU64::new(0),
        };
        let hal = FakeHal {
            objs: vec![1, 2],
            apps: HashMap::from([(2, (20, "b"))]),
            during: None,
        };
        let l = cache.read(&hal, false);
        assert_eq!(l.iter().map(|p| p.pid).collect::<Vec<_>>(), [10, 20]);
        let keys: Vec<u32> = cache.lock().keys().copied().collect();
        assert_eq!(keys, [1, 2]);
    }

    fn info(name: &str) -> AppInfo {
        AppInfo {
            bundle_id: name.into(),
            name: name.into(),
            running_output: false,
            regular: true,
        }
    }

    /// A slow listing (coreaudiod busy) must never hold up the caller after the first one.
    #[test]
    fn a_slow_app_listing_never_blocks_the_caller_after_the_first() {
        let cache = Arc::new(AppListCache::default());
        assert_eq!(cache.get_with(|| vec![info("a")])[0].name, "a");
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let (done, finished) = std::sync::mpsc::channel::<()>();
        // Returns the old list while the new listing waits on `gate`; a second call starts no second one.
        let slow = move || {
            gate.recv().unwrap();
            done.send(()).unwrap();
            vec![info("b")]
        };
        assert_eq!(cache.get_with(slow)[0].name, "a");
        assert_eq!(
            cache.get_with(|| panic!("a listing is already running"))[0].name,
            "a"
        );
        release.send(()).unwrap();
        finished.recv().unwrap();
        // The helper stores the list right after `done`: until then the call returns the old one.
        let seen = loop {
            let seen = cache.get_with(|| vec![info("c")])[0].name.clone();
            if seen != "a" {
                break seen;
            }
            std::thread::yield_now();
        };
        assert_eq!(seen, "b");
    }
}
