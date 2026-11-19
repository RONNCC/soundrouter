//! Start at Login: a per-user LaunchAgent that runs `SoundRouter --background`.
//!
//! Only the plist is managed. launchd loads `~/Library/LaunchAgents` at the next login by itself;
//! `launchctl bootstrap` here would run the job at once (RunAtLoad) and start a second copy of
//! the running app, and `bootout` from an app that launchd started would SIGTERM that app.
use std::io;
use std::path::{Path, PathBuf};

pub const LABEL: &str = "com.ronncc.soundrouter.login";

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// LaunchAgent plist that starts `exe --background` at login.
pub fn plist(exe: &Path) -> String {
    let exe = xml_escape(&exe.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{exe}</string>
		<string>--background</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
</dict>
</plist>
"#
    )
}

fn agent_path() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or(io::ErrorKind::NotFound)?;
    Ok(plist_path(Path::new(&home)))
}

pub fn is_enabled() -> bool {
    agent_path().is_ok_and(|p| p.exists())
}

pub fn set_enabled(on: bool) -> io::Result<()> {
    write_agent(&agent_path()?, &std::env::current_exe()?, on)
}

/// Create (or refresh, e.g. after the app moved) or remove the agent plist at `path`.
fn write_agent(path: &Path, exe: &Path, on: bool) -> io::Result<()> {
    if !on {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    // The release dmg (volume "SoundRouter") or a Gatekeeper-translocated copy is gone by the next login (and a launch from one would
    // overwrite a good agent on startup), so the agent must never point there.
    let exe_str = exe.to_string_lossy();
    if exe_str.starts_with("/Volumes/SoundRouter") || exe_str.contains("/AppTranslocation/") {
        return Err(io::Error::other(
            "SoundRouter is running from a disk image or a temporary location; move it to Applications first",
        ));
    }
    let text = plist(exe);
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_executable_in_background_at_load() {
        let p = plist(Path::new(
            "/Applications/SoundRouter.app/Contents/MacOS/SoundRouter",
        ));
        assert!(p.contains("<string>com.ronncc.soundrouter.login</string>"));
        assert!(p.contains(
            "<string>/Applications/SoundRouter.app/Contents/MacOS/SoundRouter</string>\n\t\t<string>--background</string>"
        ));
        assert!(p.contains("<key>RunAtLoad</key>\n\t<true/>"));
    }

    #[test]
    fn plist_escapes_xml_in_the_path() {
        let p = plist(Path::new("/tmp/A&B <x>/SoundRouter"));
        assert!(p.contains("/tmp/A&amp;B &lt;x&gt;/SoundRouter"));
    }

    #[test]
    fn plist_lives_in_the_user_launch_agents_dir() {
        assert_eq!(
            plist_path(Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/com.ronncc.soundrouter.login.plist")
        );
    }

    #[test]
    fn enable_refuses_a_disk_image_or_translocated_exe_and_keeps_the_old_agent() {
        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        let good = Path::new("/Applications/SoundRouter.app/Contents/MacOS/SoundRouter");
        write_agent(&path, good, true).unwrap();
        for bad in [
            "/Volumes/SoundRouter/SoundRouter.app/Contents/MacOS/SoundRouter",
            "/private/var/folders/x/T/AppTranslocation/ABC/d/SoundRouter.app/Contents/MacOS/SoundRouter",
        ] {
            assert!(write_agent(&path, Path::new(bad), true).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), plist(good));
        }
        write_agent(&path, Path::new(good), false).unwrap(); // turning it off still works
    }

    #[test]
    fn enable_writes_and_disable_removes_the_plist() {
        let home = tempfile::tempdir().unwrap();
        let path = plist_path(home.path());
        let exe = Path::new("/Applications/SoundRouter.app/Contents/MacOS/SoundRouter");
        write_agent(&path, exe, true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), plist(exe));
        write_agent(&path, exe, true).unwrap(); // refresh is idempotent
        write_agent(&path, Path::new("/moved/SoundRouter"), true).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("/moved/SoundRouter")
        );
        write_agent(&path, exe, false).unwrap();
        assert!(!path.exists());
        write_agent(&path, exe, false).unwrap(); // already gone is fine
    }
}
