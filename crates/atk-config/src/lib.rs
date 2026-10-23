//! High-level mouse configuration and EEPROM serialization for VXE R1 SE+ / COMPX.
//!
//! Grounded in reverse-engineered structures from the validated ATK HUB frontend:
//! - Addresses: `K$t.Address`
//! - Report rate: `z0t` (address 0)
//! - Silent height (LOD): `E0t` (address 10)
//! - DPI stages: `I0t` (addresses 12, 20, 28, 36)
//! - DPI colors: `S0t` (addresses 44, 52, 60, 68)
//! - RGB lighting: `F0t` (address 76)
//! - Keys: `j0t` (addresses 96, 100, 104, 108, 112, 116)
//! - Stabilization & correction: `X0t` (address 169)
//! - Sensor enable: `N0t` (address 181)
//! - Sensor angle: `q0t` (address 189)
//! - Far distance mode: `D0t` (opcode 22/23)

pub mod dpi;
pub mod dpi_map;

use atk_protocol::{Request, GetCommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const EEPROM_REPORT_RATE: u16 = 0;
pub const EEPROM_SILENT_HEIGHT: u16 = 10;
pub const EEPROM_DPI_STAGES: [u16; 4] = [12, 20, 28, 36];
pub const EEPROM_DPI_COLORS: [u16; 4] = [44, 52, 60, 68];
pub const EEPROM_RGB_LIGHT: u16 = 76;
pub const EEPROM_KEYS: [u16; 6] = [96, 100, 104, 108, 112, 116];
pub const EEPROM_STABILIZATION: u16 = 169;
pub const EEPROM_SENSOR_ENABLE: u16 = 181;
pub const EEPROM_SENSOR_ANGLE: u16 = 189;

pub const CMD_SET_EEPROM: u8 = 0x07;
pub const CMD_GET_EEPROM: u8 = 0x08;
pub const CMD_GET_CURRENT_CONFIG: u8 = 0x0E;
pub const CMD_SET_CURRENT_CONFIG: u8 = 0x0F;
pub const CMD_SET_FAR_DISTANCE: u8 = 0x16;
pub const CMD_GET_FAR_DISTANCE: u8 = 0x17;

#[inline]
fn crc_complement(val: u8) -> u8 {
    0x55u8.wrapping_sub(val)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Default for RgbColor {
    fn default() -> Self {
        Self { r: 255, g: 0, b: 0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DpiCoordinates {
    pub x_dpi: u16,
    pub y_dpi: u16,
    pub dpi_ex: u8,
    pub uses_logical: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DpiLevelEntry {
    pub dpi: DpiCoordinates,
    pub color: RgbColor,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LightConfig {
    pub mode: u8,
    pub brightness: u8,
    pub speed: u8,
    pub dpi_rgb_enabled: u8,
}

impl Default for LightConfig {
    fn default() -> Self {
        Self {
            mode: 1,
            brightness: 2,
            speed: 1,
            dpi_rgb_enabled: 1,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MouseSettings {
    pub report_rate: u8,
    pub performance_mode: bool,
    pub silent_height: u8,
    pub sensor_angle: i8,
    pub move_sync: bool,
    pub line_correction: bool,
    pub ripple_correction: bool,
    pub far_distance: bool,
    pub key_debounce_time: u8,
    pub sleep_time: u8,
    pub dpi_level: usize,
    pub dpi_total: usize,
    pub dpi_list: Vec<DpiLevelEntry>,
    pub light_config: LightConfig,
    pub button_mappings: Value,
    pub dpi_toggle_enabled: bool,
    pub sensor_sleep_enabled: bool,
    pub sensor_sleep_time: u32,
    pub sensor_model: u8,
    pub button_operation_mode: u8,
    pub rolling_delay: u8,
    pub bhop: u8,
    pub sensor_angle_enable: bool,
}

impl Default for MouseSettings {
    fn default() -> Self {
        Self {
            report_rate: 3, // 1000 Hz
            performance_mode: false,
            silent_height: 1, // 1mm
            sensor_angle: 0,
            move_sync: false,
            line_correction: false,
            ripple_correction: false,
            far_distance: false,
            key_debounce_time: 4, // 4ms
            sleep_time: 1, // 1 min
            dpi_level: 0,
            dpi_total: 4,
            dpi_list: vec![
                DpiLevelEntry {
                    dpi: DpiCoordinates { x_dpi: 800, y_dpi: 800, dpi_ex: 0, uses_logical: true },
                    color: RgbColor { r: 255, g: 0, b: 0 },
                },
                DpiLevelEntry {
                    dpi: DpiCoordinates { x_dpi: 1600, y_dpi: 1600, dpi_ex: 0, uses_logical: true },
                    color: RgbColor { r: 0, g: 255, b: 0 },
                },
                DpiLevelEntry {
                    dpi: DpiCoordinates { x_dpi: 2400, y_dpi: 2400, dpi_ex: 0, uses_logical: true },
                    color: RgbColor { r: 0, g: 0, b: 255 },
                },
                DpiLevelEntry {
                    dpi: DpiCoordinates { x_dpi: 3200, y_dpi: 3200, dpi_ex: 0, uses_logical: true },
                    color: RgbColor { r: 255, g: 255, b: 0 },
                },
            ],
            light_config: LightConfig::default(),
            button_mappings: json!({
                "changeKeys": {
                    "device_after": {
                        "1": "Left Button",
                        "2": "Middle Button",
                        "3": "Right Button",
                        "4": "X1",
                        "5": "X2",
                        "6": "DPI Loop"
                    }
                }
            }),
            dpi_toggle_enabled: true,
            sensor_sleep_enabled: true,
            sensor_sleep_time: 60,
            sensor_model: 1,
            button_operation_mode: 0,
            rolling_delay: 0,
            bhop: 0,
            sensor_angle_enable: false,
        }
    }
}

impl MouseSettings {
    /// Formats the settings as the nested JSON payload expected by `main.ts`.
    pub fn to_frontend_json(&self) -> Value {
        let mouse_config = json!({
            "reportRate": self.report_rate,
            "performanceMode": self.performance_mode,
            "silentHeight": self.silent_height,
            "sensorAngle": self.sensor_angle,
            "moveSync": self.move_sync,
            "lineCorrection": self.line_correction,
            "rippleCorrection": self.ripple_correction,
            "farDistance": self.far_distance,
            "keyDebounceTime": self.key_debounce_time,
            "sleepTime": self.sleep_time,
            "dpiLevel": self.dpi_level,
            "dpiTotal": self.dpi_total,
            "dpiList": self.dpi_list,
            "dpiToggleEnabled": self.dpi_toggle_enabled,
            "sensorSleepEnabled": self.sensor_sleep_enabled,
            "sensorSleepTime": self.sensor_sleep_time,
            "sensorModel": self.sensor_model,
            "buttonOperationMode": self.button_operation_mode,
            "rollingDelay": self.rolling_delay,
            "bhop": self.bhop,
            "sensorAngleEnable": self.sensor_angle_enable,
            "buttonMappings": self.button_mappings,
        });

        json!({
            "deviceConfig": {
                "version": "3.15",
                "onboard": 1,
                "mouseConfig": mouse_config,
                "lightConfig": self.light_config,
            },
            "mouseConfig": mouse_config,
            "reportRate": self.report_rate,
            "performanceMode": self.performance_mode,
            "silentHeight": self.silent_height,
            "sensorAngle": self.sensor_angle,
            "moveSync": self.move_sync,
            "lineCorrection": self.line_correction,
            "rippleCorrection": self.ripple_correction,
            "farDistance": self.far_distance,
            "keyDebounceTime": self.key_debounce_time,
            "sleepTime": self.sleep_time,
            "dpiLevel": self.dpi_level,
            "dpiTotal": self.dpi_total,
            "dpiList": self.dpi_list,
            "buttonMappings": self.button_mappings,
        })
    }
}

/// Generates the list of GET requests required to read full mouse EEPROM settings.
pub fn read_requests() -> Vec<Request> {
    vec![
        // 1. Report rate, max DPI, current DPI, bhop, button operation mode (10 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_REPORT_RATE, 10, &[]).unwrap(),
        // 2. Silent height / LOD (2 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_SILENT_HEIGHT, 2, &[]).unwrap(),
        // 3..6. DPI stages 1..8 (4 requests of 8 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_STAGES[0], 8, &[]).unwrap(),
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_STAGES[1], 8, &[]).unwrap(),
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_STAGES[2], 8, &[]).unwrap(),
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_STAGES[3], 8, &[]).unwrap(),
        // 7..10. DPI colors 1..8 (4 requests of 8 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_COLORS[0], 8, &[]).unwrap(),
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_COLORS[1], 8, &[]).unwrap(),
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_COLORS[2], 8, &[]).unwrap(),
        Request::from_fields(CMD_GET_EEPROM, EEPROM_DPI_COLORS[3], 8, &[]).unwrap(),
        // 11. RGB light effects (8 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_RGB_LIGHT, 8, &[]).unwrap(),
        // 12. Stabilization, motion sync, close led time, linear correction, ripple control (10 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_STABILIZATION, 10, &[]).unwrap(),
        // 13. Sensor enable, sleep time, sensor model (6 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_SENSOR_ENABLE, 6, &[]).unwrap(),
        // 14. Sensor angle & switch (4 bytes)
        Request::from_fields(CMD_GET_EEPROM, EEPROM_SENSOR_ANGLE, 4, &[]).unwrap(),
        // 15. Far distance mode
        Request::from_fields(CMD_GET_FAR_DISTANCE, 0, 10, &[]).unwrap(),
        // 16. Current configuration profile
        Request::from_fields(CMD_GET_CURRENT_CONFIG, 0, 1, &[]).unwrap(),
        // 17. Identity (CID/MID)
        Request::get(GetCommand::GetIdentity),
        // 18. Mouse version
        Request::get(GetCommand::GetMouseVersion),
        // 19. Dongle version
        Request::get(GetCommand::GetDongleVersion),
    ]
}

/// Builds a SetEEPROM request for report rate, max DPI, and current DPI (address 0, 10 bytes).
pub fn encode_report_rate_packet(
    rate_hw: u8,
    max_dpi: u8,
    current_dpi: u8,
    bhop: u8,
    op_mode: u8,
) -> Request {
    let payload = [
        rate_hw,
        crc_complement(rate_hw),
        max_dpi,
        crc_complement(max_dpi),
        current_dpi,
        crc_complement(current_dpi),
        bhop,
        crc_complement(bhop),
        op_mode,
        crc_complement(op_mode),
    ];
    Request::from_fields(CMD_SET_EEPROM, EEPROM_REPORT_RATE, 10, &payload).unwrap()
}

/// Converts UI polling rate index (0..6: 125, 250, 500, 1000, 2000, 4000, 8000 Hz) to hardware code.
pub fn ui_rate_to_hardware(index: u8) -> u8 {
    match index {
        0 => 8,  // 125Hz
        1 => 4,  // 250Hz
        2 => 2,  // 500Hz
        3 => 1,  // 1000Hz
        4 => 16, // 2000Hz
        5 => 32, // 4000Hz
        6 => 64, // 8000Hz
        _ => 1,  // Default 1000Hz
    }
}

/// Converts hardware polling rate code to UI index (0..6).
pub fn hardware_rate_to_ui(code: u8) -> u8 {
    match code {
        8 => 0,  // 125Hz
        4 => 1,  // 250Hz
        2 => 2,  // 500Hz
        1 => 3,  // 1000Hz
        16 => 4, // 2000Hz
        32 => 5, // 4000Hz
        64 => 6, // 8000Hz
        _ => 3,  // Default 1000Hz
    }
}

/// Builds a SetEEPROM request for silent height / LOD (address 10, 2 bytes).
pub fn encode_silent_height_packet(silent_height: u8) -> Request {
    let payload = [silent_height, crc_complement(silent_height)];
    Request::from_fields(CMD_SET_EEPROM, EEPROM_SILENT_HEIGHT, 2, &payload).unwrap()
}

/// Builds a SetEEPROM request for stabilization & correction settings (address 169, 10 bytes).
pub fn encode_stabilization_packet(
    debounce_ms: u8,
    motion_sync: bool,
    sleep_time_index: u8,
    linear_corr: bool,
    ripple_ctrl: bool,
) -> Request {
    let ms_val = if motion_sync { 1 } else { 0 };
    let lc_val = if linear_corr { 1 } else { 0 };
    let rc_val = if ripple_ctrl { 1 } else { 0 };
    let payload = [
        debounce_ms,
        crc_complement(debounce_ms),
        ms_val,
        crc_complement(ms_val),
        sleep_time_index,
        crc_complement(sleep_time_index),
        lc_val,
        crc_complement(lc_val),
        rc_val,
        crc_complement(rc_val),
    ];
    Request::from_fields(CMD_SET_EEPROM, EEPROM_STABILIZATION, 10, &payload).unwrap()
}

/// Builds a SetEEPROM request for sensor angle (address 189, 4 bytes).
pub fn encode_sensor_angle_packet(angle: i8, enabled: bool) -> Request {
    let angle_u8 = angle as u8;
    let en_u8 = if enabled { 1 } else { 0 };
    let payload = [
        angle_u8,
        crc_complement(angle_u8),
        en_u8,
        crc_complement(en_u8),
    ];
    Request::from_fields(CMD_SET_EEPROM, EEPROM_SENSOR_ANGLE, 4, &payload).unwrap()
}

/// Builds a SetFarDistanceMode request (opcode 22).
pub fn encode_far_distance_packet(enabled: bool) -> Request {
    let val = if enabled { 1 } else { 0 };
    let mut payload = [0u8; 10];
    payload[0] = val;
    Request::from_fields(CMD_SET_FAR_DISTANCE, 0, 10, &payload).unwrap()
}

/// Builds a SetEEPROM request for two DPI stages (address in `EEPROM_DPI_STAGES`, 8 bytes).
pub fn encode_dpi_stages_packet(
    address: u16,
    stage_a: (u8, u8, u8),
    stage_b: (u8, u8, u8),
) -> Request {
    let (xa, ya, ex_a) = stage_a;
    let crc_a = 0x55u8.wrapping_sub(xa.wrapping_add(ya).wrapping_add(ex_a));
    let (xb, yb, ex_b) = stage_b;
    let crc_b = 0x55u8.wrapping_sub(xb.wrapping_add(yb).wrapping_add(ex_b));
    let payload = [xa, ya, ex_a, crc_a, xb, yb, ex_b, crc_b];
    Request::from_fields(CMD_SET_EEPROM, address, 8, &payload).unwrap()
}

/// Builds a SetEEPROM request for two DPI colors (address in `EEPROM_DPI_COLORS`, 8 bytes).
pub fn encode_dpi_colors_packet(
    address: u16,
    color_a: &RgbColor,
    color_b: &RgbColor,
) -> Request {
    let crc_a = 0x55u8.wrapping_sub(color_a.r.wrapping_add(color_a.g).wrapping_add(color_a.b));
    let crc_b = 0x55u8.wrapping_sub(color_b.r.wrapping_add(color_b.g).wrapping_add(color_b.b));
    let payload = [
        color_a.r, color_a.g, color_a.b, crc_a,
        color_b.r, color_b.g, color_b.b, crc_b,
    ];
    Request::from_fields(CMD_SET_EEPROM, address, 8, &payload).unwrap()
}

/// Builds a SetEEPROM request for a single button assignment (address 96, 100, 104, 108, 112, 116, 4 bytes).
pub fn encode_button_packet(address: u16, key_class: u8, val1: u8, val2: u8) -> Request {
    let crc = 0x55u8.wrapping_sub(key_class.wrapping_add(val1).wrapping_add(val2));
    let payload = [key_class, val1, val2, crc];
    Request::from_fields(CMD_SET_EEPROM, address, 4, &payload).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_report_rate_crc_complements() {
        let req = encode_report_rate_packet(1, 4, 0, 0, 0);
        let bytes = req.bytes();
        assert_eq!(bytes[0], CMD_SET_EEPROM);
        assert_eq!(bytes[2..4], EEPROM_REPORT_RATE.to_be_bytes());
        assert_eq!(bytes[4], 10);
        // byte 5 is rate_hw (1), byte 6 is 85 - 1 = 84
        assert_eq!(bytes[5], 1);
        assert_eq!(bytes[6], 84);
        assert_eq!(bytes[7], 4);
        assert_eq!(bytes[8], 81);
        assert!(req.has_valid_outgoing_checksum());
    }

    #[test]
    fn test_silent_height_packet() {
        let req = encode_silent_height_packet(1);
        let bytes = req.bytes();
        assert_eq!(bytes[0], CMD_SET_EEPROM);
        assert_eq!(bytes[2..4], EEPROM_SILENT_HEIGHT.to_be_bytes());
        assert_eq!(bytes[5], 1);
        assert_eq!(bytes[6], 84);
        assert!(req.has_valid_outgoing_checksum());
    }

    #[test]
    fn test_read_requests_count_and_checksums() {
        let list = read_requests();
        assert_eq!(list.len(), 19);
        for req in &list {
            assert!(req.has_valid_outgoing_checksum());
        }
    }

    #[test]
    fn test_ui_rate_mapping() {
        assert_eq!(ui_rate_to_hardware(0), 8);
        assert_eq!(ui_rate_to_hardware(3), 1);
        assert_eq!(hardware_rate_to_ui(8), 0);
        assert_eq!(hardware_rate_to_ui(1), 3);
    }
}
