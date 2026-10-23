//! Pure mapping and filtering logic for the Windows input backend.
//!
//! The operating-system backend is available only when targeting Windows.

#[cfg(windows)]
mod windows_backend;

#[cfg(windows)]
pub use windows_backend::{
    GlobalStopShortcutManager, ListenerError, RawInputEvent, RawInputListener,
    WindowsInputBackend,
};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_ENTRIES: usize = 1_000;
const MAX_LOG_LINE_BYTES: usize = 16 * 1024;

const TARGET_VID: &str = "VID_373B";
const TARGET_PID: &str = "PID_1085";

const RI_LEFT_DOWN: u32 = 0x0001;
const RI_LEFT_UP: u32 = 0x0002;
const RI_RIGHT_DOWN: u32 = 0x0004;
const RI_RIGHT_UP: u32 = 0x0008;
const RI_MIDDLE_DOWN: u32 = 0x0010;
const RI_MIDDLE_UP: u32 = 0x0020;
const RI_BUTTON4_DOWN: u32 = 0x0040;
const RI_BUTTON4_UP: u32 = 0x0080;
const RI_BUTTON5_DOWN: u32 = 0x0100;
const RI_BUTTON5_UP: u32 = 0x0200;

const MOUSE_MOVE: u32 = 0x0001;
const MOUSE_LEFT_DOWN: u32 = 0x0002;
const MOUSE_LEFT_UP: u32 = 0x0004;
const MOUSE_RIGHT_DOWN: u32 = 0x0008;
const MOUSE_RIGHT_UP: u32 = 0x0010;
const MOUSE_MIDDLE_DOWN: u32 = 0x0020;
const MOUSE_MIDDLE_UP: u32 = 0x0040;
const MOUSE_X_DOWN: u32 = 0x0080;
const MOUSE_X_UP: u32 = 0x0100;
const MOUSE_WHEEL: u32 = 0x0800;
const MOUSE_HWHEEL: u32 = 0x1000;
const MOUSE_ABSOLUTE: u32 = 0x8000;
const MOUSE_VIRTUALDESK: u32 = 0x4000;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

pub(crate) const RI_MOUSE_WHEEL: u16 = 0x0400;
pub(crate) const RI_MOUSE_HWHEEL: u16 = 0x0800;
pub(crate) const RI_KEY_BREAK: u16 = 0x0001;
pub(crate) const RI_KEY_E0: u16 = 0x0002;
pub(crate) const RI_KEY_E1: u16 = 0x0004;
pub(crate) const INPUT_INJECTION_TAG: u32 = 0x4154_4B48; // "ATKH"
pub(crate) const MAX_TYPED_TEXT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_RAW_INPUT_BYTES: usize = 64 * 1024;

const VK_LBUTTON: u16 = 0x0001;
const VK_RBUTTON: u16 = 0x0002;
const VK_MBUTTON: u16 = 0x0004;
const VK_XBUTTON1: u16 = 0x0005;
const VK_XBUTTON2: u16 = 0x0006;

/// A button transition translated into the button numbering exposed to G HUB scripts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MouseButtonTransition {
    /// G HUB button number: 1 left, 2 middle, 3 right, 4 X1, 5 X2.
    pub button: u8,
    pub pressed: bool,
}

impl MouseButtonTransition {
    /// G HUB-compatible event name for this transition.
    pub const fn event_name(self) -> &'static str {
        if self.pressed {
            "MOUSE_BUTTON_PRESSED"
        } else {
            "MOUSE_BUTTON_RELEASED"
        }
    }

    /// G HUB event family used for mouse button transitions.
    pub const fn family(self) -> &'static str {
        "mouse"
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MouseInputSpec {
    pub dx: i32,
    pub dy: i32,
    pub data: u32,
    pub flags: u32,
}

