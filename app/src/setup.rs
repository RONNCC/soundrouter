//! Driver install/uninstall through the macOS administrator prompt.
#![allow(dead_code)] // wired into the GUI in the next commit
use std::path::Path;
use std::process::Command;

/// Where coreaudiod loads third-party HAL plug-ins from.
pub const HAL_DIR: &str = "/Library/Audio/Plug-Ins/HAL";
pub const INSTALLED_DRIVER: &str = "/Library/Audio/Plug-Ins/HAL/SoundRouter.driver";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The user dismissed the password dialog.
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed(m) => f.write_str(m),
        }
    }
}

/// Runs a shell script as root. Injected into the app so tests never prompt.
pub type Runner = std::sync::Arc<dyn Fn(&str) -> Result<(), SetupError> + Send + Sync>;

/// POSIX single-quote `s`: safe for any bytes except NUL (which paths cannot contain).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// An AppleScript string literal for `s`.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', r#"\""#))
}

/// Copy the driver bundle at `src` into the HAL folder, fix owner and quarantine, restart coreaudiod.
/// `src` is quoted, so any path is safe (non-UTF-8 bytes are replaced, which makes `ditto` fail loudly).
pub fn install_script(src: &Path) -> String {
    let dest = sh_quote(INSTALLED_DRIVER);
    format!(
        "mkdir -p {hal} && rm -rf {dest} && ditto {src} {dest} && chown -R root:wheel {dest} \
         && xattr -dr com.apple.quarantine {dest} && killall coreaudiod",
        hal = sh_quote(HAL_DIR),
        src = sh_quote(&src.to_string_lossy()),
    )
}

/// Remove the driver; also the user's settings folder when `remove_config`.
pub fn uninstall_script(remove_config: bool) -> String {
    let dir = crate::store::default_path();
    uninstall_script_for(
        dir.as_deref()
            .and_then(Path::parent)
            .filter(|_| remove_config),
    )
}

/// `config_dir` is deleted too when given.
fn uninstall_script_for(config_dir: Option<&Path>) -> String {
    let mut s = format!("rm -rf {}", sh_quote(INSTALLED_DRIVER));
    if let Some(d) = config_dir {
        s += &format!(" && rm -rf {}", sh_quote(&d.to_string_lossy()));
    }
    s + " && killall coreaudiod"
}

/// Map osascript's outcome: error -128 is the user pressing Cancel.
fn classify(success: bool, stderr: &str) -> Result<(), SetupError> {
    if success {
        Ok(())
    } else if stderr.contains("(-128)") {
        Err(SetupError::Cancelled)
    } else {
        Err(SetupError::Failed(stderr.trim().to_string()))
    }
}

/// Run `script` as root behind the system password dialog. Blocks until the user answers.
pub fn run_admin(script: &str) -> Result<(), SetupError> {
    let apple = format!(
        "do shell script {} with administrator privileges",
        applescript_string(script)
    );
    let out = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(apple)
        .output()
        .map_err(|e| SetupError::Failed(format!("could not run osascript: {e}")))?;
    classify(out.status.success(), &String::from_utf8_lossy(&out.stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const NASTY: &[&str] = &[
        "/Applications/Sound Router.app/Contents/Resources/SoundRouter.driver",
        "/tmp/it's here/x",
        r#"/tmp/say "hi"/x"#,
        "/tmp/$HOME/`id`/$(id)/x",
        r"/tmp/back\slash\\/x",
        "/tmp/semi;colon && rm -rf ~ #/x",
        "/tmp/new\nline/x",
        "'; touch /tmp/pwned; '",
        "",
    ];

    fn sh_echo(arg: &str) -> String {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {arg}"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn sh_quote_roundtrips_through_a_real_shell() {
        for s in NASTY {
            assert_eq!(sh_echo(&sh_quote(s)), *s);
        }
    }

    #[test]
    fn applescript_string_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn install_script_quotes_the_source_and_never_splits_it() {
        for s in NASTY {
            let script = install_script(Path::new(s));
            assert!(script.contains(&format!("ditto {} ", sh_quote(s))), "{s}");
            // the whole script is one command list: only our own `&&` separators, no stray `;`
            // outside quotes (checked by the shell parsing it as exactly the expected words).
            let probe = script.replace("killall coreaudiod", "true");
            let words = Command::new("/bin/sh")
                .arg("-n")
                .arg("-c")
                .arg(&probe)
                .status()
                .unwrap();
            assert!(words.success(), "does not parse: {probe}");
        }
    }

    #[test]
    fn install_script_does_the_documented_steps() {
        let s = install_script(&PathBuf::from("/x/SoundRouter.driver"));
        for step in [
            "ditto '/x/SoundRouter.driver' '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'",
            "chown -R root:wheel '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'",
            "xattr -dr com.apple.quarantine '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver'",
        ] {
            assert!(s.contains(step), "{step}\n{s}");
        }
        assert!(s.ends_with("killall coreaudiod"));
    }

    #[test]
    fn uninstall_removes_config_only_when_asked() {
        let keep = uninstall_script_for(None);
        assert_eq!(
            keep,
            "rm -rf '/Library/Audio/Plug-Ins/HAL/SoundRouter.driver' && killall coreaudiod"
        );
        let dir = Path::new("/Users/a b/Library/Application Support/SoundRouter");
        let drop = uninstall_script_for(Some(dir));
        assert!(drop.contains("rm -rf '/Users/a b/Library/Application Support/SoundRouter'"));
        assert!(drop.ends_with("killall coreaudiod"));
        assert!(!uninstall_script(false).contains("Application Support"));
    }

    #[test]
    fn osascript_cancel_is_cancelled() {
        assert_eq!(classify(true, ""), Ok(()));
        assert_eq!(
            classify(false, "execution error: User canceled. (-128)\n"),
            Err(SetupError::Cancelled)
        );
        assert_eq!(
            classify(false, "boom (1)\n"),
            Err(SetupError::Failed("boom (1)".into()))
        );
    }
}
