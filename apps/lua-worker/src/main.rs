use ghub_lua::{
    BackendError, GHubRuntime, InputBackend, InputCapabilities, KeyCode, MousePosition,
    RuntimeConfig, ScriptEvent, ScriptExit, ScreenRect,
};
use lua_process::protocol::{
    bounded_text, read_bounded_line, BackendOperation, BackendValue, HostMessage, WireEvent,
    WireScriptExit, WorkerMessage, MAX_BACKEND_CALLS, MAX_ERROR_BYTES, MAX_FRAME_BYTES,
    MAX_HOST_COMMANDS, MAX_LOG_BYTES, MAX_TYPED_TEXT_BYTES, validate_event,
};
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

fn main() {
    if let Err(error) = run() {
        eprintln!("lua-worker: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let start = read_initial_message().map_err(|error| error.to_string())?;
    let HostMessage::Start {
        source,
        runtime,
        backend_call_timeout_ms,
    } = start
    else {
        send_message(&stdout_writer(), &WorkerMessage::StartupError {
            message: "first host frame must be start".to_owned(),
        })
        .map_err(|error| error.to_string())?;
        return Err("first host frame must be start".to_owned());
    };

    let capabilities = runtime.capabilities;
    let runtime_config = RuntimeConfig::from(runtime);
    if source.len() > lua_process::protocol::MAX_SOURCE_BYTES {
        let message = "Lua source exceeds the configured IPC limit".to_owned();
        send_message(&stdout_writer(), &WorkerMessage::StartupError {
            message: message.clone(),
        })
        .map_err(|error| error.to_string())?;
        return Err(message);
    }
    if backend_call_timeout_ms == 0 {
        let message = "backend call timeout must be greater than zero".to_owned();
        send_message(&stdout_writer(), &WorkerMessage::StartupError {
            message: message.clone(),
        })
        .map_err(|error| error.to_string())?;
        return Err(message);
    }

    let output = stdout_writer();
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let stop_requested = Arc::new(AtomicBool::new(false));
    let (inbound_tx, inbound_rx) = mpsc::sync_channel(MAX_HOST_COMMANDS);
    let router_pending = Arc::clone(&pending);
    let router_output = Arc::clone(&output);
    let router_stop = Arc::clone(&stop_requested);
    thread::Builder::new()
        .name("lua-worker-ipc-reader".to_owned())
        .spawn(move || {
            route_host_input(
                router_pending,
                inbound_tx,
                router_output,
                router_stop,
            )
        })
        .map_err(|error| error.to_string())?;

    let backend = Arc::new(IpcBackend::new(
        output.clone(),
        pending,
        Duration::from_millis(backend_call_timeout_ms),
        capabilities,
    ));
    let runtime = GHubRuntime::with_config(backend, runtime_config);
    let mut handle = match runtime.start(source) {
        Ok(handle) => handle,
        Err(error) => {
            let message = bounded_text(&error.to_string(), MAX_ERROR_BYTES);
            send_message(&output, &WorkerMessage::StartupError {
                message: message.clone(),
            })
            .map_err(|io_error| io_error.to_string())?;
            return Err(message);
        }
    };

    if let Err(error) = handle.wait_until_ready(Duration::from_secs(60)) {
        let message = bounded_text(&error.to_string(), MAX_ERROR_BYTES);
        send_message(&output, &WorkerMessage::StartupError {
            message: message.clone(),
        })
        .map_err(|io_error| io_error.to_string())?;
        // wait_until_ready reports compile/top-level failures. Join runs the
        // runtime's cleanup before returning to the parent.
        let _ = handle.join();
        return Err(message);
    }

    send_message(&output, &WorkerMessage::Ready).map_err(|error| error.to_string())?;
    loop {
        if stop_requested.swap(false, Ordering::AcqRel) {
            let report = handle.stop().map_err(|error| error.to_string())?;
            send_finished(&output, report.exit, report.cleanup_errors)?;
            return Ok(());
        }
        if let Some(report) = handle.try_join().map_err(|error| error.to_string())? {
            send_finished(&output, report.exit, report.cleanup_errors)?;
            return Ok(());
        }
        match inbound_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Inbound::Event(event)) => {
                let event = ScriptEvent::from(event);
                if let Err(error) = handle.dispatch_event(event) {
                    send_notice(&output, &error.to_string());
                    if matches!(error, ghub_lua::RuntimeError::WorkerStopped) {
                        let report = handle.join().map_err(|join_error| join_error.to_string())?;
                        send_finished(&output, report.exit, report.cleanup_errors)?;
                        return Ok(());
                    }
                }
            }
            Ok(Inbound::Stop) => {
                let report = handle.stop().map_err(|error| error.to_string())?;
                send_finished(&output, report.exit, report.cleanup_errors)?;
                return Ok(());
            }
            Ok(Inbound::Eof) => {
                let report = handle.stop().map_err(|error| error.to_string())?;
                send_finished(&output, report.exit, report.cleanup_errors)?;
                return Ok(());
            }
            Ok(Inbound::Invalid(message)) => {
                send_notice(&output, &message);
                let report = handle.stop().map_err(|error| error.to_string())?;
                send_finished(&output, report.exit, report.cleanup_errors)?;
                return Ok(());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let report = handle.stop().map_err(|error| error.to_string())?;
                send_finished(&output, report.exit, report.cleanup_errors)?;
                return Ok(());
            }
        }
    }
}