pub(crate) fn mouse_button_input(button: u8, pressed: bool) -> Result<MouseInputSpec, &'static str> {
    let (flags, data) = match (button, pressed) {
        (1, true) => (MOUSE_LEFT_DOWN, 0),
        (1, false) => (MOUSE_LEFT_UP, 0),
        (2, true) => (MOUSE_MIDDLE_DOWN, 0),
        (2, false) => (MOUSE_MIDDLE_UP, 0),
        (3, true) => (MOUSE_RIGHT_DOWN, 0),
        (3, false) => (MOUSE_RIGHT_UP, 0),
        (4, true) => (MOUSE_X_DOWN, XBUTTON1),
        (4, false) => (MOUSE_X_UP, XBUTTON1),
        (5, true) => (MOUSE_X_DOWN, XBUTTON2),
        (5, false) => (MOUSE_X_UP, XBUTTON2),
        _ => return Err("mouse button must be in the G HUB range 1 through 5"),
    };

    Ok(MouseInputSpec {
        dx: 0,
        dy: 0,
        data,
        flags,
    })
}

pub(crate) const fn virtual_key_for_button(button: u8) -> Option<u16> {
    match button {
        1 => Some(VK_LBUTTON),
        2 => Some(VK_MBUTTON),
        3 => Some(VK_RBUTTON),
        4 => Some(VK_XBUTTON1),
        5 => Some(VK_XBUTTON2),
        _ => None,
    }
}

pub(crate) const fn relative_move_input(x: i32, y: i32) -> MouseInputSpec {
    MouseInputSpec {
        dx: x,
        dy: y,
        data: 0,
        flags: MOUSE_MOVE,
    }
}

pub(crate) fn absolute_move_input(
    x: i32,
    y: i32,
    rect: (i32, i32, i32, i32),
) -> Result<MouseInputSpec, &'static str> {
    let (left, top, width, height) = rect;
    let x = normalize_coordinate(x, left, width)?;
    let y = normalize_coordinate(y, top, height)?;
    Ok(MouseInputSpec {
        dx: x,
        dy: y,
        data: 0,
        flags: MOUSE_MOVE | MOUSE_ABSOLUTE | MOUSE_VIRTUALDESK,
    })
}

fn normalize_coordinate(value: i32, origin: i32, extent: i32) -> Result<i32, &'static str> {
    if extent <= 0 {
        return Err("virtual desktop bounds are unavailable");
    }
    if extent == 1 {
        return Ok(0);
    }
    let max = i64::from(origin) + i64::from(extent) - 1;
    let clamped = i64::from(value).clamp(i64::from(origin), max);
    let offset = clamped - i64::from(origin);
    Ok(((offset * 65_535 + (i64::from(extent) - 1) / 2) / (i64::from(extent) - 1)) as i32)
}

pub(crate) fn wheel_input(delta: i32, horizontal: bool) -> Result<MouseInputSpec, &'static str> {
    if delta == 0 {
        return Err("wheel delta must be nonzero");
    }
    Ok(MouseInputSpec {
        dx: 0,
        dy: 0,
        data: delta as u32,
        flags: if horizontal { MOUSE_HWHEEL } else { MOUSE_WHEEL },
    })
}

pub(crate) fn key_event_flags(extended: bool, key_up: bool) -> u32 {
    (if extended { 0x0001 } else { 0 })
        | (if key_up { 0x0002 } else { 0 })
        | 0x0008
}

pub(crate) fn unicode_event_flags(key_up: bool) -> u32 {
    0x0004 | (if key_up { 0x0002 } else { 0 })
}

pub(crate) fn encode_unicode_text(text: &str) -> Result<Vec<u16>, &'static str> {
    if text.len() > MAX_TYPED_TEXT_BYTES {
        return Err("text exceeds the 65536-byte input limit");
    }
    Ok(text.encode_utf16().collect())
}

