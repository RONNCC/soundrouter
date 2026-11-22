//! File logger for the GUI: Finder-launched apps have no visible stderr.
//! Writes `~/Library/Logs/SoundRouter/soundrouter.log`, mirrors to stderr on a TTY.
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Logs/SoundRouter/soundrouter.log"))
}

/// Move `path` to `path.1` (replacing any old one) if it exceeds `max` bytes. Missing file is fine.
fn rotate(path: &Path, max: u64) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(m) if m.len() > max => {
            let mut old = path.as_os_str().to_owned();
            old.push(".1");
            fs::rename(path, old)
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Open the log file, rotating first.
fn open_log(path: &Path) -> io::Result<fs::File> {
    fs::create_dir_all(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    rotate(path, MAX_BYTES)?;
    OpenOptions::new().create(true).append(true).open(path)
}

/// Install the logger and panic hook. Returns the log file path on success;
/// on failure (no HOME, unwritable dir) falls back to stderr only.
pub fn init() -> Option<PathBuf> {
    // Unset or malformed RUST_LOG falls back to info rather than logging nothing.
    let mut fb = env_filter::Builder::new();
    let custom = std::env::var("RUST_LOG").is_ok_and(|s| fb.try_parse(&s).is_ok());
    if !custom {
        fb = env_filter::Builder::new();
        fb.filter_level(log::LevelFilter::Info);
    }
    let filter = fb.build();
    let mut dispatch = fern::Dispatch::new()
        .format(|out, msg, rec| {
            out.finish(format_args!(
                "{} {:5} {} {}",
                jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ"),
                rec.level(),
                rec.target(),
                msg
            ))
        })
        .level(filter.filter())
        .filter(move |m| filter.enabled(m));
    if io::stderr().is_terminal() {
        dispatch = dispatch.chain(io::stderr());
    }
    let file = log_path().map(|p| open_log(&p).map(|f| (p, f)));
    let (path, file_err) = match file {
        Some(Ok((p, f))) => {
            dispatch = dispatch.chain(f);
            (Some(p), None)
        }
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    if dispatch.apply().is_err() {
        return None; // a logger is already installed
    }
    if let Some(e) = file_err {
        log::warn!("cannot open log file: {e}");
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "panic: {info}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        prev(info);
    }));
    path
}

/// One-line startup banner: version, macOS version, driver presence, and the raw TCC answers for
/// this process (which, launched from Finder or `open`, is the app's own TCC subject).
pub fn banner(log_file: Option<&Path>) {
    let os = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    let driver = sr_audio::driver::find_plugin().is_some();
    log::info!(
        "SoundRouter {} starting; macOS {os}; driver plug-in {}; TCC audio capture {:?}, microphone {:?}; log file {}",
        env!("CARGO_PKG_VERSION"),
        if driver { "found" } else { "NOT found" },
        sr_audio::tcc::audio_capture_status(),
        sr_audio::tcc::microphone_status(),
        log_file.map_or_else(|| "(none)".into(), |p| p.display().to_string()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_only_when_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.log");
        rotate(&p, 10).unwrap(); // missing: ok
        fs::write(&p, "0123456789").unwrap();
        rotate(&p, 10).unwrap(); // == limit: kept
        assert!(p.exists());
        fs::write(&p, "new-long-content").unwrap();
        fs::write(dir.path().join("a.log.1"), "stale").unwrap();
        rotate(&p, 10).unwrap();
        assert!(!p.exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("a.log.1")).unwrap(),
            "new-long-content"
        );
    }
}
