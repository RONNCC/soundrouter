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
}