pub(crate) fn raw_wheel_deltas(button_flags: u16, button_data: u16) -> (Option<i32>, Option<i32>) {
    let delta = i32::from(button_data as i16);
    (
        (button_flags & RI_MOUSE_WHEEL != 0).then_some(delta),
        (button_flags & RI_MOUSE_HWHEEL != 0).then_some(delta),
    )
}

pub(crate) fn raw_key_parts(make_code: u16, flags: u16, extra_information: u32) -> Option<(u16, bool, bool)> {
    // KeyCode models Set-1 plus the E0 prefix. E1 (for example Pause/Break)
    // has a distinct sequence and cannot safely be represented as an E0 key.
    if extra_information == INPUT_INJECTION_TAG
        || flags & RI_KEY_E1 != 0
        || make_code == 0
        || make_code > 0x00FF
    {
        return None;
    }
    Some((
        make_code,
        flags & RI_KEY_E0 != 0,
        flags & RI_KEY_BREAK == 0,
    ))
}

pub(crate) fn raw_input_storage_capacity(
    reported_size: usize,
    header_size: usize,
    complete_struct_size: usize,
    max_size: usize,
) -> Option<usize> {
    if header_size == 0
        || complete_struct_size < header_size
        || complete_struct_size > max_size
        || reported_size < header_size
        || reported_size > max_size
    {
        return None;
    }
    Some(reported_size.max(complete_struct_size))
}

pub(crate) fn raw_input_record_is_complete(
    record_size: usize,
    record_type: u32,
    header_size: usize,
    mouse_size: usize,
    keyboard_size: usize,
) -> bool {
    let payload_size = match record_type {
        0 => mouse_size,
        1 => keyboard_size,
        _ => return false,
    };
    header_size
        .checked_add(payload_size)
        .is_some_and(|minimum| record_size >= minimum)
}

pub(crate) fn check_send_input_count(
    requested: u32,
    inserted: u32,
    zero_count_last_error: Option<u32>,
) -> Result<(), String> {
    if inserted == requested {
        Ok(())
    } else if inserted == 0 {
        Err(format!(
            "SendInput inserted 0 of {requested} input events (GetLastError=0x{:08X}; the cause is not identified by this code)",
            zero_count_last_error.unwrap_or(0)
        ))
    } else {
        Err(format!(
            "SendInput inserted {inserted} of {requested} input events; no reliable OS error code is available for this partial result"
        ))
    }
}

#[derive(Default)]
pub(crate) struct BoundedLog {
    entries: VecDeque<LogEntry>,
}

/// A single bounded Lua console entry with a process-unique sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogEntry {
    /// Stable across snapshots and distinct even when messages have equal text.
    pub id: String,
    /// Sequence used to fetch entries incrementally.
    pub sequence: u64,
    pub message: String,
}

static NEXT_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static LOG_CLEAR_GENERATION: AtomicU64 = AtomicU64::new(0);
static LOG_SESSION_TOKEN: OnceLock<String> = OnceLock::new();

/// Returns the opaque token shared by log IDs and cursors for this app process.
pub fn log_session_token() -> &'static str {
    LOG_SESSION_TOKEN.get_or_init(|| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{now:x}", std::process::id())
    })
}

/// Returns the latest assigned log sequence for the current process.
pub fn log_sequence_now() -> u64 {
    NEXT_LOG_SEQUENCE.load(Ordering::Acquire)
}

/// Returns the generation incremented whenever a backend log is cleared.
pub fn log_clear_generation() -> u64 {
    LOG_CLEAR_GENERATION.load(Ordering::Acquire)
}

/// Parsed, canonical global shortcut for stopping the active Lua script.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopShortcut {
    pub shortcut: String,
    pub(crate) modifiers: u32,
    pub(crate) virtual_key: u32,
}

