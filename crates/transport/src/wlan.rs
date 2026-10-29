//! Windows Wi-Fi tuning for low latency: disables background scans and enables
//! media-streaming mode on every wireless interface. Windows reverts both when
//! the handle closes, so keep the `Tuning` alive for as long as streaming runs.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::NetworkManagement::WiFi::{
        WLAN_INTERFACE_INFO, WLAN_INTERFACE_INFO_LIST, WLAN_INTF_OPCODE, WlanCloseHandle, WlanEnumInterfaces,
        WlanFreeMemory, WlanOpenHandle, WlanSetInterface, wlan_intf_opcode_background_scan_enabled,
        wlan_intf_opcode_media_streaming_mode,
    };

    pub struct Tuning(HANDLE);

    impl Drop for Tuning {
        fn drop(&mut self) {
            unsafe { WlanCloseHandle(self.0, ptr::null()) };
        }
    }

    /// Returns `None` when there is no WLAN service (e.g. a wired-only machine).
    pub fn tune() -> Option<Tuning> {
        let mut version = 0u32;
        let mut handle: HANDLE = ptr::null_mut();
        if unsafe { WlanOpenHandle(2, ptr::null(), &mut version, &mut handle) } != ERROR_SUCCESS {
            return None;
        }
        let tuning = Tuning(handle);

        let mut list: *mut WLAN_INTERFACE_INFO_LIST = ptr::null_mut();
        if unsafe { WlanEnumInterfaces(handle, ptr::null(), &mut list) } != ERROR_SUCCESS {
            return None;
        }
        let ifaces: &[WLAN_INTERFACE_INFO] = unsafe {
            std::slice::from_raw_parts((*list).InterfaceInfo.as_ptr(), (*list).dwNumberOfItems as usize)
        };
        for iface in ifaces {
            let name = String::from_utf16_lossy(&iface.strInterfaceDescription);
            let name = name.trim_end_matches('\0');
            let set = |op: WLAN_INTF_OPCODE, on: bool| {
                let v: i32 = on.into();
                let r = unsafe {
                    WlanSetInterface(
                        handle,
                        &iface.InterfaceGuid,
                        op,
                        size_of::<i32>() as u32,
                        &v as *const i32 as *const c_void,
                        ptr::null(),
                    )
                };
                if r == ERROR_SUCCESS { "ok".to_string() } else { format!("error {r}") }
            };
            println!(
                "wifi tuning on {name}: background scan off {}, media streaming mode {}",
                set(wlan_intf_opcode_background_scan_enabled, false),
                set(wlan_intf_opcode_media_streaming_mode, true),
            );
        }
        unsafe { WlanFreeMemory(list as *const c_void) };
        Some(tuning)
    }
}

#[cfg(not(windows))]
mod imp {
    pub struct Tuning;

    pub fn tune() -> Option<Tuning> {
        None
    }
}

pub use imp::{Tuning, tune};