fn read_initial_message() -> io::Result<HostMessage> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let line = read_bounded_line(&mut input, MAX_FRAME_BYTES)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing start frame"))?;
    serde_json::from_slice(&line)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

enum Inbound {
    Event(WireEvent),
    Stop,
    Eof,
    Invalid(String),
}

type PendingResponses = Arc<Mutex<HashMap<u64, SyncSender<CallResponse>>>>;

struct CallResponse {
    ok: bool,
    value: Option<BackendValue>,
    error: Option<String>,
}

fn route_host_input(
    pending: PendingResponses,
    inbound: SyncSender<Inbound>,
    output: Arc<Mutex<BufWriter<io::Stdout>>>,
    stop_requested: Arc<AtomicBool>,
) {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    loop {
        let line = match read_bounded_line(&mut reader, MAX_FRAME_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => {
                stop_requested.store(true, Ordering::Release);
                let _ = inbound.try_send(Inbound::Eof);
                return;
            }
            Err(error) => {
                let _ = inbound.try_send(Inbound::Invalid(error.to_string()));
                return;
            }
        };
        let message: HostMessage = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(error) => {
                let _ = inbound.try_send(Inbound::Invalid(format!("invalid host JSON: {error}")));
                return;
            }
        };
        match message {
            HostMessage::CallResult { id, ok, value, error } => {
                if let Some(reply) = pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id)
                {
                    let _ = reply.send(CallResponse { ok, value, error });
                }
            }
            HostMessage::Event { event } => {
                if let Err(error) = validate_event(&event) {
                    send_notice(&output, error);
                } else if inbound.try_send(Inbound::Event(event)).is_err() {
                    send_notice(&output, "event queue is full; event was discarded");
                }
            }
            HostMessage::Stop => {
                stop_requested.store(true, Ordering::Release);
                let _ = inbound.try_send(Inbound::Stop);
            }
            HostMessage::Start { .. } => {
                let _ = inbound.try_send(Inbound::Invalid(
                    "start frame is only valid as the first host message".to_owned(),
                ));
                return;
            }
        }
    }
}

struct IpcBackend {
    output: Arc<Mutex<BufWriter<io::Stdout>>>,
    pending: PendingResponses,
    next_id: AtomicU64,
    timeout: Duration,
    capabilities: InputCapabilities,
}