/// Parses and normalizes a Windows global shortcut, rejecting reserved F12.
pub fn parse_stop_shortcut(value: &str) -> Result<StopShortcut, String> {
    let mut modifiers = 0u32;
    let mut key = None;

    for part in value.split('+').map(str::trim) {
        if part.is_empty() {
            return Err("atalho inválido: cada parte precisa conter uma tecla ou modificador".into());
        }
        let (flag, name) = match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => (0x0002, "Ctrl"),
            "alt" => (0x0001, "Alt"),
            "shift" => (0x0004, "Shift"),
            "win" | "windows" => (0x0008, "Win"),
            _ => {
                if key.is_some() {
                    return Err("atalho inválido: informe exatamente uma tecla principal".into());
                }
                key = Some(parse_shortcut_key(part)?);
                continue;
            }
        };
        if modifiers & flag != 0 {
            return Err(format!("atalho inválido: modificador {name} repetido"));
        }
        modifiers |= flag;
    }

    if modifiers == 0 {
        return Err("atalho inválido: é necessário ao menos um modificador (Ctrl, Alt, Shift ou Win)".into());
    }
    let (virtual_key, key_name) = key.ok_or_else(|| "atalho inválido: falta a tecla principal".to_owned())?;
    if virtual_key == 0x7B {
        return Err("F12 é reservado pelo Windows para depuração e não pode ser registrado como atalho global".into());
    }

    let mut canonical = String::new();
    for (flag, name) in [(0x0002, "Ctrl"), (0x0001, "Alt"), (0x0004, "Shift"), (0x0008, "Win")] {
        if modifiers & flag != 0 {
            if !canonical.is_empty() {
                canonical.push('+');
            }
            canonical.push_str(name);
        }
    }
    canonical.push('+');
    canonical.push_str(&key_name);
    Ok(StopShortcut {
        shortcut: canonical,
        modifiers,
        virtual_key,
    })
}

fn parse_shortcut_key(value: &str) -> Result<(u32, String), String> {
    let upper = value.to_ascii_uppercase();
    let mut chars = upper.chars();
    if let (Some(character), None) = (chars.next(), chars.next()) {
        if character.is_ascii_alphabetic() || character.is_ascii_digit() {
            return Ok((character as u32, character.to_string()));
        }
    }

    if let Some(number) = upper.strip_prefix('F').and_then(|digits| digits.parse::<u32>().ok()) {
        if (1..=24).contains(&number) {
            return Ok((0x70 + number - 1, format!("F{number}")));
        }
    }

    let key = match upper.as_str() {
        "SPACE" => (0x20, "Space"),
        "ENTER" | "RETURN" => (0x0D, "Enter"),
        "ESC" | "ESCAPE" => (0x1B, "Escape"),
        "TAB" => (0x09, "Tab"),
        "BACKSPACE" => (0x08, "Backspace"),
        "INSERT" => (0x2D, "Insert"),
        "DELETE" | "DEL" => (0x2E, "Delete"),
        "HOME" => (0x24, "Home"),
        "END" => (0x23, "End"),
        "PAGEUP" | "PGUP" => (0x21, "PageUp"),
        "PAGEDOWN" | "PGDN" => (0x22, "PageDown"),
        "LEFT" => (0x25, "Left"),
        "UP" => (0x26, "Up"),
        "RIGHT" => (0x27, "Right"),
        "DOWN" => (0x28, "Down"),
        _ => return Err(format!("tecla principal não suportada: {value}")),
    };
    Ok((key.0, key.1.to_owned()))
}

impl BoundedLog {
    pub(crate) fn push(&mut self, message: &str) {
        while self.entries.len() >= MAX_LOG_ENTRIES {
            self.entries.pop_front();
        }
        let sequence = NEXT_LOG_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
        let session = log_session_token();
        self.entries.push_back(LogEntry {
            id: format!("{session}:{sequence}"),
            sequence,
            message: bounded_log_line(message),
        });
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        LOG_CLEAR_GENERATION.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.entries.iter().map(|entry| entry.message.clone()).collect()
    }

    pub(crate) fn entries_since(&self, sequence: u64) -> Vec<LogEntry> {
        self.entries
            .iter()
            .skip_while(|entry| entry.sequence <= sequence)
            .cloned()
            .collect()
    }

