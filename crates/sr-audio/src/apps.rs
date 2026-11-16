//! HAL process objects grouped by the *responsible* app (helpers such as `com.apple.WebKit.GPU`
//! or `Chrome.helper` play audio on behalf of their owning app).
use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

use objc2_app_kit::NSRunningApplication;
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
    /// (bundle id, display name) of the running GUI app with this pid.
    fn app_of_pid(&self, pid: i32) -> Option<(String, String)>;
    /// Executable path of the process, used as a source key (`path:<exe>`) for processes with no bundle id.
    fn exe_path(&self, pid: i32) -> Option<String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppGroup {
    pub bundle_id: String,
    pub name: String,
    pub running_output: bool,
    pub objs: Vec<u32>,
    pub pids: Vec<i32>,
    /// Own bundle ids of the member processes (helpers), sorted, deduped.
    pub members: Vec<String>,
}

pub fn group(procs: &[ProcInfo], r: &dyn Resolver) -> Vec<AppGroup> {
    let mut map: BTreeMap<String, AppGroup> = BTreeMap::new();
    for p in procs {
        let resp = r.responsible_pid(p.pid);
        let app = [resp, p.pid]
            .into_iter()
            .filter(|&x| x > 0)
            .find_map(|pid| r.app_of_pid(pid));
        let (bundle_id, name) = match app {
            Some(a) => a,
            None if !p.bundle_id.is_empty() => (
                p.bundle_id.clone(),
                p.bundle_id.rsplit('.').next().unwrap_or("").to_owned(),
            ),
            // Command-line players (afplay, sox, ffplay, python) have no bundle id: key them by executable.
            None => match r.exe_path(p.pid) {
                Some(exe) => {
                    let name = exe.rsplit('/').next().unwrap_or(&exe).to_owned();
                    (format!("path:{exe}"), name)
                }
                None => continue,
            },
        };
        let g = map.entry(bundle_id.clone()).or_insert_with(|| AppGroup {
            bundle_id,
            name,
            running_output: false,
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
        })
        .collect()
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
    fn app_of_pid(&self, pid: i32) -> Option<(String, String)> {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        let bundle = app.bundleIdentifier()?.to_string();
        let name = app
            .localizedName()
            .map_or_else(|| bundle.clone(), |n| n.to_string());
        Some((bundle, name))
    }
}

pub fn process_list() -> Vec<ProcInfo> {
    let objs = hal::get_u32s(SYSTEM, kAudioHardwarePropertyProcessObjectList).unwrap_or_default();
    objs.into_iter()
        .filter_map(|obj| {
            let pid: i32 = hal::get_global(obj, kAudioProcessPropertyPID).ok()?;
            Some(ProcInfo {
                obj,
                pid,
                bundle_id: hal::get_string(obj, kAudioProcessPropertyBundleID).unwrap_or_default(),
                running_output: hal::get_global::<u32>(obj, kAudioProcessPropertyIsRunningOutput)
                    .unwrap_or(0)
                    != 0,
            })
        })
        .collect()
}

pub fn list_groups() -> Vec<AppGroup> {
    group(&process_list(), &SystemResolver)
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
            let id = std::process::Command::new("plutil")
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
        HashMap<i32, (&'static str, &'static str)>,
    );
    impl Resolver for Fake {
        fn responsible_pid(&self, pid: i32) -> i32 {
            *self.0.get(&pid).unwrap_or(&pid)
        }
        fn exe_path(&self, pid: i32) -> Option<String> {
            (pid == 50 || pid == 51).then(|| "/usr/bin/afplay".to_owned())
        }
        fn app_of_pid(&self, pid: i32) -> Option<(String, String)> {
            self.1
                .get(&pid)
                .map(|(b, n)| (b.to_string(), n.to_string()))
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
                (10, ("com.google.Chrome", "Chrome")),
                (40, ("com.apple.Safari", "Safari")),
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
        assert_eq!(g.len(), 4);
        assert_eq!(app_infos(&g).len(), 4);
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
        assert!(got.iter().all(|a| !a.running_output));
        assert!(got[0].name.to_lowercase() < got[1].name.to_lowercase());
    }
}