impl IpcBackend {
    fn new(
        output: Arc<Mutex<BufWriter<io::Stdout>>>,
        pending: PendingResponses,
        timeout: Duration,
        capabilities: InputCapabilities,
    ) -> Self {
        Self {
            output,
            pending,
            next_id: AtomicU64::new(1),
            timeout,
            capabilities,
        }
    }

    fn call(&self, operation: BackendOperation) -> Result<Option<BackendValue>, BackendError> {
        let operation_bytes = serde_json::to_vec(&operation)
            .map_err(|error| BackendError(error.to_string()))?;
        if operation_bytes.len() > MAX_TYPED_TEXT_BYTES + 128 {
            return Err(BackendError("backend call exceeds the IPC limit".to_owned()));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == 0 || id == u64::MAX {
            return Err(BackendError("backend call id space exhausted".to_owned()));
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let mut pending = self.pending.lock().unwrap_or_else(|poison| poison.into_inner());
            if pending.len() >= MAX_BACKEND_CALLS {
                return Err(BackendError("too many pending backend calls".to_owned()));
            }
            pending.insert(id, reply_tx);
        }

        let sent = {
            let mut writer = self.output.lock().unwrap_or_else(|poison| poison.into_inner());
            send_message_locked(&mut *writer, &WorkerMessage::InputCall { id, operation })
        };
        if let Err(error) = sent {
            self.pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&id);
            return Err(BackendError(format!("could not send input request: {error}")));
        }

        match reply_rx.recv_timeout(self.timeout) {
            Ok(response) if response.ok => Ok(response.value),
            Ok(response) => Err(BackendError(
                response
                    .error
                    .unwrap_or_else(|| "parent rejected backend request".to_owned()),
            )),
            Err(RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id);
                Err(BackendError("parent did not answer backend request in time".to_owned()))
            }
            Err(RecvTimeoutError::Disconnected) => Err(BackendError(
                "parent input broker disconnected".to_owned(),
            )),
        }
    }
}

impl InputBackend for IpcBackend {
    fn press_mouse_button(&self, button: u8) -> Result<(), BackendError> {
        self.call(BackendOperation::PressMouseButton { button }).map(|_| ())
    }

    fn release_mouse_button(&self, button: u8) -> Result<(), BackendError> {
        self.call(BackendOperation::ReleaseMouseButton { button }).map(|_| ())
    }

    fn is_mouse_button_pressed(&self, button: u8) -> Result<bool, BackendError> {
        self.call(BackendOperation::IsMouseButtonPressed { button })?
            .and_then(|value| match value { BackendValue::Bool(value) => Some(value), _ => None })
            .ok_or_else(|| BackendError("parent omitted or mistyped pressed-state result".to_owned()))
    }

    fn move_mouse_relative(&self, x: i32, y: i32) -> Result<(), BackendError> {
        self.call(BackendOperation::MoveMouseRelative { x, y }).map(|_| ())
    }

    fn output_log_message(&self, message: &str) -> Result<(), BackendError> {
        if message.len() > MAX_LOG_BYTES {
            return Err(BackendError("Lua log entry exceeds the IPC limit".to_owned()));
        }
        self.call(BackendOperation::OutputLogMessage {
            message: message.to_owned(),
        })
        .map(|_| ())
    }

    fn clear_log(&self) -> Result<(), BackendError> {
        self.call(BackendOperation::ClearLog).map(|_| ())
    }

    fn press_key(&self, key: KeyCode) -> Result<(), BackendError> {
        self.call(BackendOperation::PressKey { key }).map(|_| ())
    }

    fn release_key(&self, key: KeyCode) -> Result<(), BackendError> {
        self.call(BackendOperation::ReleaseKey { key }).map(|_| ())
    }

    fn is_key_pressed(&self, key: KeyCode) -> Result<bool, BackendError> {
        self.call(BackendOperation::IsKeyPressed { key })?
            .and_then(|value| match value { BackendValue::Bool(value) => Some(value), _ => None })
            .ok_or_else(|| BackendError("parent omitted or mistyped key-state result".to_owned()))
    }

