//! System sleep/wake notifications (IOKit), delivered on a private run-loop thread.
//!
//! `NSWorkspaceDidWakeNotification` needs the main run loop, which the CLI does not run; IOKit's
//! notification port can be scheduled on any thread's run loop.
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::channel;
use std::thread::JoinHandle;

use anyhow::{Result, anyhow};
use objc2_core_foundation::{CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopDefaultMode};

type IoConnect = u32;
type Callback = unsafe extern "C-unwind" fn(*mut c_void, u32, u32, *mut c_void);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C-unwind" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut *mut c_void,
        cb: Callback,
        notifier: *mut u32,
    ) -> IoConnect;
    fn IODeregisterForSystemPower(notifier: *mut u32) -> i32;
    fn IOAllowPowerChange(conn: IoConnect, id: isize) -> i32;
    fn IOServiceClose(conn: IoConnect) -> i32;
    fn IONotificationPortGetRunLoopSource(port: *mut c_void) -> *mut CFRunLoopSource;
    fn IONotificationPortDestroy(port: *mut c_void);
}

// IOKit/IOMessage.h
const CAN_SYSTEM_SLEEP: u32 = 0xE000_0270;
const SYSTEM_WILL_SLEEP: u32 = 0xE000_0280;
const SYSTEM_HAS_POWERED_ON: u32 = 0xE000_0300;

/// What the callback does with a power message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Sleep is coming: acknowledge it, or the system waits ~30 s for us.
    Acknowledge,
    /// The system woke up: audio hardware may have changed under us.
    Wake,
    Ignore,
}

pub fn action(message: u32) -> Action {
    match message {
        CAN_SYSTEM_SLEEP | SYSTEM_WILL_SLEEP => Action::Acknowledge,
        SYSTEM_HAS_POWERED_ON => Action::Wake,
        _ => Action::Ignore,
    }
}

struct Ctx {
    conn: AtomicU32,
    on_wake: Box<dyn Fn() + Send>,
}

unsafe extern "C-unwind" fn callback(refcon: *mut c_void, _svc: u32, msg: u32, arg: *mut c_void) {
    // SAFETY: `refcon` is the `Ctx` leaked by `PowerWatch::start`, freed only after the run loop stops.
    let ctx = unsafe { &*(refcon as *const Ctx) };
    match action(msg) {
        Action::Acknowledge => {
            unsafe { IOAllowPowerChange(ctx.conn.load(Relaxed), arg as isize) };
        }
        Action::Wake => (ctx.on_wake)(),
        Action::Ignore => {}
    }
}

/// A run loop handle; `CFRunLoopStop` is documented thread-safe.
struct RunLoop(CFRetained<CFRunLoop>);
unsafe impl Send for RunLoop {}

/// Runs `on_wake` after every system wake until dropped.
pub struct PowerWatch {
    run_loop: RunLoop,
    thread: Option<JoinHandle<()>>,
}

impl PowerWatch {
    pub fn start(on_wake: impl Fn() + Send + 'static) -> Result<PowerWatch> {
        let (tx, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("sr-audio-power".into())
            .spawn(move || {
                let ctx = Box::into_raw(Box::new(Ctx {
                    conn: AtomicU32::new(0),
                    on_wake: Box::new(on_wake),
                }));
                let (mut port, mut notifier) = (std::ptr::null_mut(), 0u32);
                let conn = unsafe {
                    IORegisterForSystemPower(ctx as *mut c_void, &mut port, callback, &mut notifier)
                };
                let src = if conn == 0 || port.is_null() {
                    std::ptr::null_mut()
                } else {
                    unsafe { IONotificationPortGetRunLoopSource(port) }
                };
                let Some(rl) = CFRunLoop::current().filter(|_| !src.is_null()) else {
                    let _ = tx.send(Err(anyhow!("IORegisterForSystemPower failed")));
                    drop(unsafe { Box::from_raw(ctx) });
                    return;
                };
                unsafe { &*ctx }.conn.store(conn, Relaxed);
                rl.add_source(Some(unsafe { &*src }), unsafe { kCFRunLoopDefaultMode });
                let _ = tx.send(Ok(RunLoop(rl)));
                CFRunLoop::run();
                unsafe {
                    IODeregisterForSystemPower(&mut notifier);
                    IOServiceClose(conn);
                    IONotificationPortDestroy(port);
                    drop(Box::from_raw(ctx));
                }
            })?;
        match rx.recv() {
            Ok(Ok(run_loop)) => Ok(PowerWatch {
                run_loop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("power thread died")),
        }
    }
}

impl Drop for PowerWatch {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            // A stop that lands before the thread enters `run` is lost: repeat until it exits.
            while !t.is_finished() {
                self.run_loop.0.stop();
                std::thread::yield_now();
            }
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_messages_map_to_actions() {
        assert_eq!(action(CAN_SYSTEM_SLEEP), Action::Acknowledge);
        assert_eq!(action(SYSTEM_WILL_SLEEP), Action::Acknowledge);
        assert_eq!(action(SYSTEM_HAS_POWERED_ON), Action::Wake);
        assert_eq!(action(0xE000_0320), Action::Ignore); // WillPowerOn: devices not ready yet
        assert_eq!(action(0), Action::Ignore);
    }

    #[test]
    fn watch_starts_and_stops_cleanly() {
        let w = PowerWatch::start(|| {}).expect("IOKit power registration");
        drop(w);
    }
}
