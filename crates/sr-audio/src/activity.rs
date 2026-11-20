//! App Nap insurance: while a virtual device's IOProc runs, the process holds an `NSProcessInfo`
//! activity so macOS neither naps it (timer coalescing, throttled threads) nor lets the system idle it
//! into a longer wake-up latency. One token covers all devices; it begins with the first lease and ends
//! with the last.
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSActivityOptions, NSObjectProtocol, NSProcessInfo, NSString};

/// Begins and ends the activity; a trait so tests can count the calls instead of touching the OS.
pub trait Shim {
    type Token;
    fn begin(&self) -> Self::Token;
    fn end(&self, token: Self::Token);
}

/// Reference count over a [`Shim`]: `begin` on the 0 -> 1 step, `end` on the 1 -> 0 step.
pub struct Counter<S: Shim> {
    shim: S,
    held: usize,
    token: Option<S::Token>,
}

impl<S: Shim> Counter<S> {
    pub fn new(shim: S) -> Self {
        Counter {
            shim,
            held: 0,
            token: None,
        }
    }

    pub fn acquire(&mut self) {
        if self.held == 0 {
            self.token = Some(self.shim.begin());
        }
        self.held += 1;
    }

    pub fn release(&mut self) {
        self.held = self.held.saturating_sub(1);
        if self.held == 0
            && let Some(t) = self.token.take()
        {
            self.shim.end(t);
        }
    }

    pub fn held(&self) -> usize {
        self.held
    }
}

impl<S: Shim> Drop for Counter<S> {
    fn drop(&mut self) {
        if let Some(t) = self.token.take() {
            self.shim.end(t);
        }
    }
}

/// The real thing: `NSActivityUserInitiatedAllowingIdleSystemSleep | NSActivityLatencyCritical`.
#[derive(Default)]
pub struct Process;

/// The activity object `beginActivityWithOptions:reason:` returns.
pub struct Token(Retained<ProtocolObject<dyn NSObjectProtocol>>);
// SAFETY: NSProcessInfo is thread-safe, and the activity object is only handed back to it.
unsafe impl Send for Token {}

impl Shim for Process {
    type Token = Token;

    fn begin(&self) -> Token {
        Token(
            NSProcessInfo::processInfo().beginActivityWithOptions_reason(
                NSActivityOptions::UserInitiatedAllowingIdleSystemSleep
                    | NSActivityOptions::LatencyCritical,
                &NSString::from_str("SoundRouter routing"),
            ),
        )
    }

    fn end(&self, token: Token) {
        // SAFETY: the token came from `beginActivityWithOptions:reason:`.
        unsafe { NSProcessInfo::processInfo().endActivity(&token.0) };
    }
}

/// Shared by an engine's devices; each running device holds a [`Lease`].
pub struct Activity(Mutex<Counter<Process>>);

impl Default for Activity {
    fn default() -> Self {
        Activity(Mutex::new(Counter::new(Process)))
    }
}

impl Activity {
    pub fn lease(self: &Arc<Self>) -> Lease {
        self.lock().acquire();
        Lease(self.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Counter<Process>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Keeps the activity alive until dropped.
pub struct Lease(Arc<Activity>);

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.lock().release();
    }
}

/// True while Low Power Mode is on: macOS then lowers CPU clocks and may stretch wake-ups, so the
/// smaller audio delays are more likely to glitch.
pub fn low_power_mode() -> bool {
    NSProcessInfo::processInfo().isLowPowerModeEnabled()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone, Default)]
    struct Log(Rc<RefCell<Vec<String>>>);

    impl Shim for Log {
        type Token = u32;
        fn begin(&self) -> u32 {
            let n = self.0.borrow().len() as u32;
            self.0.borrow_mut().push(format!("begin {n}"));
            n
        }
        fn end(&self, t: u32) {
            self.0.borrow_mut().push(format!("end {t}"));
        }
    }

    #[test]
    fn one_activity_spans_the_first_to_the_last_holder() {
        let log = Log::default();
        let mut c = Counter::new(log.clone());
        c.acquire();
        c.acquire();
        assert_eq!(log.0.borrow().as_slice(), ["begin 0"]);
        c.release();
        assert_eq!(log.0.borrow().len(), 1, "still held by one device");
        c.release();
        assert_eq!(log.0.borrow().as_slice(), ["begin 0", "end 0"]);
        c.acquire();
        assert_eq!(log.0.borrow().as_slice(), ["begin 0", "end 0", "begin 2"]);
        assert_eq!(c.held(), 1);
    }

    #[test]
    fn an_extra_release_is_harmless_and_drop_ends_a_live_activity() {
        let log = Log::default();
        let mut c = Counter::new(log.clone());
        c.release();
        assert!(log.0.borrow().is_empty());
        c.acquire();
        drop(c);
        assert_eq!(log.0.borrow().as_slice(), ["begin 0", "end 0"]);
    }

    #[test]
    fn leases_hold_the_real_activity_and_release_it() {
        let a = Arc::new(Activity::default());
        let (l1, l2) = (a.lease(), a.lease());
        assert_eq!(a.lock().held(), 2);
        drop(l1);
        assert_eq!(a.lock().held(), 1);
        drop(l2);
        assert_eq!(a.lock().held(), 0);
    }
}
