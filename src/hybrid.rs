//! Hybrid GPU/CPU rendering control (rustyfiles fork).
//!
//! The renderer can run on a hardware Direct3D 11 device ("GPU") or on Microsoft's WARP software
//! rasterizer ("CPU"). This module owns:
//!
//! * the public, process-wide state (current backend, requested [`RendererMode`], counters),
//! * a background *load monitor* (system GPU 3D-engine utilization via PDH, CPU via
//!   `GetSystemTimes`, plus the renderer's own frame times),
//! * the hysteresis [`Policy`] that decides when to switch,
//! * a hand-off slot through which a *pre-warmed* [`DirectXDevices`] reaches the vsync thread,
//!   which performs the actual window-level swap (see `platform.rs` / `directx_renderer.rs`).
//!
//! Configuration (read once, at first device creation):
//! * `RUSTYFILES_RENDERER=auto|gpu|cpu` (default `auto`)
//! * `RUSTYFILES_FRAME_LOG=1` (debug: log a frame-time histogram line every 2 s; a file path instead of 1 also appends raw samples there)
//! * `RUSTYFILES_RENDERER_FLIP_SECS=N` (debug: force a switch every N seconds while in `auto`)

use std::sync::{
    Once,
    atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::{
    Win32::{
        Foundation::FILETIME,
        System::{
            Performance::{
                PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
                PDH_MORE_DATA, PdhAddEnglishCounterW, PdhCollectQueryData, PdhCloseQuery,
                PdhGetFormattedCounterArrayW, PdhOpenQueryW,
            },
            Threading::GetSystemTimes,
        },
    },
    core::{PCWSTR, w},
};

use crate::DirectXDevices;

/// Which device the UI is currently drawn with.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RendererBackend {
    /// Hardware Direct3D 11 device.
    Gpu = 1,
    /// Direct3D 11 WARP (software rasterizer on the CPU).
    Cpu = 2,
}

impl RendererBackend {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Gpu),
            2 => Some(Self::Cpu),
            _ => None,
        }
    }

    /// Short label suitable for a status bar.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gpu => "GPU",
            Self::Cpu => "CPU",
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Gpu => Self::Cpu,
            Self::Cpu => Self::Gpu,
        }
    }
}

/// User-facing renderer selection.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RendererMode {
    /// Switch automatically based on load.
    Auto = 0,
    /// Always hardware.
    Gpu = 1,
    /// Always WARP.
    Cpu = 2,
}

impl RendererMode {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Gpu,
            2 => Self::Cpu,
            _ => Self::Auto,
        }
    }

    /// Parses `auto` / `gpu` / `cpu` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "gpu" | "hardware" | "hw" => Some(Self::Gpu),
            "cpu" | "warp" | "software" | "sw" => Some(Self::Cpu),
            _ => None,
        }
    }
}

static CURRENT_BACKEND: AtomicU8 = AtomicU8::new(0);
static MODE: AtomicU8 = AtomicU8::new(RendererMode::Auto as u8);
static HW_AVAILABLE: AtomicBool = AtomicBool::new(true);
static SWITCH_COUNT: AtomicU32 = AtomicU32::new(0);
static SWITCH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
// Load snapshot, stored as percent * 10 (u32::MAX = unknown).
static GPU_X10: AtomicU32 = AtomicU32::new(u32::MAX);
static CPU_X10: AtomicU32 = AtomicU32::new(u32::MAX);
// Frame timing.
static LAST_FRAME_US: AtomicU64 = AtomicU64::new(0);
static SLOW_FRAMES: AtomicU32 = AtomicU32::new(0);
static LAST_SLOW_AT_MS: AtomicU64 = AtomicU64::new(0);
static ENV_INIT: Once = Once::new();
static MONITOR_INIT: Once = Once::new();

static RAW_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
static FRAME_LOG: AtomicBool = AtomicBool::new(false);
static FRAME_SAMPLES: Mutex<Vec<u32>> = Mutex::new(Vec::new());

