//! The PC's monitors as the client lists them: friendly name ("Dell AW3420DW", from the display
//! configuration), size, refresh rate and which is primary, in DXGI's order (`capture::outputs`).

use std::collections::HashMap;

use proto::stream::{MAX_MONITORS, Monitor, Monitors};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::core::PCWSTR;

use super::capture::{Output, outputs};

fn wide_str(w: &[u16]) -> String {
    String::from_utf16_lossy(w).trim_end_matches('\0').to_string()
}

/// GDI device name → the monitor's own name, for the monitors that report one.
fn friendly_names() -> HashMap<String, String> {
    let mut names = HashMap::new();
    let (mut n_paths, mut n_modes) = (0u32, 0u32);
    unsafe {
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, &mut n_modes).is_err() {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut n_paths, paths.as_mut_ptr(), &mut n_modes, modes.as_mut_ptr(), None).is_err() {
            return names;
        }
        for p in &paths[..n_paths as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = p.sourceInfo.adapterId;
            source.header.id = p.sourceInfo.id;
            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = p.targetInfo.adapterId;
            target.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) == 0 && DisplayConfigGetDeviceInfo(&mut target.header) == 0 {
                let name = wide_str(&target.monitorFriendlyDeviceName);
                if !name.is_empty() {
                    names.insert(wide_str(&source.viewGdiDeviceName), name);
                }
            }
        }
    }
    names
}

fn refresh_hz(device: &str) -> u16 {
    let name: Vec<u16> = device.encode_utf16().chain(Some(0)).collect();
    let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
    if unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
        mode.dmDisplayFrequency as u16
    } else {
        0
    }
}

/// The monitors and which of them `streaming` (a GDI name) is.
pub fn list(streaming: &str) -> Option<(Monitors, Vec<Output>)> {
    let all = outputs().ok()?;
    let names = friendly_names();
    let mut m = Monitors::default();
    for (i, o) in all.iter().take(MAX_MONITORS).enumerate() {
        let (w, h) = ((o.rect.right - o.rect.left) as u16, (o.rect.bottom - o.rect.top) as u16);
        let name = names.get(&o.device).cloned().unwrap_or_else(|| o.device.trim_start_matches("\\\\.\\").to_string());
        m.list[i] = Monitor::new(w, h, refresh_hz(&o.device), o.primary(), &name);
        if o.device == streaming {
            m.streaming = i as u8;
        }
        m.count += 1;
    }
    Some((m, all))
}
