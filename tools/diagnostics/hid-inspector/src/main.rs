#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod windows_diagnostics {
    use serde::Serialize;
    use std::ffi::OsString;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStringExt;
    use std::slice;
    use windows::core::{GUID, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
        SetupDiGetDeviceInterfaceDetailW, HDEVINFO, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
        SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
    };
    use windows::Win32::Devices::HumanInterfaceDevice::{
        HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
        HidP_GetButtonCaps, HidP_GetCaps, HidP_GetValueCaps, HIDD_ATTRIBUTES,
        HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_REPORT_TYPE, HIDP_STATUS_SUCCESS,
        HIDP_VALUE_CAPS, HidP_Feature, HidP_Input, HidP_Output, PHIDP_PREPARSED_DATA,
    };
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HWND, ERROR_INSUFFICIENT_BUFFER,
        ERROR_NO_MORE_ITEMS, INVALID_HANDLE_VALUE,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    const TARGET_VENDOR_ID: u16 = 0x373b;
    const TARGET_PRODUCT_ID: u16 = 0x1085;

    #[derive(Serialize)]
    struct DiagnosticError {
        stage: String,
        win32_error: Option<u32>,
        ntstatus: Option<String>,
        detail: Option<String>,
    }

    impl DiagnosticError {
        fn win32(stage: impl Into<String>, code: u32) -> Self {
            Self {
                stage: stage.into(),
                win32_error: Some(code),
                ntstatus: None,
                detail: None,
            }
        }

        fn ntstatus(stage: impl Into<String>, status: i32) -> Self {
            Self {
                stage: stage.into(),
                win32_error: None,
                ntstatus: Some(format!("0x{:08x}", status as u32)),
                detail: None,
            }
        }

        fn detail(stage: impl Into<String>, message: impl Into<String>) -> Self {
            Self {
                stage: stage.into(),
                win32_error: None,
                ntstatus: None,
                detail: Some(message.into()),
            }
        }
    }

    #[derive(Serialize)]
    struct ReportIds {
        value_caps: Vec<u8>,
        button_caps: Vec<u8>,
    }

    #[derive(Serialize)]
    struct ReportTypeCaps {
        input: ReportIds,
        output: ReportIds,
        feature: ReportIds,
    }

    #[derive(Serialize)]
    struct CollectionDiagnostic {
        vendor_id: u16,
        product_id: u16,
        usage_page: u16,
        usage: u16,
        input_report_byte_length: u16,
        output_report_byte_length: u16,
        feature_report_byte_length: u16,
        report_ids: ReportTypeCaps,
        errors: Vec<DiagnosticError>,
    }

    #[derive(Serialize)]
    struct DiagnosticOutput {
        schema_version: u8,
        status: &'static str,
        operation: &'static str,
        target_vendor_id: u16,
        target_product_id: u16,
        interface_candidates_matching_path: u32,
        collections_reported: u32,
        writes_sent: u8,
        input_reports_read: u8,
        collections: Vec<CollectionDiagnostic>,
        errors: Vec<DiagnosticError>,
        limitations: Vec<&'static str>,
    }

    fn win32_last_error(stage: &'static str) -> DiagnosticError {
        DiagnosticError::win32(stage, unsafe { GetLastError().0 })
    }

    fn collect_value_report_ids(
        report_type: HIDP_REPORT_TYPE,
        count: u16,
        preparsed: PHIDP_PREPARSED_DATA,
        stage: &'static str,
    ) -> Result<Vec<u8>, DiagnosticError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut caps = vec![HIDP_VALUE_CAPS::default(); usize::from(count)];
        let mut returned = count;
        let status = unsafe {
            HidP_GetValueCaps(report_type, caps.as_mut_ptr(), &mut returned, preparsed)
        };
        if status != HIDP_STATUS_SUCCESS {
            return Err(DiagnosticError::ntstatus(stage, status.0));
        }
        if usize::from(returned) > caps.len() {
            return Err(DiagnosticError::detail(stage, "HID API returned a cap count larger than its input buffer"));
        }
        let mut ids = caps[..usize::from(returned)]
            .iter()
            .map(|cap| cap.ReportID)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn collect_button_report_ids(
        report_type: HIDP_REPORT_TYPE,
        count: u16,
        preparsed: PHIDP_PREPARSED_DATA,
        stage: &'static str,
    ) -> Result<Vec<u8>, DiagnosticError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut caps = vec![HIDP_BUTTON_CAPS::default(); usize::from(count)];
        let mut returned = count;
        let status = unsafe {
            HidP_GetButtonCaps(report_type, caps.as_mut_ptr(), &mut returned, preparsed)
        };
        if status != HIDP_STATUS_SUCCESS {
            return Err(DiagnosticError::ntstatus(stage, status.0));
        }
        if usize::from(returned) > caps.len() {
            return Err(DiagnosticError::detail(stage, "HID API returned a cap count larger than its input buffer"));
        }
        let mut ids = caps[..usize::from(returned)]
            .iter()
            .map(|cap| cap.ReportID)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    fn ids_for_type(
        report_type: HIDP_REPORT_TYPE,
        value_count: u16,
        button_count: u16,
        preparsed: PHIDP_PREPARSED_DATA,
        type_name: &'static str,
        errors: &mut Vec<DiagnosticError>,
    ) -> ReportIds {
        let value_caps = match collect_value_report_ids(
            report_type,
            value_count,
            preparsed,
            match type_name {
                "input" => "HidP_GetValueCaps(input)",
                "output" => "HidP_GetValueCaps(output)",
                _ => "HidP_GetValueCaps(feature)",
            },
        ) {
            Ok(ids) => ids,
            Err(error) => {
                errors.push(error);
                Vec::new()
            }
        };
        let button_caps = match collect_button_report_ids(
            report_type,
            button_count,
            preparsed,
            match type_name {
                "input" => "HidP_GetButtonCaps(input)",
                "output" => "HidP_GetButtonCaps(output)",
                _ => "HidP_GetButtonCaps(feature)",
            },
        ) {
            Ok(ids) => ids,
            Err(error) => {
                errors.push(error);
                Vec::new()
            }
        };
        ReportIds {
            value_caps,
            button_caps,
        }
    }

    fn inspect_open_collection(
        handle: HANDLE,
        attributes: HIDD_ATTRIBUTES,
    ) -> Result<CollectionDiagnostic, DiagnosticError> {
        let mut preparsed = PHIDP_PREPARSED_DATA::default();
        let has_preparsed = unsafe { HidD_GetPreparsedData(handle, &mut preparsed) };
        if has_preparsed.0 == 0 {
            return Err(win32_last_error("HidD_GetPreparsedData"));
        }

        let mut caps = HIDP_CAPS::default();
        let status = unsafe { HidP_GetCaps(preparsed, &mut caps) };
        let result = if status != HIDP_STATUS_SUCCESS {
            Err(DiagnosticError::ntstatus("HidP_GetCaps", status.0))
        } else {
            let mut errors = Vec::new();
            let report_ids = ReportTypeCaps {
                input: ids_for_type(
                    HidP_Input,
                    caps.NumberInputValueCaps,
                    caps.NumberInputButtonCaps,
                    preparsed,
                    "input",
                    &mut errors,
                ),
                output: ids_for_type(
                    HidP_Output,
                    caps.NumberOutputValueCaps,
                    caps.NumberOutputButtonCaps,
                    preparsed,
                    "output",
                    &mut errors,
                ),
                feature: ids_for_type(
                    HidP_Feature,
                    caps.NumberFeatureValueCaps,
                    caps.NumberFeatureButtonCaps,
                    preparsed,
                    "feature",
                    &mut errors,
                ),
            };
            Ok(CollectionDiagnostic {
                vendor_id: attributes.VendorID,
                product_id: attributes.ProductID,
                usage_page: caps.UsagePage,
                usage: caps.Usage,
                input_report_byte_length: caps.InputReportByteLength,
                output_report_byte_length: caps.OutputReportByteLength,
                feature_report_byte_length: caps.FeatureReportByteLength,
                report_ids,
                errors,
            })
        };

        let freed = unsafe { HidD_FreePreparsedData(preparsed) };
        if freed.0 == 0 {
            let free_error = win32_last_error("HidD_FreePreparsedData");
            return match result {
                Ok(mut collection) => {
                    collection.errors.push(free_error);
                    Ok(collection)
                }
                Err(_) => Err(free_error),
            };
        }
        result
    }

    fn inspect_path(path: &[u16], vendor_id: u16, product_id: u16) -> Result<Option<CollectionDiagnostic>, DiagnosticError> {
        let handle = match unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            )
        } {
            Ok(handle) => handle,
            Err(_) => return Err(win32_last_error("CreateFileW(read-only-access-zero)")),
        };
        if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
            return Err(win32_last_error("CreateFileW(read-only-access-zero)"));
        }

        let mut attributes = HIDD_ATTRIBUTES::default();
        attributes.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
        let has_attributes = unsafe { HidD_GetAttributes(handle, &mut attributes) };
        let result = if has_attributes.0 == 0 {
            Err(win32_last_error("HidD_GetAttributes"))
        } else if attributes.VendorID != vendor_id || attributes.ProductID != product_id {
            Ok(None)
        } else {
            inspect_open_collection(handle, attributes).map(Some)
        };

        let close_result = unsafe { CloseHandle(handle) };
        if close_result.is_err() {
            let close_error = win32_last_error("CloseHandle");
            return match result {
                Ok(Some(mut collection)) => {
                    collection.errors.push(close_error);
                    Ok(Some(collection))
                }
                Ok(None) => Err(close_error),
                Err(_) => Err(close_error),
            };
        }
        result
    }

    fn path_from_detail(detail: *const SP_DEVICE_INTERFACE_DETAIL_DATA_W, byte_count: u32) -> Vec<u16> {
        let device_path_ptr = unsafe { std::ptr::addr_of!((*detail).DevicePath).cast::<u16>() };
        let offset = size_of::<u32>();
        let max_chars = byte_count.saturating_sub(offset as u32) as usize / size_of::<u16>();
        let path_with_tail = unsafe { slice::from_raw_parts(device_path_ptr, max_chars) };
        let end = path_with_tail.iter().position(|character| *character == 0).unwrap_or(max_chars);
        let mut path = path_with_tail[..end].to_vec();
        path.push(0);
        path
    }

    fn path_mentions_target(path: &[u16], vendor_id: u16, product_id: u16) -> bool {
        let text = OsString::from_wide(path).to_string_lossy().to_ascii_lowercase();
        text.contains(&format!("vid_{vendor_id:04x}")) && text.contains(&format!("pid_{product_id:04x}"))
    }

    fn get_interface_path(
        device_info_set: HDEVINFO,
        interface: &SP_DEVICE_INTERFACE_DATA,
    ) -> Result<Vec<u16>, DiagnosticError> {
        let mut required_size = 0u32;
        let first_call = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                device_info_set,
                interface,
                None,
                0,
                Some(&mut required_size),
                None,
            )
        };
        if first_call.is_ok() {
            return Err(DiagnosticError::detail(
                "SetupDiGetDeviceInterfaceDetailW(size-query)",
                "size query unexpectedly succeeded without an output buffer",
            ));
        }
        let query_error = unsafe { GetLastError().0 };
        if query_error != ERROR_INSUFFICIENT_BUFFER.0 {
            return Err(DiagnosticError::win32(
                "SetupDiGetDeviceInterfaceDetailW(size-query)",
                query_error,
            ));
        }
        if required_size < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 {
            return Err(DiagnosticError::detail(
                "SetupDiGetDeviceInterfaceDetailW(size-query)",
                "required buffer size is smaller than the detail structure",
            ));
        }

        let words = (required_size as usize).div_ceil(size_of::<usize>());
        let mut storage = vec![0usize; words];
        let detail = storage.as_mut_ptr().cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        let filled = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                device_info_set,
                interface,
                Some(detail),
                required_size,
                Some(&mut required_size),
                None,
            )
        };
        if filled.is_err() {
            return Err(win32_last_error("SetupDiGetDeviceInterfaceDetailW(fill)"));
        }
        Ok(path_from_detail(detail, required_size))
    }

    fn run_diagnostics(vendor_id: u16, product_id: u16) -> DiagnosticOutput {
        let mut output = DiagnosticOutput {
            schema_version: 1,
            status: "ok",
            operation: "read-only HID descriptor and caps inspection; no reports sent or read",
            target_vendor_id: vendor_id,
            target_product_id: product_id,
            interface_candidates_matching_path: 0,
            collections_reported: 0,
            writes_sent: 0,
            input_reports_read: 0,
            collections: Vec::new(),
            errors: Vec::new(),
            limitations: vec![
                "No HID input, output, or feature reports were transferred.",
                "No device path, serial number, or unique identifier is emitted.",
                "A response checksum rule is not established by the current frontend source evidence.",
            ],
        };

        let hid_guid: GUID = unsafe { HidD_GetHidGuid() };
        let info_set = match unsafe {
            SetupDiGetClassDevsW(
                Some(&hid_guid),
                PCWSTR::null(),
                HWND::default(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        } {
            Ok(info_set) => info_set,
            Err(_) => {
                output.status = "enumeration-error";
                output.errors.push(win32_last_error("SetupDiGetClassDevsW"));
                return output;
            }
        };

        let mut index = 0u32;
        loop {
            let mut interface = SP_DEVICE_INTERFACE_DATA::default();
            interface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            let enumerated = unsafe {
                SetupDiEnumDeviceInterfaces(info_set, None, &hid_guid, index, &mut interface)
            };
            if enumerated.is_err() {
                let code = unsafe { GetLastError().0 };
                if code != ERROR_NO_MORE_ITEMS.0 {
                    output.status = "enumeration-partial-error";
                    output.errors.push(DiagnosticError::win32("SetupDiEnumDeviceInterfaces", code));
                }
                break;
            }
            index = index.saturating_add(1);

            let path = match get_interface_path(info_set, &interface) {
                Ok(path) => path,
                Err(error) => {
                    output.errors.push(error);
                    continue;
                }
            };
            if !path_mentions_target(&path, vendor_id, product_id) {
                continue;
            }
            output.interface_candidates_matching_path += 1;
            match inspect_path(&path, vendor_id, product_id) {
                Ok(Some(collection)) => output.collections.push(collection),
                Ok(None) => {}
                Err(error) => output.errors.push(error),
            }
        }

        if unsafe { SetupDiDestroyDeviceInfoList(info_set) }.is_err() {
            output.errors.push(win32_last_error("SetupDiDestroyDeviceInfoList"));
            if output.status == "ok" {
                output.status = "cleanup-error";
            }
        }
        output.collections_reported = output.collections.len() as u32;
        if output.collections.iter().any(|collection| !collection.errors.is_empty()) && output.status == "ok" {
            output.status = "caps-partial-error";
        }
        if !output.errors.is_empty() && output.status == "ok" {
            output.status = "partial-error";
        }
        output
    }

    pub fn main() {
        let args: Vec<_> = std::env::args().skip(1).collect();
        if matches!(args.as_slice(), [help] if help == "--help" || help == "-h") {
            println!("hid-diagnostics [--vid HEX --pid HEX]\nReads descriptor metadata only; no HID reports are transferred.");
            return;
        }
        let target = match args.as_slice() {
            [] => Some((TARGET_VENDOR_ID, TARGET_PRODUCT_ID)),
            [vid_flag, vid, pid_flag, pid] if vid_flag == "--vid" && pid_flag == "--pid" => {
                let parse = |value: &str| u16::from_str_radix(value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value), 16).ok();
                parse(vid).zip(parse(pid))
            }
            _ => None,
        };
        let Some((vendor_id, product_id)) = target else {
            eprintln!("Usage: hid-diagnostics [--vid HEX --pid HEX]");
            std::process::exit(2);
        };
        let output = run_diagnostics(vendor_id, product_id);
        match serde_json::to_string_pretty(&output) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("failed to serialize diagnostic JSON: {error}");
                std::process::exit(2);
            }
        }
        if output.status != "ok" {
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn main() {
    windows_diagnostics::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("hid-diagnostics is supported only on Windows");
    std::process::exit(1);
}