pub(crate) fn frame_log_enabled() -> bool {
    FRAME_LOG.load(Ordering::Relaxed)
}

/// Logs p50/p95/p99/max of the frames drawn since the previous call (`RUSTYFILES_FRAME_LOG=1`).
fn log_frame_stats(backend: RendererBackend) {
    let mut v = std::mem::take(&mut *FRAME_SAMPLES.lock());
    if v.is_empty() {
        return;
    }
    if let Some(path) = RAW_PATH.get() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let mut out = String::new();
            for us in &v {
                out.push_str(&format!("{},{us}
", backend.label()));
            }
            let _ = f.write_all(out.as_bytes());
        }
    }
    v.sort_unstable();
    let pct = |p: f64| v[(((v.len() - 1) as f64) * p).round() as usize] as f32 / 1000.0;
    log::info!(
        "frame-stats backend={} n={} p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms",
        backend.label(),
        v.len(),
        pct(0.5),
        pct(0.95),
        pct(0.99),
        pct(1.0)
    );
}

static PENDING: Mutex<Option<SendDevices>> = Mutex::new(None);

struct SendDevices(DirectXDevices);
// SAFETY: D3D11 devices are free-threaded; the hand-off is a plain move between two threads.
unsafe impl Send for SendDevices {}

/// The backend the window is currently drawn with (`None` before the first device exists).
pub fn current_backend() -> Option<RendererBackend> {
    RendererBackend::from_u8(CURRENT_BACKEND.load(Ordering::Relaxed))
}

/// `"GPU"`, `"CPU"` or `"?"`, for a status bar.
pub fn current_backend_label() -> &'static str {
    current_backend().map_or("?", RendererBackend::label)
}

/// The requested renderer mode.
pub fn mode() -> RendererMode {
    init_from_env();
    RendererMode::from_u8(MODE.load(Ordering::Relaxed))
}

/// Changes the renderer mode at runtime. `Gpu`/`Cpu` take effect on the monitor's next tick
/// (within ~500 ms) via a hot swap; `Auto` hands control back to the load policy.
pub fn set_mode(mode: RendererMode) {
    init_from_env();
    let old = MODE.swap(mode as u8, Ordering::Relaxed);
    if old != mode as u8 {
        log::info!("renderer mode set to {mode:?}");
    }
}

/// Number of completed backend switches since start.
pub fn switch_count() -> u32 {
    SWITCH_COUNT.load(Ordering::Relaxed)
}

/// Latest measurements of the load monitor.
#[derive(Copy, Clone, Debug)]
pub struct LoadSnapshot {
    /// System GPU 3D engine utilization in percent (busiest engine, own process excluded).
    pub gpu_percent: Option<f32>,
    /// System CPU utilization in percent.
    pub cpu_percent: Option<f32>,
    /// Duration of the most recent frame (draw + present) in milliseconds.
    pub last_frame_ms: f32,
}

pub fn load_snapshot() -> LoadSnapshot {
    let f = |a: &AtomicU32| {
        let v = a.load(Ordering::Relaxed);
        (v != u32::MAX).then(|| v as f32 / 10.0)
    };
    LoadSnapshot {
        gpu_percent: f(&GPU_X10),
        cpu_percent: f(&CPU_X10),
        last_frame_ms: LAST_FRAME_US.load(Ordering::Relaxed) as f32 / 1000.0,
    }
}

// ---------------------------------------------------------------------------------------------
// crate-internal hooks
// ---------------------------------------------------------------------------------------------

pub(crate) fn init_from_env() {
    ENV_INIT.call_once(|| {
        if let Ok(v) = std::env::var("RUSTYFILES_RENDERER") {
            match RendererMode::parse(&v) {
                Some(m) => {
                    MODE.store(m as u8, Ordering::Relaxed);
                    log::info!("RUSTYFILES_RENDERER={v} -> mode {m:?}");
                }
                None => log::warn!("RUSTYFILES_RENDERER={v:?} is not auto|gpu|cpu; using auto"),
            }
        }
    });
}

