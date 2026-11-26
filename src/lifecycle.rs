//! Leaving cleanly in every way the app can end or pause: the terminal window
//! being closed, Windows logging off or shutting down, and the Ally going to
//! sleep. Each of these gives us only a second or two, so the event handler
//! raises a flag, the streaming code closes the session, and the handler waits
//! briefly for that to finish before letting Windows carry on.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::core::BOOL;

/// The process is about to be ended from outside.
pub static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set once everything has been closed and it is safe to die.
pub static CLEANED_UP: AtomicBool = AtomicBool::new(false);
/// The machine is about to sleep: close the session, but keep the app.
pub static SUSPEND_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Set once the session has been closed after a suspend request.
pub static SESSION_CLOSED: AtomicBool = AtomicBool::new(false);

fn wait_for(flag: &AtomicBool, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Asks the streaming code to close everything and waits (briefly) until it has.
pub fn request_shutdown_and_wait() {
    SHUTDOWN_REQUESTED.store(true, Ordering::Relaxed);
    wait_for(&CLEANED_UP, Duration::from_millis(4000));
}

/// Asks the streaming code to close the session before the machine sleeps.
pub fn request_suspend_and_wait() {
    SESSION_CLOSED.store(false, Ordering::Relaxed);
    SUSPEND_REQUESTED.store(true, Ordering::Relaxed);
    wait_for(&SESSION_CLOSED, Duration::from_millis(1500));
    // Nobody was streaming, or it took too long: do not leave a stale request behind.
    SUSPEND_REQUESTED.store(false, Ordering::Relaxed);
}

unsafe extern "system" fn console_event(kind: u32) -> BOOL {
    // 2 = console window closed, 5 = log off, 6 = shut down.
    if matches!(kind, 2 | 5 | 6) {
        request_shutdown_and_wait();
        return BOOL(1);
    }
    BOOL(0) // Ctrl+C and Ctrl+Break keep their normal handling
}

/// Makes closing the terminal window behave like a normal exit.
pub fn install_console_handler() {
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(console_event), true);
    }
}
