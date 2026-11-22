//! `soundrouter://` links: the OS hands them to the running app (or launches it with one) as a
//! "get URL" Apple event; they wait here until the app's event loop takes them.
use eframe::egui::Context;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A flood of links (any local process can send them) is not worth remembering.
const MAX_PENDING: usize = 8;
/// Longest link kept; a preset name is at most 64 characters, even fully percent-encoded.
const MAX_LEN: usize = 1024;

/// Links accepted per `WINDOW`: every one re-applies a preset (the engine is rebuilt), and any local process or
/// web page can send them. A person pressing a button does not come close.
const BURST: usize = 4;
const WINDOW: Duration = Duration::from_secs(2);

#[derive(Default)]
struct State {
    pending: Vec<String>,
    /// Start of the current window and the links accepted in it.
    window: Option<(Instant, usize)>,
}

#[derive(Default)]
struct Queue(Mutex<State>);

impl Queue {
    fn push(&self, url: String) {
        self.push_at(url, Instant::now());
    }

    fn push_at(&self, url: String, now: Instant) {
        let mut s = self.0.lock().unwrap();
        if s.pending.len() >= MAX_PENDING || url.len() > MAX_LEN {
            return;
        }
        let (start, n) = match s.window {
            Some((start, n)) if now.duration_since(start) < WINDOW => (start, n),
            _ => (now, 0),
        };
        s.window = Some((start, n + 1));
        if n < BURST {
            s.pending.push(url);
        }
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap().pending)
    }
}

static QUEUE: OnceLock<Queue> = OnceLock::new();
static WAKE: OnceLock<Context> = OnceLock::new();

fn queue() -> &'static Queue {
    QUEUE.get_or_init(Queue::default)
}

/// Links received since the last call.
pub fn take() -> Vec<String> {
    queue().take()
}

/// Wake the event loop when a link arrives. Links that came earlier wait in the queue.
pub fn wake_on_link(ctx: &Context) {
    let _ = WAKE.set(ctx.clone());
    if !queue().0.lock().unwrap().pending.is_empty() {
        ctx.request_repaint();
    }
}

#[cfg(target_os = "macos")]
pub use mac::install;

/// Without macOS there are no links to receive.
#[cfg(not(target_os = "macos"))]
pub fn install() {}

#[cfg(target_os = "macos")]
mod mac {
    //! A launch by link delivers its Apple event before `applicationDidFinishLaunching`, which is
    //! before the window creator runs, so the handler is installed from `main`, before the event
    //! loop exists. The class and the selector are plain Objective-C; no typed AppKit binding is
    //! needed.
    use super::{WAKE, queue};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject};
    use objc2::{AnyThread, class, define_class, msg_send, sel};
    use objc2_foundation::NSString;
    use std::cell::RefCell;

    /// 'GURL' (`kInternetEventClass` and `kAEGetURL`) and '----' (`keyDirectObject`).
    const GURL: u32 = u32::from_be_bytes(*b"GURL");
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "SRLinkHandler"]
        #[ivars = ()]
        struct LinkHandler;

        impl LinkHandler {
            #[unsafe(method(handleGetURL:withReplyEvent:))]
            fn handle_get_url(&self, event: &AnyObject, _reply: &AnyObject) {
                // SAFETY: `event` is the NSAppleEventDescriptor AppKit passes to the handler.
                let url: Option<Retained<NSString>> = unsafe {
                    let param: Option<Retained<AnyObject>> =
                        msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
                    param.and_then(|p| msg_send![&*p, stringValue])
                };
                if let Some(url) = url {
                    queue().push(url.to_string());
                    if let Some(ctx) = WAKE.get() {
                        ctx.request_repaint();
                    }
                }
            }
        }
    );

    impl LinkHandler {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            // SAFETY: NSObject's `init`.
            unsafe { msg_send![super(this), init] }
        }
    }

    thread_local! {
        /// The manager does not retain its handler; main-thread only, like the registration.
        static HANDLER: RefCell<Option<Retained<LinkHandler>>> = const { RefCell::new(None) };
    }

    /// Register for links. Call once on the main thread, before `eframe::run_native`.
    pub fn install() {
        let handler = LinkHandler::new();
        // SAFETY: the selector matches the method defined above; the handler stays alive in HANDLER.
        unsafe {
            let manager: Retained<AnyObject> =
                msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            let _: () = msg_send![
                &*manager,
                setEventHandler: &*handler,
                andSelector: sel!(handleGetURL:withReplyEvent:),
                forEventClass: GURL,
                andEventID: GURL
            ];
        }
        HANDLER.with(|h| *h.borrow_mut() = Some(handler));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_come_out_once_in_order() {
        let q = Queue::default();
        q.push("soundrouter://preset/a".into());
        q.push("soundrouter://preset/b".into());
        assert_eq!(
            q.take(),
            ["soundrouter://preset/a", "soundrouter://preset/b"]
        );
        assert!(q.take().is_empty());
    }

    #[test]
    fn a_burst_is_capped_per_window_and_the_window_reopens() {
        let (q, t0) = (Queue::default(), Instant::now());
        for i in 0..100 {
            q.push_at(format!("soundrouter://preset/{i}"), t0);
        }
        assert_eq!(q.take().len(), BURST);
        q.push_at("soundrouter://preset/late".into(), t0 + WINDOW / 2);
        assert!(q.take().is_empty(), "still the same window");
        q.push_at("soundrouter://preset/next".into(), t0 + WINDOW);
        assert_eq!(q.take(), ["soundrouter://preset/next"]);
    }

    #[test]
    fn a_flood_taken_every_frame_is_still_limited() {
        // Taking after each link (what the event loop does) must not reset the limit: every accepted link
        // re-applies a preset, which rebuilds the engine.
        let q = Queue::default();
        let mut applied = 0;
        for i in 0..100 {
            q.push(format!("soundrouter://preset/{i}"));
            applied += q.take().len();
        }
        assert_eq!(applied, BURST);
    }

    #[test]
    fn links_nobody_takes_pile_up_only_to_the_pending_cap() {
        let (q, t0) = (Queue::default(), Instant::now());
        for i in 0..100 {
            q.push_at(format!("soundrouter://preset/{i}"), t0 + WINDOW * i);
        }
        assert_eq!(q.take().len(), MAX_PENDING);
    }

    #[test]
    fn an_oversized_link_is_dropped() {
        let q = Queue::default();
        q.push(format!("soundrouter://preset/{}", "x".repeat(MAX_LEN)));
        assert!(q.take().is_empty());
    }
}
