//! The DisplayWire icon on every window: the stream windows (one per monitor), the list of PCs
//! and the settings window. 64×64 straight-alpha RGBA, made from `DisplayWire.icns`.

const SIZE: u32 = 64;
const RGBA: &[u8; (SIZE * SIZE * 4) as usize] = include_bytes!("../../assets/icon-64.rgba");

/// For a stream window: the title bar, Alt+Tab and the taskbar.
pub fn stream_window(attrs: winit::window::WindowAttributes) -> winit::window::WindowAttributes {
    use winit::platform::windows::WindowAttributesExtWindows;
    let icon = || winit::window::Icon::from_rgba(RGBA.to_vec(), SIZE, SIZE).ok();
    attrs.with_window_icon(icon()).with_taskbar_icon(icon())
}

/// For the egui windows (the list of PCs, settings).
pub fn egui() -> std::sync::Arc<eframe::egui::IconData> {
    std::sync::Arc::new(eframe::egui::IconData { rgba: RGBA.to_vec(), width: SIZE, height: SIZE })
}