/// Which backend a fresh `DirectXDevices::new()` should create.
pub(crate) fn desired_backend_for_new_devices() -> RendererBackend {
    init_from_env();
    if let Some(current) = current_backend() {
        // Device-lost recovery: keep the backend we were on.
        return current;
    }
    match RendererMode::from_u8(MODE.load(Ordering::Relaxed)) {
        RendererMode::Cpu => RendererBackend::Cpu,
        RendererMode::Gpu => RendererBackend::Gpu,
        RendererMode::Auto if fast_start_enabled() => {
            // Fast start: WARP is ready ~190 ms sooner than a discrete GPU device. Open on the
            // CPU, then hot-swap to the GPU (pre-warmed off-thread) once the monitor sees it is
            // not busy. If the GPU is saturated at launch (game/render), we never touch it.
            FAST_START.store(true, Ordering::Relaxed);
            HW_AVAILABLE.store(true, Ordering::Relaxed);
            RendererBackend::Cpu
        }
        RendererMode::Auto => RendererBackend::Gpu,
    }
}

static FAST_START: AtomicBool = AtomicBool::new(false);

fn fast_start_enabled() -> bool {
    std::env::var("RUSTYFILES_FAST_START").map_or(true, |v| v.trim() != "0")
}

pub(crate) fn set_current_backend(backend: RendererBackend) {
    CURRENT_BACKEND.store(backend as u8, Ordering::Relaxed);
}

pub(crate) fn set_hardware_available(available: bool) {
    HW_AVAILABLE.store(available, Ordering::Relaxed);
}

/// Called by the renderer after every completed draw+present.
pub(crate) fn record_frame(elapsed: Duration) {
    LAST_FRAME_US.store(elapsed.as_micros() as u64, Ordering::Relaxed);
    if FRAME_LOG.load(Ordering::Relaxed) {
        FRAME_SAMPLES.lock().push(elapsed.as_micros() as u32);
    }
    if elapsed > Policy::SLOW_FRAME {
        log::debug!("slow frame: {elapsed:?} (draw or GPU lag)");
        SLOW_FRAMES.fetch_add(1, Ordering::Relaxed);
        LAST_SLOW_AT_MS.store(now_ms(), Ordering::Relaxed);
    } else {
        SLOW_FRAMES.store(0, Ordering::Relaxed);
    }
}

fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Takes the pre-warmed devices the monitor prepared, if any (vsync thread).
pub(crate) fn take_pending_devices() -> Option<DirectXDevices> {
    PENDING.lock().take().map(|d| d.0)
}

/// The vsync thread reports the outcome of a swap.
pub(crate) fn finish_switch(result: Option<RendererBackend>) {
    if let Some(backend) = result {
        set_current_backend(backend);
        SWITCH_COUNT.fetch_add(1, Ordering::Relaxed);
        SLOW_FRAMES.store(0, Ordering::Relaxed);
    }
    SWITCH_IN_FLIGHT.store(false, Ordering::Release);
}

/// Starts the load monitor thread (idempotent).
pub(crate) fn start_monitor() {
    MONITOR_INIT.call_once(|| {
        init_from_env();
        std::thread::Builder::new()
            .name("RendererMonitor".to_owned())
            .spawn(monitor_main)
            .map_err(|e| log::error!("failed to start renderer monitor: {e}"))
            .ok();
    });
}

// ---------------------------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------------------------

