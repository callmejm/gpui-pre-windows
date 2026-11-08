//! rustyfiles fork: an optional frame-time log for benchmarks. With `GPUI_FRAME_LOG=<path>` every
//! drawn frame appends one line, `t_ms<TAB>draw_ms`: the time since the first frame and the time
//! spent building and submitting that frame on the UI thread. The gaps between `t_ms` values
//! show stutter; `draw_ms` shows what each frame cost. Off (no cost) without the variable.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static LOG: OnceLock<Option<Mutex<(BufWriter<File>, Instant)>>> = OnceLock::new();

fn log() -> &'static Option<Mutex<(BufWriter<File>, Instant)>> {
    LOG.get_or_init(|| {
        let path = std::env::var_os("GPUI_FRAME_LOG")?;
        let f = File::create(path).ok()?;
        Some(Mutex::new((BufWriter::new(f), Instant::now())))
    })
}

pub(crate) fn enabled() -> bool {
    log().is_some()
}

pub(crate) fn record(start: Instant, took: Duration) {
    if let Some(m) = log() {
        let mut g = m.lock().unwrap();
        let t = start.duration_since(g.1).as_secs_f64() * 1000.0;
        let _ = writeln!(g.0, "{t:.3}\t{:.3}", took.as_secs_f64() * 1000.0);
        // the process may be killed when the benchmark ends: keep the file current
        let _ = g.0.flush();
    }
}