    pub(crate) fn bounds(&self) -> (Option<u64>, Option<u64>) {
        (
            self.entries.front().map(|entry| entry.sequence),
            self.entries.back().map(|entry| entry.sequence),
        )
    }
}

fn bounded_log_line(message: &str) -> String {
    if message.len() <= MAX_LOG_LINE_BYTES {
        return message.to_owned();
    }

    const MARKER: &str = "…";
    let mut end = MAX_LOG_LINE_BYTES - MARKER.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    let mut line = String::with_capacity(MAX_LOG_LINE_BYTES);
    line.push_str(&message[..end]);
    line.push_str(MARKER);
    line
}

/// Translates the button-status bits from a RAWMOUSE record to G HUB button IDs.
///
/// The output order is deterministic and follows G HUB's 1-through-5 numbering.
pub(crate) fn raw_mouse_button_transitions(flags: u32) -> Vec<MouseButtonTransition> {
    let mappings = [
        (1, RI_LEFT_DOWN, RI_LEFT_UP),
        (2, RI_MIDDLE_DOWN, RI_MIDDLE_UP),
        (3, RI_RIGHT_DOWN, RI_RIGHT_UP),
        (4, RI_BUTTON4_DOWN, RI_BUTTON4_UP),
        (5, RI_BUTTON5_DOWN, RI_BUTTON5_UP),
    ];
    let mut events = Vec::with_capacity(5);
    for (button, down, up) in mappings {
        if flags & down != 0 {
            events.push(MouseButtonTransition {
                button,
                pressed: true,
            });
        }
        if flags & up != 0 {
            events.push(MouseButtonTransition {
                button,
                pressed: false,
            });
        }
    }
    events
}

pub(crate) fn is_target_device_path(path: &str) -> bool {
    let path = path.to_ascii_uppercase();
    path.contains(TARGET_VID) && path.contains(TARGET_PID)
}

