//! `SR_TRACE_APPLY=1`: one stderr line per stage of the click-to-first-meter timeline, with the
//! milliseconds since the last [`click`]. Off by default; the check is one cached load.

use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

static ON: OnceLock<bool> = OnceLock::new();
/// The last click and the stages already reported once since then.
static STATE: Mutex<Option<(Instant, Vec<&'static str>)>> = Mutex::new(None);

pub fn enabled() -> bool {
    *ON.get_or_init(|| std::env::var_os("SR_TRACE_APPLY").is_some_and(|v| v == "1"))
}

/// Starts a new timeline at "now" (the UI event).
pub fn click(what: &str) {
    if enabled() {
        *STATE.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), Vec::new()));
        eprintln!("[trace +0.0ms] {what}");
    }
}

/// One stage of the running timeline (ignored before the first click).
pub fn stage(what: &str) {
    if enabled()
        && let Some((t, _)) = &*STATE.lock().unwrap_or_else(PoisonError::into_inner)
    {
        eprintln!("[trace +{:.1}ms] {what}", t.elapsed().as_secs_f64() * 1e3);
    }
}

/// Like [`stage`], but only the first time since the last click: for per-frame code paths.
pub fn once(what: &'static str) {
    if !enabled() {
        return;
    }
    let mut s = STATE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((t, seen)) = &mut *s
        && !seen.contains(&what)
    {
        seen.push(what);
        eprintln!("[trace +{:.1}ms] {what}", t.elapsed().as_secs_f64() * 1e3);
    }
}