/// Tunables for the automatic switching policy. See `FORK.md` for the rationale.
#[derive(Copy, Clone, Debug)]
pub(crate) struct PolicyConfig {
    /// GPU utilization at/above which the GPU counts as saturated.
    pub gpu_busy: f32,
    /// ...for at least this long.
    pub gpu_busy_for: Duration,
    /// Only move to WARP when the CPU is at/below this.
    pub cpu_ok_for_warp: f32,
    /// GPU utilization at/below which the GPU counts as free again.
    pub gpu_free: f32,
    /// ...for at least this long (switch back to hardware).
    pub gpu_free_for: Duration,
    /// CPU utilization at/above which the CPU counts as saturated.
    pub cpu_busy: f32,
    /// ...for at least this long (switch back to hardware, if the GPU is not saturated).
    pub cpu_busy_for: Duration,
    /// Frame-time trigger: this many consecutive slow frames.
    pub slow_frames: u32,
    /// Never switch more often than this.
    pub min_dwell: Duration,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            gpu_busy: 90.0,
            gpu_busy_for: Duration::from_millis(1500),
            cpu_ok_for_warp: 70.0,
            gpu_free: 60.0,
            gpu_free_for: Duration::from_secs(3),
            cpu_busy: 90.0,
            cpu_busy_for: Duration::from_secs(2),
            slow_frames: 3,
            min_dwell: Duration::from_secs(5),
        }
    }
}

pub(crate) struct Policy {
    cfg: PolicyConfig,
    last_switch: Instant,
    gpu_high_since: Option<Instant>,
    gpu_low_since: Option<Instant>,
    cpu_high_since: Option<Instant>,
}

impl Policy {
    /// A frame (draw + present) slower than this counts as "slow".
    pub(crate) const SLOW_FRAME: Duration = Duration::from_millis(50);

    pub(crate) fn new(cfg: PolicyConfig, now: Instant) -> Self {
        Self {
            cfg,
            last_switch: now,
            gpu_high_since: None,
            gpu_low_since: None,
            cpu_high_since: None,
        }
    }

    /// Records that a switch was started (for dwell time and timer reset).
    pub(crate) fn note_switch(&mut self, now: Instant) {
        self.last_switch = now;
        self.gpu_high_since = None;
        self.gpu_low_since = None;
        self.cpu_high_since = None;
    }

