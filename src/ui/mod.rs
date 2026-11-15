//! Everything visual: the Slint components (compiled from `ui/*.slint` by build.rs),
//! the tray icon, the toast popup and the procedurally drawn app icon.

pub mod icon;
pub mod toast;
pub mod tray;

use crate::audio::IconData;
use icon::IconState;

/// Deterministic avatar colour for apps without an icon.
pub fn tint_for(key: &str) -> slint::Color {
    const PALETTE: [(u8, u8, u8); 8] = [
        (0x7b, 0x6b, 0xff),
        (0x2f, 0xa8, 0xe0),
        (0x24, 0xb3, 0x8a),
        (0xe0, 0x8a, 0x2f),
        (0xd9, 0x4f, 0x8f),
        (0x5f, 0x7d, 0xf0),
        (0xc0, 0x5c, 0xe0),
        (0x3f, 0xa0, 0x6a),
    ];
    let h = key.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let (r, g, b) = PALETTE[(h as usize) % PALETTE.len()];
    slint::Color::from_rgb_u8(r, g, b)
}

pub fn initial_for(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into())
}

pub fn image_from_icon(icon: &IconData) -> slint::Image {
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&icon.rgba, icon.width, icon.height);
    slint::Image::from_rgba8(buf)
}

pub fn icon_image(size: u32, state: IconState) -> slint::Image {
    let rgba = icon::render(size, state);
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&rgba, size, size);
    slint::Image::from_rgba8(buf)
}

/// The app icon as a winit window icon.
pub fn winit_icon(size: u32) -> Option<slint::winit_030::winit::window::Icon> {
    slint::winit_030::winit::window::Icon::from_rgba(icon::render(size, IconState::Normal), size, size).ok()
}

/// Window icons applied when the native window is *created*. Slint can't convert icons made
/// from pixel buffers itself, and the window doesn't exist yet when `show()` returns, so
/// this is the reliable place (title bar / Alt+Tab, plus the Windows taskbar's big icon).
pub fn with_app_icons(
    attrs: slint::winit_030::winit::window::WindowAttributes,
) -> slint::winit_030::winit::window::WindowAttributes {
    let attrs = attrs.with_window_icon(winit_icon(32));
    #[cfg(target_os = "windows")]
    let attrs = {
        use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
        attrs.with_taskbar_icon(winit_icon(64))
    };
    attrs
}
