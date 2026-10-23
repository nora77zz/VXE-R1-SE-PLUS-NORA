//! Windows PnP notifications only wake the HID actor; callbacks never do I/O.
#[cfg(windows)]
mod windows_watcher {
    use std::ffi::c_void;
    use windows::Win32::Devices::DeviceAndDriverInstallation::*;
    use windows::Win32::Devices::HumanInterfaceDevice::HidD_GetHidGuid;

    struct Context(Box<dyn Fn() + Send + Sync>);

    pub struct DeviceChangeWatcher {
        handle: HCMNOTIFICATION,
        context: *mut Context,
    }

    unsafe extern "system" fn changed(
        _: HCMNOTIFICATION,
        context: *const c_void,
        action: CM_NOTIFY_ACTION,
        event_data: *const CM_NOTIFY_EVENT_DATA,
        event_size: u32,
    ) -> u32 {
        if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
            || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
        {
            const LINK_OFFSET: usize = std::mem::offset_of!(CM_NOTIFY_EVENT_DATA, u)
                + std::mem::offset_of!(CM_NOTIFY_EVENT_DATA_0_2, SymbolicLink);
            if event_data.is_null() || (event_size as usize) < LINK_OFFSET + 2 { return 0; }
            let count = ((event_size as usize - LINK_OFFSET) / 2).min(32768);
            let link = std::slice::from_raw_parts((event_data as *const u8).add(LINK_OFFSET).cast::<u16>(), count);
            if !supported_link(link) { return 0; }
            let context = &*(context as *const Context);
            // Never unwind across the Windows callback boundary.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (context.0)()));
        }
        0
    }

    fn supported_link(link: &[u16]) -> bool {
        let end = link.iter().position(|value| *value == 0).unwrap_or(link.len());
        let link = String::from_utf16_lossy(&link[..end]).to_ascii_lowercase();
        link.contains("vid_3554&pid_f58f") || link.contains("vid_373b&pid_1085")
    }

    #[cfg(test)]
    mod tests {
        use super::supported_link;
        #[test]
        fn watcher_registers_and_unregisters_on_windows() {
            let watcher = super::DeviceChangeWatcher::new(|| {}).expect("HID PnP notification registration");
            drop(watcher);
        }
        #[test]
        fn notifications_accept_only_the_two_supported_usb_identities() {
            for link in ["\\\\?\\HID#VID_3554&PID_F58F&MI_00#abc", "\\\\?\\hid#vid_373b&pid_1085&mi_00#abc"] {
                assert!(supported_link(&link.encode_utf16().collect::<Vec<_>>()));
            }
            for link in ["\\\\?\\hid#vid_3554&pid_0001#abc", "\\\\?\\hid#vid_1234&pid_1085#abc", ""] {
                assert!(!supported_link(&link.encode_utf16().collect::<Vec<_>>()));
            }
        }
    }

    impl DeviceChangeWatcher {
        pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Result<Self, String> {
            let mut filter = CM_NOTIFY_FILTER::default();
            filter.cbSize = std::mem::size_of::<CM_NOTIFY_FILTER>() as u32;
            filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
            filter.u.DeviceInterface.ClassGuid = unsafe { HidD_GetHidGuid() };
            let context = Box::into_raw(Box::new(Context(Box::new(wake))));
            let mut handle = HCMNOTIFICATION::default();
            let result = unsafe {
                CM_Register_Notification(&filter, Some(context.cast()), Some(changed), &mut handle)
            };
            if result != CR_SUCCESS {
                unsafe { drop(Box::from_raw(context)); }
                return Err(format!("CM_Register_Notification: {}", result.0));
            }
            Ok(Self { handle, context })
        }
    }

    impl Drop for DeviceChangeWatcher {
        fn drop(&mut self) {
            // Successful unregister waits for callbacks. On failure retain the
            // context so an outstanding callback cannot dereference freed memory.
            if unsafe { CM_Unregister_Notification(self.handle) } == CR_SUCCESS {
                unsafe { drop(Box::from_raw(self.context)); }
            }
        }
    }
}

#[cfg(windows)]
pub use windows_watcher::DeviceChangeWatcher;
