//! rustyfiles fork: demand-driven frames.
//!
//! Upstream invalidates every window on every vsync, so an idle window still wakes the UI thread
//! 60–240 times per second (≈1–3% of a core doing nothing). GPUI core already supports
//! demand-driven platforms through `PlatformWindow::frame_waker` / `schedule_frame`; this module
//! implements them for Windows:
//! - the waker sets a "frame wanted" flag and wakes the vsync thread,
//! - the vsync thread only invalidates windows when a frame is wanted,
//! - after ~1 s without demand it stops waiting on vsync and parks (waking every 250 ms to keep
//!   servicing device-lost checks and hybrid-renderer switches).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

static WANTED: AtomicBool = AtomicBool::new(true);
static PARK: Mutex<bool> = Mutex::new(false);
static PARK_CV: Condvar = Condvar::new();
static LAST_DEMAND: Mutex<Option<Instant>> = Mutex::new(None);

/// Disable with `RUSTYFILES_CONTINUOUS_FRAMES=1` (upstream behaviour, for comparison).
fn continuous() -> bool {
    static C: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *C.get_or_init(|| std::env::var_os("RUSTYFILES_CONTINUOUS_FRAMES").is_some())
}

/// A window needs a frame. Cheap; callable from any thread.
pub(crate) fn request() {
    if !WANTED.swap(true, Ordering::AcqRel) {
        let mut woke = PARK.lock().unwrap();
        *woke = true;
        PARK_CV.notify_all();
    }
}

/// Called by the vsync thread each iteration: returns true when windows should be invalidated.
pub(crate) fn take() -> bool {
    if continuous() {
        return true;
    }
    let wanted = WANTED.swap(false, Ordering::AcqRel);
    if wanted {
        *LAST_DEMAND.lock().unwrap() = Some(Instant::now());
    }
    wanted
}

/// True when the vsync thread may stop following vsync and park.
pub(crate) fn idle() -> bool {
    if continuous() {
        return false;
    }
    match *LAST_DEMAND.lock().unwrap() {
        Some(t) => t.elapsed() > Duration::from_millis(1000) && !WANTED.load(Ordering::Acquire),
        None => false,
    }
}

/// Parks until a frame is requested or `timeout` elapses.
pub(crate) fn park(timeout: Duration) {
    let mut woke = PARK.lock().unwrap();
    if WANTED.load(Ordering::Acquire) {
        *woke = false;
        return;
    }
    let (mut g, _) = PARK_CV.wait_timeout_while(woke, timeout, |w| !*w).unwrap();
    *g = false;
    woke = g;
    drop(woke);
}
