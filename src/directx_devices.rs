use anyhow::{Context, Result};
use gpui_util::ResultExt;
use itertools::Itertools;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
            D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Dxgi::{
            CreateDXGIFactory2, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_CREATE_FACTORY_DEBUG,
            DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory6,
        },
    },
};
use windows::core::Interface;

use crate::hybrid::{self, RendererBackend};
use crate::{DirectXGlobalElements, DirectXRenderPipelines};
use parking_lot::Mutex;
use std::sync::Arc;

pub(crate) fn try_to_recover_from_device_lost<T>(mut f: impl FnMut() -> Result<T>) -> Result<T> {
    (0..5)
        .map(|i| {
            if i > 0 {
                // Add a small delay before retrying
                std::thread::sleep(std::time::Duration::from_millis(100 + i * 10));
            }
            f()
        })
        .find_or_last(Result::is_ok)
        .unwrap()
        .context("DirectXRenderer failed to recover from lost device after multiple attempts")
}

/// Pipelines and global buffers built ahead of time (on the monitor thread) for the first
/// window that adopts a freshly created device.
pub(crate) struct Prewarmed {
    pub(crate) globals: DirectXGlobalElements,
    pub(crate) pipelines: DirectXRenderPipelines,
}

// SAFETY: D3D11 device objects are free-threaded; this is only ever moved between threads.
unsafe impl Send for Prewarmed {}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
    /// Whether this is the hardware device or WARP.
    pub(crate) backend: RendererBackend,
    /// Set on the copy sent to windows during a planned (non-device-lost) backend switch.
    pub(crate) graceful_switch: bool,
    pub(crate) prewarmed: Arc<Mutex<Option<Prewarmed>>>,
}

impl DirectXDevices {
    /// Creates devices for the backend the process is currently on (or, at startup, the one the
    /// renderer mode asks for). Falls back to WARP when no hardware adapter works.
    pub(crate) fn new() -> Result<Self> {
        let devices = Self::create(hybrid::desired_backend_for_new_devices())?;
        hybrid::set_current_backend(devices.backend);
        Ok(devices)
    }

    /// Like [`create`](Self::create), plus builds the per-device pipelines up front.
    pub(crate) fn create_prewarmed(backend: RendererBackend) -> Result<Self> {
        let devices = Self::create(backend)?;
        // Only use the pre-built pieces if the backend we got is the one requested.
        if devices.backend == backend {
            let globals = DirectXGlobalElements::new(&devices.device)
                .context("Prewarming global elements")?;
            let pipelines = DirectXRenderPipelines::new(&devices.device)
                .context("Prewarming render pipelines")?;
            *devices.prewarmed.lock() = Some(Prewarmed { globals, pipelines });
        }
        Ok(devices)
    }

    pub(crate) fn create(backend: RendererBackend) -> Result<Self> {
        let debug_layer_available = check_debug_layer_available();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;

        let mut created = None;
        if backend == RendererBackend::Gpu {
            match get_hardware_adapter(&dxgi_factory, debug_layer_available) {
                Ok(c) => created = Some((RendererBackend::Gpu, c)),
                Err(err) => {
                    log::warn!("No usable hardware adapter ({err:#}); falling back to WARP");
                    hybrid::set_hardware_available(false);
                }
            }
        }
        let (backend, (adapter, device, device_context, feature_level)) = match created {
            Some(c) => c,
            None => (
                RendererBackend::Cpu,
                get_warp_adapter(&dxgi_factory, debug_layer_available)
                    .context("Creating WARP device")?,
            ),
        };
        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created {} device with Direct3D 11.1 feature level.", backend.label())
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created {} device with Direct3D 11.0 feature level.", backend.label())
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created {} device with Direct3D 10.1 feature level.", backend.label())
            }
            other => log::info!("Created {} device with feature level {:?}.", backend.label(), other),
        }

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
            backend,
            graceful_switch: false,
            prewarmed: Arc::new(Mutex::new(None)),
        })
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

type CreatedDevice = (
    IDXGIAdapter1,
    ID3D11Device,
    ID3D11DeviceContext,
    D3D_FEATURE_LEVEL,
);

/// First hardware (non-software) adapter that can create a usable D3D11 device.
fn get_hardware_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<CreatedDevice> {
    for adapter_index in 0.. {
        let Ok(adapter) = (unsafe { dxgi_factory.EnumAdapters(adapter_index) }) else {
            break;
        };
        let adapter: IDXGIAdapter1 = adapter.cast()?;
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            if desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0 {
                log::info!("Skipping software adapter: {}", gpu_name);
                continue;
            }
            log::info!("Using GPU: {}", gpu_name);
        }
        // Check to see whether the adapter supports Direct3D 11 and create
        // the device if it does.
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        if let Some(device) = get_device(
            &adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        )
        .log_err()
        {
            return Ok((adapter, device, context.unwrap(), feature_level));
        }
    }

    Err(anyhow::anyhow!("no hardware adapter supports Direct3D 11"))
}

/// Creates the device on Microsoft's WARP software rasterizer.
///
/// Prefers the explicit WARP adapter from DXGI; if that fails, uses
/// `D3D11CreateDevice(None, D3D_DRIVER_TYPE_WARP, ..)` and asks the device for its adapter.
fn get_warp_adapter(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<CreatedDevice> {
    if let Ok(adapter) = unsafe { dxgi_factory.EnumWarpAdapter::<IDXGIAdapter1>() } {
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        match get_device(
            &adapter,
            Some(&mut context),
            Some(&mut feature_level),
            debug_layer_available,
        ) {
            Ok(device) => {
                log::info!("Using WARP software rasterizer (via DXGI WARP adapter)");
                return Ok((adapter, device, context.unwrap(), feature_level));
            }
            Err(err) => log::warn!("WARP adapter device creation failed: {err:#}"),
        }
    }

    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )
        .context("D3D11CreateDevice(WARP)")?;
    }
    let device = device.context("WARP device missing")?;
    let adapter: IDXGIAdapter1 = unsafe { device.cast::<IDXGIDevice>()?.GetAdapter()?.cast()? };
    log::info!("Using WARP software rasterizer (via D3D_DRIVER_TYPE_WARP)");
    Ok((adapter, device, context.context("WARP context missing")?, feature_level))
}

#[inline]
fn get_device(
    adapter: &IDXGIAdapter1,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )?;
    }
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")?;
    }
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}
