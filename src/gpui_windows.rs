#![cfg(target_os = "windows")]

mod clipboard;
mod demand;
mod destination_list;
mod dialog;
mod direct_manipulation;
mod frame_log;
mod direct_write;
mod directx_atlas;
mod directx_devices;
mod directx_renderer;
mod dispatcher;
mod display;
mod events;
pub mod dnd_hint;
pub mod hybrid;
mod keyboard;
mod platform;
mod system_notifications;
mod system_settings;
mod util;
mod vsync;
mod window;
mod wrapper;

pub(crate) use clipboard::*;
pub(crate) use destination_list::*;
pub(crate) use direct_write::*;
pub(crate) use directx_atlas::*;
pub(crate) use directx_devices::*;
pub(crate) use directx_renderer::*;
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use events::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use system_notifications::*;
pub(crate) use system_settings::*;
pub(crate) use util::*;
pub(crate) use vsync::*;
pub(crate) use window::*;
pub(crate) use wrapper::*;

pub use platform::WindowsPlatform;
/// rustyfiles: keep owner windows looking active while an app popup menu has focus.
pub use window::KEEP_ACTIVE_LOOK;
/// rustyfiles: smooth wheel scrolling on/off.
pub use events::SMOOTH_SCROLL;

pub(crate) use windows::Win32::Foundation::HWND;