pub(crate) fn should_capture_device(handle_is_non_null: bool, device_path: Option<&str>) -> bool {
    handle_is_non_null && device_path.is_some_and(is_target_device_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_press_and_release_builders_match_the_g_hub_numbering() {
        let expected = [
            (MOUSE_LEFT_DOWN, MOUSE_LEFT_UP, 0),
            (MOUSE_MIDDLE_DOWN, MOUSE_MIDDLE_UP, 0),
            (MOUSE_RIGHT_DOWN, MOUSE_RIGHT_UP, 0),
            (MOUSE_X_DOWN, MOUSE_X_UP, XBUTTON1),
            (MOUSE_X_DOWN, MOUSE_X_UP, XBUTTON2),
        ];

        for (index, (down_flags, up_flags, data)) in expected.into_iter().enumerate() {
            let button = (index + 1) as u8;
            let down = mouse_button_input(button, true).expect("valid button press");
            let up = mouse_button_input(button, false).expect("valid button release");
            assert_eq!(down.flags, down_flags);
            assert_eq!(up.flags, up_flags);
            assert_eq!(down.data, data);
            assert_eq!(up.data, data);
            assert_eq!((down.dx, down.dy, up.dx, up.dy), (0, 0, 0, 0));
        }
    }

    #[test]
    fn button_builder_rejects_numbers_outside_the_ghub_range() {
        assert!(mouse_button_input(0, true).is_err());
        assert!(mouse_button_input(6, false).is_err());
    }

    #[test]
    fn send_input_count_check_reports_complete_partial_and_zero_results() {
        assert!(check_send_input_count(1, 1, None).is_ok());
        assert_eq!(
            check_send_input_count(2, 1, Some(5)),
            Err("SendInput inserted 1 of 2 input events; no reliable OS error code is available for this partial result".into())
        );
        assert_eq!(
            check_send_input_count(1, 0, Some(0)),
            Err("SendInput inserted 0 of 1 input events (GetLastError=0x00000000; the cause is not identified by this code)".into())
        );
    }

    #[test]
    fn bounded_log_evicts_old_lines_and_truncates_valid_utf8() {
        let long = format!("{}🚀", "x".repeat(MAX_LOG_LINE_BYTES));
        let mut log = BoundedLog::default();
        log.push(&long);
        assert!(log.entries[0].message.len() <= MAX_LOG_LINE_BYTES);
        assert!(log.entries[0].message.is_char_boundary(log.entries[0].message.len()));
        assert!(log.entries[0].message.ends_with('…'));

        for index in 0..=MAX_LOG_ENTRIES {
            log.push(&format!("line {index}"));
        }
        assert_eq!(log.entries.len(), MAX_LOG_ENTRIES);
        assert_eq!(log.entries.front().map(|entry| entry.message.as_str()), Some("line 1"));
        assert_eq!(log.entries.back().map(|entry| entry.message.as_str()), Some("line 1000"));
        log.clear();
        assert!(log.entries.is_empty());
    }

    #[test]
    fn bounded_log_incremental_reads_keep_unique_ids_and_detect_rotation_bounds() {
        let mut log = BoundedLog::default();
        log.push("same");
        let first = log.entries.back().unwrap().clone();
        log.push("same");
        let second = log.entries.back().unwrap().clone();
        assert_ne!(first.id, second.id);
        assert_eq!(log.entries_since(first.sequence), vec![second.clone()]);
        assert_eq!(log.bounds(), (Some(first.sequence), Some(second.sequence)));

        for index in 0..MAX_LOG_ENTRIES {
            log.push(&format!("rotation {index}"));
        }
        let (oldest, latest) = log.bounds();
        assert!(oldest.unwrap() > first.sequence);
        assert_eq!(latest, Some(log.entries.back().unwrap().sequence));
        assert_eq!(log.entries_since(latest.unwrap()), Vec::<LogEntry>::new());
    }

    #[test]
    fn clearing_a_bounded_log_advances_its_cursor_generation() {
        let mut log = BoundedLog::default();
        log.push("before clear");
        let generation = log_clear_generation();
        log.clear();
        assert!(log_clear_generation() > generation);
        assert!(log.entries.is_empty());
    }

    #[test]
    fn stop_shortcut_parser_normalizes_supported_combinations_and_rejects_f12() {
        let shortcut = parse_stop_shortcut("shift + ALT + ctrl + f11").unwrap();
        assert_eq!(shortcut.shortcut, "Ctrl+Alt+Shift+F11");
        assert_eq!(shortcut.modifiers, 0x0002 | 0x0001 | 0x0004);
        assert_eq!(shortcut.virtual_key, 0x7A);
        assert!(parse_stop_shortcut("Ctrl+Alt+F12").unwrap_err().contains("reservado"));
        assert!(parse_stop_shortcut("F11").is_err());
        assert!(parse_stop_shortcut("Ctrl+Ctrl+F11").is_err());
        assert!(parse_stop_shortcut("Ctrl+F11+F10").is_err());
    }

    #[test]
    fn virtual_key_mapping_uses_physical_win32_mouse_buttons() {
        assert_eq!(
            (1..=5).map(virtual_key_for_button).collect::<Vec<_>>(),
            vec![
                Some(VK_LBUTTON),
                Some(VK_MBUTTON),
                Some(VK_RBUTTON),
                Some(VK_XBUTTON1),
                Some(VK_XBUTTON2),
            ]
        );
        assert_eq!(virtual_key_for_button(0), None);
        assert_eq!(virtual_key_for_button(6), None);
    }

    #[test]
    fn relative_move_builder_keeps_signed_deltas_and_does_not_set_absolute_flag() {
        let movement = relative_move_input(-19, 7);
        assert_eq!((movement.dx, movement.dy), (-19, 7));
        assert_eq!(movement.data, 0);
        assert_eq!(movement.flags, MOUSE_MOVE);
        assert_eq!(movement.flags & 0x8000, 0);
    }

    #[test]
    fn new_api_absolute_move_normalizes_virtual_desktop_pixels_and_clamps_edges() {
        let bounds = (-1920, -200, 4480, 1440);
        let top_left = absolute_move_input(-1920, -200, bounds).unwrap();
        let bottom_right = absolute_move_input(2559, 1239, bounds).unwrap();
        let clamped = absolute_move_input(i32::MAX, i32::MIN, bounds).unwrap();
        assert_eq!((top_left.dx, top_left.dy), (0, 0));
        assert_eq!((bottom_right.dx, bottom_right.dy), (65_535, 65_535));
        assert_eq!((clamped.dx, clamped.dy), (65_535, 0));
        assert_eq!(top_left.flags, MOUSE_MOVE | MOUSE_ABSOLUTE | MOUSE_VIRTUALDESK);
        assert!(absolute_move_input(0, 0, (0, 0, 0, 10)).is_err());
        assert_eq!(absolute_move_input(7, 9, (7, 9, 1, 1)).unwrap().dx, 0);
    }

    #[test]
    fn new_api_wheel_builders_keep_signed_delta_and_direction_flag() {
        let vertical = wheel_input(-120, false).unwrap();
        let horizontal = wheel_input(60, true).unwrap();
        assert_eq!(vertical.data, (-120i32) as u32);
        assert_eq!(vertical.flags, MOUSE_WHEEL);
        assert_eq!(horizontal.data, 60);
        assert_eq!(horizontal.flags, MOUSE_HWHEEL);
        assert!(wheel_input(0, false).is_err());
        assert_eq!(raw_wheel_deltas(RI_MOUSE_WHEEL, (-120i16) as u16), (Some(-120), None));
        assert_eq!(raw_wheel_deltas(RI_MOUSE_HWHEEL, 60), (None, Some(60)));
    }

    #[test]
    fn new_api_keyboard_flags_preserve_scan_code_extended_and_release_bits() {
        assert_eq!(key_event_flags(false, false), 0x0008);
        assert_eq!(key_event_flags(true, false), 0x0009);
        assert_eq!(key_event_flags(true, true), 0x000B);
        assert_eq!(unicode_event_flags(false), 0x0004);
        assert_eq!(unicode_event_flags(true), 0x0006);
        assert_eq!(raw_key_parts(0x1D, 0, 0), Some((0x1D, false, true)));
        assert_eq!(raw_key_parts(0x1D, RI_KEY_E0, 0), Some((0x1D, true, true)));
        assert_eq!(raw_key_parts(0x1D, RI_KEY_BREAK, 0), Some((0x1D, false, false)));
        assert_eq!(raw_key_parts(0x1D, 0, INPUT_INJECTION_TAG), None);
        assert_eq!(raw_key_parts(0x1D, RI_KEY_E1, 0), None);
        assert_eq!(raw_key_parts(0, 0, 0), None);
    }

    #[test]
    fn new_api_raw_input_lengths_accept_short_keyboard_records_and_reject_truncation() {
        let header = 24;
        let mouse = 20;
        let keyboard = 16;
        let full_struct = 48;

        assert_eq!(
            raw_input_storage_capacity(40, header, full_struct, MAX_RAW_INPUT_BYTES),
            Some(full_struct)
        );
        assert_eq!(
            raw_input_storage_capacity(header - 1, header, full_struct, MAX_RAW_INPUT_BYTES),
            None
        );
        assert_eq!(
            raw_input_storage_capacity(MAX_RAW_INPUT_BYTES + 1, header, full_struct, MAX_RAW_INPUT_BYTES),
            None
        );
        assert_eq!(
            raw_input_storage_capacity(40, header, MAX_RAW_INPUT_BYTES + 1, MAX_RAW_INPUT_BYTES),
            None
        );

        assert!(raw_input_record_is_complete(40, 1, header, mouse, keyboard));
        assert!(!raw_input_record_is_complete(39, 1, header, mouse, keyboard));
        assert!(raw_input_record_is_complete(44, 0, header, mouse, keyboard));
        assert!(!raw_input_record_is_complete(43, 0, header, mouse, keyboard));
        assert!(!raw_input_record_is_complete(48, 2, header, mouse, keyboard));
        assert!(!raw_input_record_is_complete(usize::MAX, 1, usize::MAX, mouse, keyboard));
    }

    #[test]
    fn new_api_unicode_text_encodes_utf16_pairs_and_enforces_the_byte_limit() {
        assert_eq!(
            encode_unicode_text("Aé🎯").unwrap(),
            vec![0x0041, 0x00E9, 0xD83C, 0xDFAF]
        );
        assert_eq!(
            encode_unicode_text(&"a".repeat(MAX_TYPED_TEXT_BYTES))
                .unwrap()
                .len(),
            MAX_TYPED_TEXT_BYTES
        );
        assert!(encode_unicode_text(&"a".repeat(MAX_TYPED_TEXT_BYTES + 1)).is_err());
        assert!(encode_unicode_text(&"é".repeat(MAX_TYPED_TEXT_BYTES / 2 + 1)).is_err());
    }

    #[test]
    fn raw_mouse_flags_translate_left_middle_right_and_side_buttons() {
        let flags = RI_LEFT_DOWN
            | RI_MIDDLE_UP
            | RI_RIGHT_DOWN
            | RI_BUTTON4_DOWN
            | RI_BUTTON5_UP;
        assert_eq!(
            raw_mouse_button_transitions(flags),
            vec![
                MouseButtonTransition { button: 1, pressed: true },
                MouseButtonTransition { button: 2, pressed: false },
                MouseButtonTransition { button: 3, pressed: true },
                MouseButtonTransition { button: 4, pressed: true },
                MouseButtonTransition { button: 5, pressed: false },
            ]
        );
    }

    #[test]
    fn capture_filter_requires_non_null_handle_and_both_target_ids() {
        let target = r"\\?\HID#VID_373B&PID_1085&MI_01&COL01#local";
        assert!(should_capture_device(true, Some(target)));
        assert!(should_capture_device(
            true,
            Some(r"\\?\hid#vid_373b&pid_1085&mi_01#case-insensitive")
        ));
        assert!(!should_capture_device(false, Some(target)));
        assert!(!should_capture_device(true, None));
        assert!(!should_capture_device(
            true,
            Some(r"\\?\HID#VID_373B&PID_0001#different-product")
        ));
        assert!(!should_capture_device(
            true,
            Some(r"\\?\HID#VID_0001&PID_1085#different-vendor")
        ));
    }

    #[test]
    fn mock_capture_delivery_emits_only_target_mouse_button_events() {
        let transitions = if should_capture_device(
            true,
            Some(r"\\?\HID#VID_373B&PID_1085&MI_01&COL01#fixture"),
        ) {
            raw_mouse_button_transitions(RI_LEFT_DOWN | RI_BUTTON4_UP)
        } else {
            Vec::new()
        };
        let delivered = transitions
            .into_iter()
            .map(|transition| (
                transition.event_name(),
                transition.button,
                transition.family(),
            ))
            .collect::<Vec<_>>();
        assert_eq!(
            delivered,
            vec![
                ("MOUSE_BUTTON_PRESSED", 1, "mouse"),
                ("MOUSE_BUTTON_RELEASED", 4, "mouse"),
            ]
        );

        assert!(raw_mouse_button_transitions(RI_LEFT_DOWN).len() == 1);
        assert!(should_capture_device(
            true,
            Some(r"\\?\HID#VID_373B&PID_1085#fixture")
        ));
        assert!(!should_capture_device(
            false,
            Some(r"\\?\HID#VID_373B&PID_1085#fixture")
        ));
    }
}
