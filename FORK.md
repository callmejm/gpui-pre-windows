# gpui-pre-windows fork (rustyfiles hybrid renderer)

Copy of crates.io `gpui-pre-windows` 0.3.11 (zed@f816e1f, crate `gpui_windows`); rustyfiles pulls it in
with `[patch.crates-io]` pointing at this repository. Registry `Cargo.lock` / `.cargo-ok` were dropped.
Goal of the fork: **the UI keeps drawing when the GPU or the CPU is saturated** by hot-swapping the whole
window between a hardware D3D11 device ("GPU") and Direct3D 11 WARP ("CPU", Microsoft's software
rasterizer running the same HLSL shaders).

## Changes vs upstream

| File | Change |
|---|---|
| `src/hybrid.rs` (new, `pub mod hybrid`) | Public API, load monitor, switching policy, hand-off slot for pre-warmed devices. |
| `src/directx_devices.rs` | `DirectXDevices` gains `backend`, `graceful_switch`, `prewarmed`. `create(backend)`; WARP creation (`EnumWarpAdapter`, fallback `D3D11CreateDevice(None, D3D_DRIVER_TYPE_WARP)` + `IDXGIDevice::GetAdapter`); hardware enumeration skips software adapters; **no more `unreachable!()`**: if no hardware adapter works, WARP is used. |
| `src/directx_renderer.rs` | `begin_switch()` / `finish_switch()` (hot swap), `commit_pending`, frame timing hook in `draw()`, `wait_for_gpu()`, device-independent DirectComposition device, pipelines/globals made `pub(crate)` so they can be prebuilt. |
| `src/events.rs` | `WM_GPUI_GPU_DEVICE_LOST` with `graceful_switch` runs `begin_switch` instead of the destructive device-lost path. |
| `src/platform.rs` | Vsync thread picks up a pre-warmed device and runs `switch_backend()`; monitor started in `attach_gpu()`. |
| `src/events.rs`, `src/window.rs` (closing) | The OLE drop target is revoked in `WM_DESTROY`, while the HWND is still valid. Upstream revokes it in `WindowsWindow::drop`, after a user-closed window is already destroyed: `RevokeDragDrop` failed, the drop target kept its `Rc<WindowsWindowInner>` and every closed window leaked its state and GPU buffers (~28 MB, rustyfiles issue #3). `drop` only hides/destroys a window that still exists. Live windows are counted (`windows alive`, debug log). |

## Public API (`gpui_windows::hybrid`)

```rust
hybrid::current_backend() -> Option<RendererBackend>   // Gpu | Cpu
hybrid::current_backend_label() -> &'static str         // "GPU" | "CPU" | "?"  (status bar)
hybrid::mode() / hybrid::set_mode(RendererMode)         // Auto | Gpu | Cpu
hybrid::switch_count() -> u32
hybrid::load_snapshot() -> LoadSnapshot { gpu_percent, cpu_percent, last_frame_ms }
```

Env vars (read once at startup):
* `RUSTYFILES_RENDERER=auto|gpu|cpu` (default `auto`)
* `RUSTYFILES_FRAME_LOG=1|<file>` debug: frame-time histogram log every 2 s (frame time includes GPU completion); file = raw samples.
* `RUSTYFILES_RENDERER_FLIP_SECS=N` debug: while in `auto`, force a switch every N seconds.
* `RUST_LOG=gpui_windows=debug` prints the monitor's load line every 5 s.

`set_mode(Gpu|Cpu)` triggers a hot swap within ~500 ms. The app needs a direct dependency on
`gpui-pre-windows` to call these (rf-app already has it).

## How a switch works (v1: whole window)

1. **Decide** (monitor thread `RendererMonitor`, tick 500 ms): see policy below.
2. **Pre-warm** (still on the monitor thread, old device keeps drawing): create the other
   `DirectXDevices` (hardware ~220 ms, WARP ~150 ms in a debug build) *and* its
   `DirectXRenderPipelines` + `DirectXGlobalElements` (in debug builds this includes the runtime HLSL compile).
3. **Hand-off**: the device goes into a slot that the vsync thread polls every vsync.
4. **Build on the UI thread** (`begin_switch`, ~1 ms with prewarmed pipelines): new swap chain
   (`CreateSwapChainForComposition`), render-target/path textures, atlas reset onto the new device. All fallible
   work happens *before* any state is replaced, so a failure leaves the old backend untouched (window message
   result `1` -> `switch_backend` returns `None`, monitor retries later, with the dwell cooldown).
   The DirectComposition visual still points at the **old swap chain**, so the last frame stays on screen.
5. **Render the first frame on the new device** (platform sends `WM_GPUI_FORCE_UPDATE_WINDOW`; GPUI re-rasterizes
   glyphs/images into the fresh atlas). Present on the new, not-yet-visible swap chain.
6. **Commit** (`finish_switch`): flush + wait on a `D3D11_QUERY_EVENT` (<= 250 ms) so the frame is complete, then
   `IDCompositionVisual::SetContent(new swap chain)` + `Commit()`: DWM swaps atomically. The old swap chain,
   device and context are released afterwards (the DComp device is device-independent, so it does not pin the old
   D3D device). If the commit fails it is retried on the next frame.
7. The text system (DirectWrite color glyph rasterizer) and platform-level device state are moved to the new device
   (same calls as the device-lost path), without its 350 + 200 ms sleeps.

If DirectComposition is disabled (`GPUI_DISABLE_DIRECT_COMPOSITION`), the HWND-bound swap chain cannot coexist
with a second one; `begin_switch` then falls back to the upstream destructive device-lost path (may flicker).

## Policy (`hybrid::Policy`, tuned in `PolicyConfig`)

Samples every 500 ms. GPU = busiest `\GPU Engine(*engtype_3D)\Utilization Percentage` engine (instances of the
same engine summed, capped at 100, own process excluded, max across engines/adapters). CPU = `GetSystemTimes` delta.
Frame time = draw + `Present` of the renderer's own frames (the first frame after a switch is excluded).

* GPU -> WARP when GPU >= 90 % held >= 1.5 s **and** CPU <= 70 %; or >= 3 consecutive frames > 50 ms (seen in the
  last 2 s) while CPU <= 85 %.
* WARP -> GPU when GPU <= 60 % held >= 3 s; or CPU >= 90 % held >= 2 s while GPU < 80 %; or >= 3 slow frames on WARP
  while GPU < 80 %.
* Never switch more often than every 5 s (`min_dwell`); hysteresis windows reset on every switch.
* If PDH is unavailable only the frame-time rules apply. If no hardware device exists, the app stays on WARP.
* The unit tests in `hybrid.rs` (`cargo test -p gpui-pre-windows --lib hybrid`) cover the policy.

Thresholds were checked under real GPU load, see `docs/HYBRID-RENDER-TEST.md` (unchanged).

## Known gaps

* Whole-window only (per-region layers come later). The seams for that: `DirectXRenderer` already owns its
  devices/resources/DComp visual separately from the shared `DirectXDevices`.
* Pre-warm covers device + pipelines but not the swap chain/atlas, which are per window and built on the UI thread
  (~1 ms). The glyph atlas is rebuilt by the forced redraw: that frame costs one full re-rasterization (54 ms on
  GPU->WARP, 20 ms on WARP->GPU in a debug build, measured around the whole message round trip).
* Multi-window: each window switches in the same vsync-thread pass, but a failure in one window leaves windows
  inconsistent (all stay drawable; they just run on different backends until the next switch). Single window tested.
* Not verified under real GPU/CPU saturation (no game was run unattended); verified with the forced-flip switch.
  The "no flicker" claim is checked with `PrintWindow` captures (400 captures across 3 switches, identical pixel
  statistics), not with a high-speed camera/DWM frame trace.
* WARP uses CPU: at 4K / high refresh it may be slow; the slow-frame rule moves back to hardware when possible.
* Device-lost recovery keeps the current backend (`desired_backend_for_new_devices`); a hardware device that can't be
  recreated falls back to WARP via `create()`.


## Drop effect hint (`src/dnd_hint.rs`)
Upstream reports `DROPEFFECT_COPY` for every drag over the window. The fork lets the app choose the
effect shown while hovering (`gpui_windows::dnd_hint::set(MOVE|COPY|LINK|NONE)`), with Ctrl/Shift/Alt
overriding like Explorer. `Drop` still returns COPY so a source never deletes originals; the app
performs the transfer itself.

## Demand-driven frames (`src/demand.rs`)
Upstream invalidates every window on every vsync, waking the UI thread 60–240×/s even when idle.
The fork implements GPUI core's `PlatformWindow::frame_waker` / `schedule_frame` for Windows:
the vsync thread only invalidates when GPUI signals a dirty window / animation frame, and parks
(250 ms ticks for device-lost + hybrid switch servicing) after 1 s of no demand.
Measured (debug build, idle window): 203 ms CPU per 10 s → 31 ms per 10 s.
`RUSTYFILES_CONTINUOUS_FRAMES=1` restores upstream behaviour for comparison.

## Fast start (hybrid.rs) — off by default since 2026-11-09
Opt-in with `RUSTYFILES_FAST_START=1`. It made the window appear sooner but WARP drew the first
frames on every core until the hot-swap: ~4 s of CPU time at launch against ~0.8 s when Auto opens
straight on the GPU (rustyfiles' speed test showed it as a CPU column worse than Explorer's).
Auto now opens on the hardware adapter when one works and falls back to WARP when none does.

When enabled: in Auto mode the first device is WARP (ready ~190 ms sooner than the NVIDIA device). The monitor's
first tick hot-swaps to the GPU if it is not busy (pre-warmed off-thread, ~13 ms on the message
loop); if the GPU is saturated at launch (game, render) the app stays on the CPU and never touches
it. The old device is `ClearState`/`Flush`/`Trim`med on every switch so WARP heaps are returned
(private bytes after the startup switch: 166 MB → 106 MB).
Measured (release): window visible 465 ms → ~300 ms.

## GPU latency probe (hybrid policy input)
`Present` is asynchronous, so draw+present time cannot see a saturated GPU. Each frame now issues
a `D3D11_QUERY_EVENT` right before `Present` (which flushes it) and later draws poll it with
`D3D11_ASYNC_GETDATA_DONOTFLUSH`, never waiting. The frame time fed to the policy is
`max(draw time, how long the previous probed frame has been unfinished)`. One probe in flight at
a time; reset on device switch / device lost. Measured: idle 0 slow frames; under 100 ms GPU
packets 34 of ~80 interactive frames flagged slow (53–65 ms), so the "3 consecutive frames > 50 ms"
rule is now a real signal. Slow frames are logged at debug level (`gpui_windows::hybrid`).

## Acrylic popups (2026-11-05)
- `WindowBackgroundAppearance::Blurred` → Windows 11 system Acrylic (`DWMSBT_TRANSIENTWINDOW`) with `DWMWCP_ROUNDSMALL` corners on build ≥ 22621; legacy blur otherwise (window.rs `dwm_acrylic`).
- `KEEP_ACTIVE_LOOK` (exported): while set, `WM_NCACTIVATE(FALSE)` is answered as active, so the owner keeps its active frame / Mica while an app popup menu has focus (events.rs).
