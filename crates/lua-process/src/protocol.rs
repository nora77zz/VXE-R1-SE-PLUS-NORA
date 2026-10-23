//! Bounded JSON-lines protocol shared by the broker and `lua-worker` process.

use ghub_lua::{
    InputCapabilities, KeyCode, MousePosition, RuntimeConfig, ScreenRect, ScriptEvent,
};
use serde::{Deserialize, Serialize};

pub const MAX_SOURCE_BYTES: usize = 1_048_576;
pub const MAX_FRAME_BYTES: usize = MAX_SOURCE_BYTES + 65_536;
pub const MAX_EVENT_NAME_BYTES: usize = 256;
pub const MAX_FAMILY_BYTES: usize = 128;
pub const MAX_LOG_BYTES: usize = 8_192;
pub const MAX_TYPED_TEXT_BYTES: usize = 16_384;
pub const MAX_ERROR_BYTES: usize = 1_024;
pub const MAX_HOST_COMMANDS: usize = 128;
pub const MAX_BACKEND_CALLS: usize = 32;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    Start {
        source: String,
        runtime: WireRuntimeConfig,
        backend_call_timeout_ms: u64,
    },
    Event { event: WireEvent },
    Stop,
    CallResult {
        id: u64,
        ok: bool,
        value: Option<BackendValue>,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Ready,
    StartupError { message: String },
    InputCall { id: u64, operation: BackendOperation },
    WorkerFinished {
        exit: WireScriptExit,
        cleanup_errors: Vec<String>,
    },
    ShutdownStarting,
    Notice { message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BackendOperation {
    PressMouseButton { button: u8 },
    ReleaseMouseButton { button: u8 },
    IsMouseButtonPressed { button: u8 },
    MoveMouseRelative { x: i32, y: i32 },
    PressKey { key: KeyCode },
    ReleaseKey { key: KeyCode },
    IsKeyPressed { key: KeyCode },
    IsKeyLockOn { lock: String },
    TypeText { text: String },
    MoveMouseTo { x: i32, y: i32 },
    MousePosition,
    ScreenRect,
    ScrollMouse { vertical: i32, horizontal: i32 },
    EnableKeyboardEvents { enabled: bool },
    Capabilities,
    OutputLogMessage { message: String },
    ClearLog,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BackendValue {
    Bool(bool),
    MousePosition(MousePosition),
    ScreenRect(ScreenRect),
    Capabilities(InputCapabilities),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireRuntimeConfig {
    pub memory_limit_bytes: usize,
    pub instruction_check_interval: u32,
    pub event_queue_capacity: usize,
    #[serde(default = "default_shutdown_grace_ms")]
    pub shutdown_grace_ms: u64,
    #[serde(default)]
    pub capabilities: InputCapabilities,
}

impl From<&RuntimeConfig> for WireRuntimeConfig {
    fn from(value: &RuntimeConfig) -> Self {
        Self {
            memory_limit_bytes: value.memory_limit_bytes,
            instruction_check_interval: value.instruction_check_interval,
            event_queue_capacity: value.event_queue_capacity,
            shutdown_grace_ms: value.shutdown_grace.as_millis().min(u64::MAX as u128) as u64,
            capabilities: InputCapabilities::default(),
        }
    }
}

impl From<WireRuntimeConfig> for RuntimeConfig {
    fn from(value: WireRuntimeConfig) -> Self {
        Self {
            memory_limit_bytes: value.memory_limit_bytes,
            instruction_check_interval: value.instruction_check_interval,
            event_queue_capacity: value.event_queue_capacity,
            shutdown_grace: std::time::Duration::from_millis(value.shutdown_grace_ms),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WireEvent {
    pub name: String,
    pub arg: Option<i64>,
    pub family: Option<String>,
    #[serde(default)]
    pub key: Option<KeyCode>,
}

impl From<ScriptEvent> for WireEvent {
    fn from(value: ScriptEvent) -> Self {
        Self {
            name: value.name,
            arg: value.arg,
            family: value.family,
            key: value.key,
        }
    }
}

impl From<WireEvent> for ScriptEvent {
    fn from(value: WireEvent) -> Self {
        Self {
            name: value.name,
            arg: value.arg,
            family: value.family,
            key: value.key,
        }
    }
}

fn default_shutdown_grace_ms() -> u64 {
    700
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum WireScriptExit {
    Cancelled,
    Failed(String),
}

pub fn validate_source(source: &str) -> Result<(), &'static str> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err("Lua source exceeds the configured IPC limit");
    }
    Ok(())
}

pub fn validate_event(event: &WireEvent) -> Result<(), &'static str> {
    if event.name.is_empty() || event.name.len() > MAX_EVENT_NAME_BYTES {
        return Err("event name is empty or exceeds the IPC limit");
    }
    if event.family.as_ref().is_some_and(|family| family.len() > MAX_FAMILY_BYTES) {
        return Err("event family exceeds the IPC limit");
    }
    if event.key.is_some_and(|key| !(1..=255).contains(&key.scan_code)) {
        return Err("key scan code must be between 1 and 255");
    }
    Ok(())
}

pub fn bounded_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Reads one newline-delimited frame without allocating beyond `limit` bytes.
pub fn read_bounded_line(reader: &mut impl std::io::BufRead, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::{Error, ErrorKind};

    let mut line = Vec::with_capacity(256);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(Error::new(ErrorKind::InvalidData, "EOF before JSON line delimiter"));
        }
        let take = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => index + 1,
            None => available.len(),
        };
        if line.len().saturating_add(take) > limit {
            return Err(Error::new(ErrorKind::InvalidData, "JSON line exceeds the IPC frame limit"));
        }
        let ends_line = available.get(take.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if ends_line {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}