    /// Evaluates one sample. `gpu`/`cpu` are percentages (`None` = unknown), `slow_frames` the
    /// number of consecutive recent slow frames. Returns the backend to switch to, if any.
    pub(crate) fn evaluate(
        &mut self,
        now: Instant,
        current: RendererBackend,
        gpu: Option<f32>,
        cpu: Option<f32>,
        slow_frames: u32,
    ) -> Option<RendererBackend> {
        let track = |since: &mut Option<Instant>, cond: bool| {
            if cond {
                since.get_or_insert(now);
            } else {
                *since = None;
            }
        };
        track(&mut self.gpu_high_since, gpu.is_some_and(|g| g >= self.cfg.gpu_busy));
        track(&mut self.gpu_low_since, gpu.is_some_and(|g| g <= self.cfg.gpu_free));
        track(&mut self.cpu_high_since, cpu.is_some_and(|c| c >= self.cfg.cpu_busy));

        if now.duration_since(self.last_switch) < self.cfg.min_dwell {
            return None;
        }
        let held = |since: Option<Instant>, d: Duration| since.is_some_and(|s| now.duration_since(s) >= d);
        match current {
            RendererBackend::Gpu => {
                let cpu_has_headroom = cpu.is_none_or(|c| c <= self.cfg.cpu_ok_for_warp);
                if held(self.gpu_high_since, self.cfg.gpu_busy_for) && cpu_has_headroom {
                    return Some(RendererBackend::Cpu);
                }
                // Frames stall even though the GPU counter looks calm (or is unavailable).
                // Require a little more CPU headroom than the GPU rule: WARP burns CPU.
                let cpu_ok_for_slow = cpu.is_none_or(|c| c <= 85.0);
                if slow_frames >= self.cfg.slow_frames && cpu_ok_for_slow {
                    return Some(RendererBackend::Cpu);
                }
            }
            RendererBackend::Cpu => {
                if held(self.gpu_low_since, self.cfg.gpu_free_for) {
                    return Some(RendererBackend::Gpu);
                }
                let gpu_not_saturated = gpu.is_none_or(|g| g < self.cfg.gpu_busy - 10.0);
                if held(self.cpu_high_since, self.cfg.cpu_busy_for) && gpu_not_saturated {
                    return Some(RendererBackend::Gpu);
                }
                if slow_frames >= self.cfg.slow_frames && gpu_not_saturated {
                    return Some(RendererBackend::Gpu);
                }
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Monitor
// ---------------------------------------------------------------------------------------------

const TICK: Duration = Duration::from_millis(500);

fn monitor_main() {
    let mut gpu_counter = GpuCounter::open();
    if gpu_counter.is_none() {
        log::warn!("GPU utilization counter unavailable; auto mode relies on frame times only");
    }
    let mut cpu = CpuSampler::new();
    let flip_every = std::env::var("RUSTYFILES_RENDERER_FLIP_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .map(Duration::from_secs_f64);
    if let Some(d) = flip_every {
        log::info!("RUSTYFILES_RENDERER_FLIP_SECS: forcing a backend switch every {d:?} (auto mode)");
    }
    let frame_log = std::env::var("RUSTYFILES_FRAME_LOG").is_ok_and(|v| v.trim() != "0" && !v.trim().is_empty());
    FRAME_LOG.store(frame_log, Ordering::Relaxed);
    if let Ok(v) = std::env::var("RUSTYFILES_FRAME_LOG") {
        // Any value other than 1 is a file the raw per-frame samples ("GPU,1234" = microseconds) are appended to.
        if frame_log && v.trim() != "1" {
            let _ = RAW_PATH.set(v.trim().to_owned());
        }
    }
    let mut last_frame_log = Instant::now();
    let mut policy = Policy::new(PolicyConfig::default(), Instant::now());
    let mut last_flip = Instant::now();
    let mut last_log = Instant::now();

    loop {
        std::thread::sleep(TICK);
        let now = Instant::now();

        let gpu = gpu_counter.as_mut().and_then(GpuCounter::sample);
        let cpu_pct = cpu.sample();
        GPU_X10.store(gpu.map_or(u32::MAX, |g| (g * 10.0) as u32), Ordering::Relaxed);
        CPU_X10.store(cpu_pct.map_or(u32::MAX, |c| (c * 10.0) as u32), Ordering::Relaxed);

        let Some(current) = current_backend() else { continue };
        if frame_log && now.duration_since(last_frame_log) >= Duration::from_secs(2) {
            last_frame_log = now;
            log_frame_stats(current);
            log::info!("load gpu={gpu:?} cpu={cpu_pct:?}");
        }
        if SWITCH_IN_FLIGHT.load(Ordering::Acquire) {
            continue;
        }

        let slow = {
            let recent = now_ms().saturating_sub(LAST_SLOW_AT_MS.load(Ordering::Relaxed)) < 2000;
            if recent { SLOW_FRAMES.load(Ordering::Relaxed) } else { 0 }
        };

        let hw_ok = HW_AVAILABLE.load(Ordering::Relaxed);
        let mode = RendererMode::from_u8(MODE.load(Ordering::Relaxed));
        let (target, why): (Option<RendererBackend>, String) = match mode {
            RendererMode::Gpu => ((current != RendererBackend::Gpu).then_some(RendererBackend::Gpu), "mode=gpu".into()),
            RendererMode::Cpu => ((current != RendererBackend::Cpu).then_some(RendererBackend::Cpu), "mode=cpu".into()),
            RendererMode::Auto => {
                if FAST_START.swap(false, Ordering::Relaxed) && current == RendererBackend::Cpu {
                    if gpu.is_none_or(|g| g < 85.0) {
                        (Some(RendererBackend::Gpu), "fast start: GPU idle, moving to hardware".into())
                    } else {
                        log::info!("fast start: GPU busy ({gpu:?}%), staying on CPU");
                        (None, String::new())
                    }
                } else if let Some(every) = flip_every {
                    if now.duration_since(last_flip) >= every {
                        (Some(current.other()), "debug flip".into())
                    } else {
                        (None, String::new())
                    }
                } else if !hw_ok {
                    (None, String::new())
                } else {
                    let t = policy.evaluate(now, current, gpu, cpu_pct, slow);
                    (
                        t,
                        format!(
                            "gpu={:?}% cpu={:?}% slow_frames={slow}",
                            gpu.map(|g| g.round()),
                            cpu_pct.map(|c| c.round())
                        ),
                    )
                }
            }
        };

        if now.duration_since(last_log) >= Duration::from_secs(5) {
            last_log = now;
            log::debug!(
                "renderer load: backend={} gpu={:?} cpu={:?} frame={:.1}ms",
                current.label(),
                gpu,
                cpu_pct,
                LAST_FRAME_US.load(Ordering::Relaxed) as f32 / 1000.0
            );
        }

        let Some(target) = target.filter(|t| *t != current) else { continue };
        if target == RendererBackend::Gpu && !hw_ok {
            continue;
        }

        log::info!("renderer switch requested: {} -> {} ({why})", current.label(), target.label());
        policy.note_switch(now);
        last_flip = now;

        // Pre-warm: create the other device (and its pipelines) here, off the UI/vsync threads,
        // while the old device keeps drawing.
        let started = Instant::now();
        match DirectXDevices::create_prewarmed(target) {
            Ok(devices) => {
                log::info!("pre-warmed {} device in {:?}", target.label(), started.elapsed());
                SWITCH_IN_FLIGHT.store(true, Ordering::Release);
                *PENDING.lock() = Some(SendDevices(devices));
            }
            Err(err) => {
                log::error!("could not create {} device: {err:#}", target.label());
                if target == RendererBackend::Gpu {
                    HW_AVAILABLE.store(false, Ordering::Relaxed);
                }
            }
        }
    }
}

struct CpuSampler {
    prev: Option<(u64, u64, u64)>,
}

impl CpuSampler {
    fn new() -> Self {
        Self { prev: Self::read() }
    }

    fn read() -> Option<(u64, u64, u64)> {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
        let q = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        Some((q(idle), q(kernel), q(user)))
    }

    fn sample(&mut self) -> Option<f32> {
        let cur = Self::read()?;
        let prev = self.prev.replace(cur)?;
        let idle = cur.0.wrapping_sub(prev.0);
        let total = cur.1.wrapping_sub(prev.1) + cur.2.wrapping_sub(prev.2); // kernel includes idle
        if total == 0 {
            return None;
        }
        Some((100.0 * (1.0 - idle as f64 / total as f64)).clamp(0.0, 100.0) as f32)
    }
}

/// `\GPU Engine(*engtype_3D)\Utilization Percentage` via PDH.
struct GpuCounter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    buf: Vec<u64>,
    own_pid: u32,
}

// SAFETY: PDH handles are plain opaque handles used from the monitor thread only.
unsafe impl Send for GpuCounter {}

impl GpuCounter {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            let path = w!("\\GPU Engine(*engtype_3D)\\Utilization Percentage");
            if PdhAddEnglishCounterW(query, path, 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            // The first collection only establishes the baseline.
            PdhCollectQueryData(query);
            Some(Self { query, counter, buf: Vec::new(), own_pid: std::process::id() })
        }
    }

    /// Utilization of the busiest 3D engine (instances of the same engine are summed and capped
    /// at 100; this process's own usage is excluded; the max over engines/adapters is returned).
    fn sample(&mut self) -> Option<f32> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let status = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if status != PDH_MORE_DATA || size == 0 {
                return None;
            }
            self.buf.resize((size as usize).div_ceil(8) + 1, 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            let status =
                PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items));
            if status != 0 {
                return None;
            }
            let items = std::slice::from_raw_parts(items, count as usize);
            let own_prefix = format!("pid_{}_", self.own_pid);
            let mut engines: Vec<(String, f64)> = Vec::new();
            for item in items {
                if item.FmtValue.CStatus != 0 {
                    continue;
                }
                let Ok(name) = item.szName.to_string() else { continue };
                if name.starts_with(&own_prefix) {
                    continue;
                }
                // "pid_123_luid_0x.._0x.._phys_0_eng_0_engtype_3D" -> engine key "luid_..._eng_0"
                let key = name.split_once("luid_").map_or(name.as_str(), |(_, r)| r);
                let key = key.split("_engtype_").next().unwrap_or(key).to_owned();
                let v = item.FmtValue.Anonymous.doubleValue;
                match engines.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, acc)) => *acc += v,
                    None => engines.push((key, v)),
                }
            }
            Some(engines.iter().map(|(_, v)| v.min(100.0)).fold(0.0f64, f64::max) as f32)
        }
    }
}