    fn is_key_lock_on(&self, lock: &str) -> Result<bool, BackendError> {
        self.call(BackendOperation::IsKeyLockOn { lock: lock.to_owned() })?
            .and_then(|value| match value { BackendValue::Bool(value) => Some(value), _ => None })
            .ok_or_else(|| BackendError("parent omitted or mistyped lock-state result".to_owned()))
    }

    fn type_text(&self, text: &str) -> Result<(), BackendError> {
        if text.len() > MAX_TYPED_TEXT_BYTES {
            return Err(BackendError("Unicode text exceeds the IPC limit".to_owned()));
        }
        self.call(BackendOperation::TypeText { text: text.to_owned() }).map(|_| ())
    }

    fn move_mouse_to(&self, x: i32, y: i32) -> Result<(), BackendError> {
        self.call(BackendOperation::MoveMouseTo { x, y }).map(|_| ())
    }

    fn mouse_position(&self) -> Result<MousePosition, BackendError> {
        match self.call(BackendOperation::MousePosition)? {
            Some(BackendValue::MousePosition(position)) => Ok(position),
            _ => Err(BackendError("parent omitted or mistyped mouse-position result".to_owned())),
        }
    }

    fn screen_rect(&self) -> Result<ScreenRect, BackendError> {
        match self.call(BackendOperation::ScreenRect)? {
            Some(BackendValue::ScreenRect(rect)) => Ok(rect),
            _ => Err(BackendError("parent omitted or mistyped screen-bounds result".to_owned())),
        }
    }

    fn scroll_mouse(&self, vertical: i32, horizontal: i32) -> Result<(), BackendError> {
        self.call(BackendOperation::ScrollMouse { vertical, horizontal }).map(|_| ())
    }

    fn enable_keyboard_events(&self, enabled: bool) -> Result<(), BackendError> {
        self.call(BackendOperation::EnableKeyboardEvents { enabled }).map(|_| ())
    }

    fn capabilities(&self) -> InputCapabilities {
        self.capabilities
    }

    fn shutdown_started(&self) -> Result<(), BackendError> {
        send_message(&self.output, &WorkerMessage::ShutdownStarting)
            .map_err(|error| BackendError(format!("could not notify parent about Lua shutdown: {error}")))
    }
}

fn stdout_writer() -> Arc<Mutex<BufWriter<io::Stdout>>> {
    Arc::new(Mutex::new(BufWriter::new(io::stdout())))
}

fn send_message(
    writer: &Arc<Mutex<BufWriter<io::Stdout>>>,
    message: &WorkerMessage,
) -> io::Result<()> {
    let mut writer = writer.lock().unwrap_or_else(|poison| poison.into_inner());
    send_message_locked(&mut *writer, message)
}

fn send_message_locked(writer: &mut impl Write, message: &WorkerMessage) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, message).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn send_notice(writer: &Arc<Mutex<BufWriter<io::Stdout>>>, message: &str) {
    let _ = send_message(
        writer,
        &WorkerMessage::Notice {
            message: bounded_text(message, MAX_ERROR_BYTES),
        },
    );
}

fn send_finished(
    writer: &Arc<Mutex<BufWriter<io::Stdout>>>,
    exit: ScriptExit,
    cleanup_errors: Vec<String>,
) -> Result<(), String> {
    let exit = match exit {
        ScriptExit::Cancelled => WireScriptExit::Cancelled,
        ScriptExit::Failed(message) => {
            WireScriptExit::Failed(bounded_text(&message, MAX_ERROR_BYTES))
        }
    };
    let cleanup_errors = cleanup_errors
        .into_iter()
        .take(32)
        .map(|error| bounded_text(&error, MAX_ERROR_BYTES))
        .collect();
    send_message(
        writer,
        &WorkerMessage::WorkerFinished {
            exit,
            cleanup_errors,
        },
    )
    .map_err(|error| error.to_string())
}