impl Drop for GpuCounter {
    fn drop(&mut self) {
        unsafe { PdhCloseQuery(self.query) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn switches_to_warp_after_sustained_gpu_load_only() {
        let base = Instant::now();
        let mut p = Policy::new(PolicyConfig::default(), base);
        // Within the dwell time nothing happens.
        assert_eq!(p.evaluate(t(base, 1.0), RendererBackend::Gpu, Some(10.0), Some(20.0), 0), None);
        // GPU busy starts at 6.0s, 1.5s later we switch.
        assert_eq!(p.evaluate(t(base, 6.0), RendererBackend::Gpu, Some(99.0), Some(20.0), 0), None);
        assert_eq!(p.evaluate(t(base, 7.0), RendererBackend::Gpu, Some(99.0), Some(20.0), 0), None);
        assert_eq!(
            p.evaluate(t(base, 7.6), RendererBackend::Gpu, Some(99.0), Some(20.0), 0),
            Some(RendererBackend::Cpu)
        );
    }

    #[test]
    fn stays_on_gpu_when_cpu_is_busy_too() {
        let base = Instant::now();
        let mut p = Policy::new(PolicyConfig::default(), base);
        for i in 0..40 {
            let r = p.evaluate(t(base, 6.0 + i as f64 * 0.5), RendererBackend::Gpu, Some(99.0), Some(95.0), 0);
            assert_eq!(r, None);
        }
    }

    #[test]
    fn switches_back_after_gpu_is_free_for_three_seconds() {
        let base = Instant::now();
        let mut p = Policy::new(PolicyConfig::default(), base);
        assert_eq!(p.evaluate(t(base, 6.0), RendererBackend::Cpu, Some(30.0), Some(40.0), 0), None);
        assert_eq!(p.evaluate(t(base, 8.9), RendererBackend::Cpu, Some(30.0), Some(40.0), 0), None);
        assert_eq!(
            p.evaluate(t(base, 9.1), RendererBackend::Cpu, Some(30.0), Some(40.0), 0),
            Some(RendererBackend::Gpu)
        );
    }

    #[test]
    fn slow_frames_trigger_and_dwell_prevents_flapping() {
        let base = Instant::now();
        let mut p = Policy::new(PolicyConfig::default(), base);
        assert_eq!(
            p.evaluate(t(base, 6.0), RendererBackend::Gpu, Some(10.0), Some(30.0), 3),
            Some(RendererBackend::Cpu)
        );
        p.note_switch(t(base, 6.0));
        // Immediately after a switch even strong signals are ignored for 5 s.
        assert_eq!(p.evaluate(t(base, 8.0), RendererBackend::Cpu, Some(0.0), Some(0.0), 0), None);
        assert_eq!(p.evaluate(t(base, 10.9), RendererBackend::Cpu, Some(0.0), Some(0.0), 0), None);
    }
}
