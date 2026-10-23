//! Out-of-process Lua host. The process receives only an explicitly supplied
//! worker executable path; synthetic input is brokered through the parent.

pub mod protocol;

use ghub_lua::{InputBackend, RuntimeConfig, ScriptEvent};
use protocol::{
    bounded_text, read_bounded_line, validate_event, validate_source, BackendOperation,
    BackendValue, HostMessage, WireEvent,
    WireRuntimeConfig, WireScriptExit, WorkerMessage, MAX_ERROR_BYTES, MAX_FRAME_BYTES,
    MAX_LOG_BYTES,
};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HOST_COMMAND_CAPACITY: usize = protocol::MAX_HOST_COMMANDS;
const OUTPUT_QUEUE_CAPACITY: usize = 32;
const MAX_REPORT_MESSAGES: usize = 32;
const MAX_STDERR_TAIL_BYTES: usize = 8_192;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Limits all parent/child queues and bounds the stop handshake.
#[derive(Clone, Debug)]
pub struct ProcessConfig {
    pub runtime: RuntimeConfig,
    pub startup_timeout: Duration,
    pub stop_grace: Duration,
    /// Maximum time the public stop call waits for broker cleanup to finish.
    pub stop_timeout: Duration,
    pub backend_call_timeout: Duration,
    pub event_queue_capacity: usize,
}

impl Default for ProcessConfig {
    fn default() -> Self {
        Self {
            runtime: RuntimeConfig::default(),
            startup_timeout: Duration::from_secs(15),
            stop_grace: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(3),
            backend_call_timeout: Duration::from_millis(750),
            event_queue_capacity: HOST_COMMAND_CAPACITY,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessError {
    InvalidConfiguration(String),
    InvalidExecutablePath(String),
    WorkerSpawn(String),
    StartupTimeout,
    StartupFailed(String),
    EventQueueFull,
    WorkerStopped,
    StopTimeout,
    AlreadyJoined,
}

impl std::fmt::Display for ProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => write!(f, "invalid process configuration: {message}"),
            Self::InvalidExecutablePath(message) => write!(f, "invalid Lua worker executable: {message}"),
            Self::WorkerSpawn(message) => write!(f, "could not start Lua worker process: {message}"),
            Self::StartupTimeout => f.write_str("Lua worker process did not become ready in time"),
            Self::StartupFailed(message) => write!(f, "Lua worker failed to start: {message}"),
            Self::EventQueueFull => f.write_str("Lua process event queue is full"),
            Self::WorkerStopped => f.write_str("Lua worker process has stopped"),
            Self::StopTimeout => {
                f.write_str("Lua worker broker did not finish cleanup before the stop deadline")
            }
            Self::AlreadyJoined => f.write_str("Lua worker process was already joined"),
        }
    }
}

impl std::error::Error for ProcessError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessExit {
    Cancelled,
    Failed(String),
    KilledAfterGrace,
    StartupFailed(String),
    UnexpectedEof,
    ProtocolError(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessReport {
    pub exit: ProcessExit,
    pub cleanup_errors: Vec<String>,
    pub messages: Vec<String>,
    pub stderr_tail: String,
    pub forced_kill: bool,
}

/// Starts child workers and brokers their input calls through one parent-owned
/// [`InputBackend`]. Construction itself never launches the executable.
pub struct LuaProcessRuntime {
    backend: Arc<dyn InputBackend>,
    config: ProcessConfig,
}

impl LuaProcessRuntime {
    pub fn new(backend: Arc<dyn InputBackend>) -> Self {
        Self::with_config(backend, ProcessConfig::default())
    }

    pub fn with_config(backend: Arc<dyn InputBackend>, config: ProcessConfig) -> Self {
        Self { backend, config }
    }

    /// Explicitly starts the executable at `worker_executable`; no PATH lookup,
    /// shell, or implicit worker discovery is performed.
    pub fn start(
        &self,
        worker_executable: impl AsRef<Path>,
        source: impl Into<String>,
    ) -> Result<LuaProcessHandle, ProcessError> {
        validate_config(&self.config)?;
        let source = source.into();
        validate_source(&source)
            .map_err(|message| ProcessError::InvalidConfiguration(message.to_owned()))?;

        let mut runtime = WireRuntimeConfig::from(&self.config.runtime);
        runtime.shutdown_grace_ms = bounded_shutdown_grace_ms(
            runtime.shutdown_grace_ms,
            self.config.stop_grace,
        );
        runtime.capabilities = self.backend.capabilities();
        let start_message = HostMessage::Start {
            source,
            runtime,
            backend_call_timeout_ms: duration_millis(self.config.backend_call_timeout),
        };
        let encoded_start = serde_json::to_vec(&start_message)
            .map_err(|error| ProcessError::InvalidConfiguration(error.to_string()))?;
        if encoded_start.len().saturating_add(1) > MAX_FRAME_BYTES {
            return Err(ProcessError::InvalidConfiguration(
                "encoded Lua start frame exceeds the IPC limit".to_owned(),
            ));
        }

        let executable = canonical_worker_path(worker_executable.as_ref())?;
        let (command_tx, command_rx) = mpsc::sync_channel(self.config.event_queue_capacity);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (report_tx, report_rx) = mpsc::sync_channel(1);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let broker_stop_requested = Arc::clone(&stop_requested);
        let backend = Arc::clone(&self.backend);
        let config = self.config.clone();
        let worker_path = executable.clone();

        let join = thread::Builder::new()
            .name("lua-process-broker".to_owned())
            .spawn(move || {
                broker_main(
                    worker_path,
                    start_message,
                    backend,
                    config,
                    command_rx,
                    broker_stop_requested,
                    ready_tx,
                    report_tx,
                )
            })
            .map_err(|error| ProcessError::WorkerSpawn(error.to_string()))?;

        match ready_rx.recv_timeout(self.config.startup_timeout + Duration::from_millis(1000)) {
            Ok(Ok(())) => Ok(LuaProcessHandle {
                commands: command_tx,
                report: Some(report_rx),
                pending_report: None,
                join: Some(join),
                stop_timeout: self.config.stop_timeout,
                stop_requested,
            }),
            Ok(Err(message)) => {
                let _ = join.join();
                Err(ProcessError::StartupFailed(message))
            }
            Err(RecvTimeoutError::Timeout) => {
                stop_requested.store(true, Ordering::Release);
                drop(join);
                Err(ProcessError::StartupTimeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = join.join();
                Err(ProcessError::WorkerStopped)
            }
        }
    }
}

pub struct LuaProcessHandle {
    commands: SyncSender<HostCommand>,
    report: Option<Receiver<ProcessReport>>,
    pending_report: Option<ProcessReport>,
    join: Option<JoinHandle<()>>,
    stop_timeout: Duration,
    stop_requested: Arc<AtomicBool>,
}

impl LuaProcessHandle {
    /// Enqueues one bounded event for the child worker.
    pub fn dispatch(&self, event: ScriptEvent) -> Result<(), ProcessError> {
        let wire = WireEvent::from(event);
        validate_event(&wire)
            .map_err(|message| ProcessError::InvalidConfiguration(message.to_owned()))?;
        match self.commands.try_send(HostCommand::Event(wire)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(ProcessError::EventQueueFull),
            Err(TrySendError::Disconnected(_)) => Err(ProcessError::WorkerStopped),
        }
    }

    /// Requests cooperative cancellation, waits only for the configured grace
    /// period inside the broker, then kills the child and releases held buttons.
    pub fn stop(&mut self) -> Result<ProcessReport, ProcessError> {
        self.stop_requested.store(true, Ordering::Release);
        self.join_worker()
    }

    /// Checks for a completed worker report without waiting.
    ///
    /// Returns `Ok(None)` while the broker is still running or has published a
    /// report but has not finished its final teardown. Returns the report once
    /// the broker thread has finished, joining it without blocking. As with
    /// [`stop`](Self::stop), a report that was already consumed returns
    /// [`ProcessError::AlreadyJoined`].
    pub fn try_report(&mut self) -> Result<Option<ProcessReport>, ProcessError> {
        let report_rx = self.report.as_ref().ok_or(ProcessError::AlreadyJoined)?;
        if self.pending_report.is_none() {
            match report_rx.try_recv() {
                Ok(report) => self.pending_report = Some(report),
                Err(TryRecvError::Empty) => return Ok(None),
                Err(TryRecvError::Disconnected) => {
                    if self.join.as_ref().is_some_and(|join| !join.is_finished()) {
                        return Ok(None);
                    }
                    self.report.take();
                    if let Some(join) = self.join.take() {
                        let _ = join.join();
                    }
                    return Err(ProcessError::WorkerStopped);
                }
            }
        }

        if !self.join.as_ref().is_some_and(|join| join.is_finished()) {
            return Ok(None);
        }

        let report = self
            .pending_report
            .take()
            .expect("a report is pending after a successful receive");
        self.report.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        Ok(Some(report))
    }

    fn join_worker(&mut self) -> Result<ProcessReport, ProcessError> {
        let report = if let Some(report) = self.pending_report.take() {
            self.report.take().ok_or(ProcessError::AlreadyJoined)?;
            report
        } else {
            let report_rx = self.report.take().ok_or(ProcessError::AlreadyJoined)?;
            match report_rx.recv_timeout(self.stop_timeout) {
                Ok(report) => report,
                Err(RecvTimeoutError::Timeout) => {
                    // Stop waiting at the public API boundary. The broker remains
                    // detached and may still complete a slow backend cleanup.
                    drop(self.join.take());
                    return Err(ProcessError::StopTimeout);
                }
                Err(RecvTimeoutError::Disconnected) => return Err(ProcessError::WorkerStopped),
            }
        };
        if let Some(join) = self.join.take() {
            if join.is_finished() {
                let _ = join.join();
            }
        }
        Ok(report)
    }
}

impl Drop for LuaProcessHandle {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.stop_requested.store(true, Ordering::Release);
            // The broker owns the child and performs bounded cleanup after the
            // stop request; detaching avoids blocking an arbitrary Drop site.
            drop(self.join.take());
        }
    }
}

enum HostCommand {
    Event(WireEvent),
}

enum OutputFrame {
    Message(WorkerMessage),
    Eof,
    Invalid(String),
}

fn broker_main(
    worker_path: PathBuf,
    start_message: HostMessage,
    backend: Arc<dyn InputBackend>,
    config: ProcessConfig,
    commands: Receiver<HostCommand>,
    stop_requested: Arc<AtomicBool>,
    ready: SyncSender<Result<(), String>>,
    report: SyncSender<ProcessReport>,
) {
    let mut child = match spawn_worker(&worker_path) {
        Ok(child) => child,
        Err(error) => {
            let message = error.to_string();
            let _ = ready.send(Err(message.clone()));
            let _ = report.send(empty_report(ProcessExit::StartupFailed(message)));
            return;
        }
    };

    let stderr_tail = Arc::new(std::sync::Mutex::new(VecDeque::new()));
    if let Some(stderr) = child.stderr.take() {
        let tail = Arc::clone(&stderr_tail);
        let _ = thread::Builder::new()
            .name("lua-worker-stderr".to_owned())
            .spawn(move || drain_stderr(stderr, tail));
    }

    let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = ready.send(Err("worker stdout pipe was unavailable".to_owned()));
            terminate_child(&mut child);
            let _ = report.send(empty_report(ProcessExit::StartupFailed(
                "worker stdout pipe was unavailable".to_owned(),
            )));
            return;
        }
    };
    let reader = thread::Builder::new()
        .name("lua-worker-protocol-reader".to_owned())
        .spawn(move || read_worker_messages(stdout, output_tx));
    if let Err(error) = reader {
        let message = error.to_string();
        let _ = ready.send(Err(message.clone()));
        terminate_child(&mut child);
        let _ = report.send(empty_report(ProcessExit::StartupFailed(message)));
        return;
    }

    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = ready.send(Err("worker stdin pipe was unavailable".to_owned()));
            terminate_child(&mut child);
            let _ = report.send(empty_report(ProcessExit::StartupFailed(
                "worker stdin pipe was unavailable".to_owned(),
            )));
            return;
        }
    };

    if let Err(error) = write_frame(&mut stdin, &start_message) {
        let message = error.to_string();
        let _ = ready.send(Err(message.clone()));
        terminate_child(&mut child);
        let mut result = empty_report(ProcessExit::StartupFailed(message));
        release_all(&*backend, &mut BTreeSet::new(), &mut result.cleanup_errors);
        result.stderr_tail = read_stderr_tail(&stderr_tail);
        let _ = report.send(result);
        return;
    }

    let startup_deadline = Instant::now() + config.startup_timeout;
    let mut ready_sent = false;
    let mut held_inputs = BTreeSet::new();
    let mut keyboard_events_enabled = false;
    let mut messages = Vec::new();
    let mut stopping = false;
    let mut stop_deadline = None;
    let mut worker_finished: Option<(ProcessExit, Vec<String>)> = None;
    let mut terminal: Option<ProcessExit> = None;
    let mut startup_failure: Option<String> = None;
    let mut forced_kill = false;

    loop {
        begin_stop_if_requested(
            &stop_requested,
            &mut stopping,
            &mut stop_deadline,
            config.stop_grace,
            &mut stdin,
        );
        if !ready_sent && Instant::now() >= startup_deadline {
            let stderr = read_stderr_tail(&stderr_tail);
            let msg = if stderr.is_empty() {
                "startup deadline expired".to_owned()
            } else {
                format!("startup deadline expired: {stderr}")
            };
            terminal = Some(ProcessExit::StartupFailed(msg.clone()));
            let _ = ready.send(Err(msg));
            break;
        }

        if !stopping {
            match commands.try_recv() {
                Ok(HostCommand::Event(event)) => {
                    begin_stop_if_requested(
                        &stop_requested,
                        &mut stopping,
                        &mut stop_deadline,
                        config.stop_grace,
                        &mut stdin,
                    );
                    if stopping {
                        // Stop is a separate priority signal; queued events are
                        // discarded once the signal has been observed.
                    } else if let Err(message) = validate_event(&event) {
                        push_message(&mut messages, message);
                    } else if let Err(error) = write_frame(&mut stdin, &HostMessage::Event { event }) {
                        terminal = Some(ProcessExit::ProtocolError(error.to_string()));
                        break;
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    stop_requested.store(true, Ordering::Release);
                    begin_stop_if_requested(
                        &stop_requested,
                        &mut stopping,
                        &mut stop_deadline,
                        config.stop_grace,
                        &mut stdin,
                    );
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        match output_rx.recv_timeout(POLL_INTERVAL) {
            Ok(OutputFrame::Message(WorkerMessage::Ready)) => {
                if !ready_sent {
                    ready_sent = true;
                    let _ = ready.send(Ok(()));
                } else {
                    push_message(&mut messages, "duplicate ready frame");
                }
            }
            Ok(OutputFrame::Message(WorkerMessage::StartupError { message })) => {
                let message = bounded_text(&message, MAX_ERROR_BYTES);
                startup_failure = Some(message.clone());
                terminal = Some(ProcessExit::StartupFailed(message.clone()));
                if !ready_sent {
                    let _ = ready.send(Err(message));
                }
                // The runtime may have registered lifecycle callbacks before
                // a top-level error. Keep the child alive for its bounded
                // best-effort PROFILE_DEACTIVATED/atk.on_stop shutdown.
                stop_requested.store(true, Ordering::Release);
                begin_stop_if_requested(
                    &stop_requested,
                    &mut stopping,
                    &mut stop_deadline,
                    config.stop_grace,
                    &mut stdin,
                );
            }
            Ok(OutputFrame::Message(WorkerMessage::InputCall { id, operation })) => {
                begin_stop_if_requested(
                    &stop_requested,
                    &mut stopping,
                    &mut stop_deadline,
                    config.stop_grace,
                    &mut stdin,
                );
                let result = execute_operation(
                    &*backend,
                    &operation,
                    !stopping,
                    &stop_requested,
                    &mut held_inputs,
                    &mut keyboard_events_enabled,
                );
                let response = match result {
                    Ok(value) => HostMessage::CallResult {
                        id,
                        ok: true,
                        value,
                        error: None,
                    },
                    Err(error) => HostMessage::CallResult {
                        id,
                        ok: false,
                        value: None,
                        error: Some(bounded_text(&error, MAX_ERROR_BYTES)),
                    },
                };
                if let Err(error) = write_frame(&mut stdin, &response) {
                    terminal = Some(ProcessExit::ProtocolError(error.to_string()));
                    break;
                }
            }
            Ok(OutputFrame::Message(WorkerMessage::WorkerFinished {
                exit,
                cleanup_errors,
            })) => {
                let exit = match exit {
                    WireScriptExit::Cancelled => ProcessExit::Cancelled,
                    WireScriptExit::Failed(message) => {
                        ProcessExit::Failed(bounded_text(&message, MAX_ERROR_BYTES))
                    }
                };
                worker_finished = Some((exit, bounded_errors(cleanup_errors)));
                terminal = Some(startup_failure.as_ref().map_or_else(
                    || worker_finished.as_ref().unwrap().0.clone(),
                    |message| ProcessExit::StartupFailed(message.clone()),
                ));
                break;
            }
            Ok(OutputFrame::Message(WorkerMessage::Notice { message })) => {
                push_message(&mut messages, &bounded_text(&message, MAX_ERROR_BYTES));
            }
            Ok(OutputFrame::Message(WorkerMessage::ShutdownStarting)) => {
                stop_requested.store(true, Ordering::Release);
                begin_stop_if_requested(
                    &stop_requested,
                    &mut stopping,
                    &mut stop_deadline,
                    config.stop_grace,
                    &mut stdin,
                );
            }
            Ok(OutputFrame::Eof) => {
                terminal = Some(startup_failure.as_ref().map_or(
                    ProcessExit::UnexpectedEof,
                    |message| ProcessExit::StartupFailed(message.clone()),
                ));
                if !ready_sent {
                    let stderr = read_stderr_tail(&stderr_tail);
                    let msg = if stderr.is_empty() {
                        "worker closed stdout before it became ready".to_owned()
                    } else {
                        format!("worker failed to start: {stderr}")
                    };
                    let _ = ready.send(Err(msg));
                }
                break;
            }
            Ok(OutputFrame::Invalid(message)) => {
                terminal = Some(ProcessExit::ProtocolError(bounded_text(&message, MAX_ERROR_BYTES)));
                if !ready_sent {
                    let stderr = read_stderr_tail(&stderr_tail);
                    let msg = if stderr.is_empty() {
                        format!("worker sent an invalid protocol frame: {message}")
                    } else {
                        format!("worker sent an invalid protocol frame ({message}): {stderr}")
                    };
                    let _ = ready.send(Err(msg));
                }
                break;
            }
            Err(RecvTimeoutError::Disconnected) => {
                terminal = Some(ProcessExit::UnexpectedEof);
                if !ready_sent {
                    let stderr = read_stderr_tail(&stderr_tail);
                    let msg = if stderr.is_empty() {
                        "worker protocol reader stopped".to_owned()
                    } else {
                        format!("worker process stopped: {stderr}")
                    };
                    let _ = ready.send(Err(msg));
                }
                break;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        if stopping && stop_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            forced_kill = true;
            terminal = Some(ProcessExit::KilledAfterGrace);
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if worker_finished.is_none() {
                    terminal = Some(ProcessExit::UnexpectedEof);
                } else if !status.success() {
                    push_message(&mut messages, "worker exited with a nonzero status");
                }
                break;
            }
            Ok(None) => {}
            Err(error) => {
                terminal = Some(ProcessExit::ProtocolError(error.to_string()));
                break;
            }
        }
    }

    // On any abnormal end, stop accepting input before killing the child. The
    // parent tracker is authoritative even if the child died inside Lua/C.
    if forced_kill || worker_finished.is_none() {
        if !stopping {
            let _ = write_frame(&mut stdin, &HostMessage::Stop);
        }
        terminate_child(&mut child);
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }

    let mut cleanup_errors = worker_finished
        .map(|(_, errors)| errors)
        .unwrap_or_default();
    release_all(&*backend, &mut held_inputs, &mut cleanup_errors);
    if keyboard_events_enabled {
        if let Err(error) = backend.enable_keyboard_events(false) {
            cleanup_errors.push(format!("disable physical keyboard event capture: {error}"));
        }
    }
    if forced_kill {
        push_message(
            &mut messages,
            "worker was force-terminated after the shutdown deadline; PROFILE_DEACTIVATED and atk.on_stop may not have completed",
        );
    }
    let result = ProcessReport {
        exit: terminal.unwrap_or(ProcessExit::UnexpectedEof),
        cleanup_errors: bounded_errors(cleanup_errors),
        messages,
        stderr_tail: read_stderr_tail(&stderr_tail),
        forced_kill,
    };
    if !ready_sent {
        let startup_error = match &result.exit {
            ProcessExit::StartupFailed(message) | ProcessExit::ProtocolError(message) => message.clone(),
            other => format!("worker stopped during startup: {other:?}"),
        };
        let _ = ready.send(Err(startup_error));
    }
    let _ = report.send(result);
}

fn begin_stop_if_requested(
    stop_requested: &AtomicBool,
    stopping: &mut bool,
    stop_deadline: &mut Option<Instant>,
    stop_grace: Duration,
    stdin: &mut std::process::ChildStdin,
) {
    if !*stopping && stop_requested.load(Ordering::Acquire) {
        *stopping = true;
        *stop_deadline = Some(Instant::now() + stop_grace);
        let _ = write_frame(stdin, &HostMessage::Stop);
    }
}

fn execute_operation(
    backend: &dyn InputBackend,
    operation: &BackendOperation,
    allow_input: bool,
    stop_requested: &AtomicBool,
    held_inputs: &mut BTreeSet<HeldInput>,
    keyboard_events_enabled: &mut bool,
) -> Result<Option<BackendValue>, String> {
    let stopping = !allow_input || stop_requested.load(Ordering::Acquire);
    if let BackendOperation::ReleaseMouseButton { button } = operation {
        validate_button(*button)?;
        if stopping && !held_inputs.contains(&HeldInput::Mouse(*button)) {
            return Err("stopping process may release only brokered pressed buttons".to_owned());
        }
        return backend.release_mouse_button(*button).map(|()| {
            held_inputs.remove(&HeldInput::Mouse(*button));
            None
        }).map_err(|error| error.to_string());
    }
    if let BackendOperation::ReleaseKey { key } = operation {
        validate_key(*key)?;
        if stopping && !held_inputs.contains(&HeldInput::Key(*key)) {
            return Err("stopping process may release only brokered pressed keys".to_owned());
        }
        return backend.release_key(*key).map(|()| {
            held_inputs.remove(&HeldInput::Key(*key));
            None
        }).map_err(|error| error.to_string());
    }
    let safe_during_shutdown = matches!(
        operation,
        BackendOperation::OutputLogMessage { .. }
            | BackendOperation::ClearLog
            | BackendOperation::EnableKeyboardEvents { enabled: false }
            | BackendOperation::Capabilities
            | BackendOperation::IsMouseButtonPressed { .. }
            | BackendOperation::IsKeyPressed { .. }
            | BackendOperation::IsKeyLockOn { .. }
            | BackendOperation::MousePosition
            | BackendOperation::ScreenRect
    );
    if stopping && !safe_during_shutdown {
        return Err("Lua process is stopping".to_owned());
    }
    match operation {
        BackendOperation::PressMouseButton { button } => {
            validate_button(*button)?;
            backend
                .press_mouse_button(*button)
                .map(|()| {
                    held_inputs.insert(HeldInput::Mouse(*button));
                    None
                })
                .map_err(|error| error.to_string())
        }
        BackendOperation::IsMouseButtonPressed { button } => {
            validate_button(*button)?;
            backend
                .is_mouse_button_pressed(*button)
                .map(|value| Some(BackendValue::Bool(value)))
                .map_err(|error| error.to_string())
        }
        BackendOperation::MoveMouseRelative { x, y } => backend
            .move_mouse_relative(*x, *y)
            .map(|()| None)
            .map_err(|error| error.to_string()),
        BackendOperation::PressKey { key } => {
            validate_key(*key)?;
            backend.press_key(*key).map(|()| {
                held_inputs.insert(HeldInput::Key(*key));
                None
            }).map_err(|error| error.to_string())
        }
        BackendOperation::ReleaseKey { .. } => unreachable!(),
        BackendOperation::IsKeyPressed { key } => {
            validate_key(*key)?;
            backend.is_key_pressed(*key).map(|value| Some(BackendValue::Bool(value))).map_err(|error| error.to_string())
        }
        BackendOperation::IsKeyLockOn { lock } => {
            let lock = validate_lock_name(lock)?;
            backend.is_key_lock_on(lock).map(|value| Some(BackendValue::Bool(value))).map_err(|error| error.to_string())
        }
        BackendOperation::TypeText { text } => {
            if text.len() > protocol::MAX_TYPED_TEXT_BYTES {
                return Err("Unicode text exceeds the IPC limit".to_owned());
            }
            backend.type_text(text).map(|()| None).map_err(|error| error.to_string())
        }
        BackendOperation::MoveMouseTo { x, y } => backend.move_mouse_to(*x, *y).map(|()| None).map_err(|error| error.to_string()),
        BackendOperation::MousePosition => backend.mouse_position().map(|position| Some(BackendValue::MousePosition(position))).map_err(|error| error.to_string()),
        BackendOperation::ScreenRect => backend.screen_rect().map(|rect| Some(BackendValue::ScreenRect(rect))).map_err(|error| error.to_string()),
        BackendOperation::ScrollMouse { vertical, horizontal } => backend.scroll_mouse(*vertical, *horizontal).map(|()| None).map_err(|error| error.to_string()),
        BackendOperation::EnableKeyboardEvents { enabled } => backend.enable_keyboard_events(*enabled).map(|()| {
            *keyboard_events_enabled = *enabled;
            None
        }).map_err(|error| error.to_string()),
        BackendOperation::Capabilities => Ok(Some(BackendValue::Capabilities(backend.capabilities()))),
        BackendOperation::OutputLogMessage { message } => {
            if message.len() > MAX_LOG_BYTES {
                return Err("Lua log entry exceeds the IPC limit".to_owned());
            }
            backend
                .output_log_message(message)
                .map(|()| None)
                .map_err(|error| error.to_string())
        }
        BackendOperation::ClearLog => backend
            .clear_log()
            .map(|()| None)
            .map_err(|error| error.to_string()),
        BackendOperation::ReleaseMouseButton { .. } => unreachable!(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum HeldInput {
    Mouse(u8),
    Key(ghub_lua::KeyCode),
}

fn validate_button(button: u8) -> Result<(), String> {
    if (1..=5).contains(&button) {
        Ok(())
    } else {
        Err("mouse button must be in the G HUB range 1 through 5".to_owned())
    }
}

fn validate_key(key: ghub_lua::KeyCode) -> Result<(), String> {
    if (1..=255).contains(&key.scan_code) {
        Ok(())
    } else {
        Err("key scan code must be between 1 and 255".to_owned())
    }
}

fn validate_lock_name(lock: &str) -> Result<&'static str, String> {
    match lock.to_ascii_lowercase().replace('_', "").as_str() {
        "capslock" => Ok("capslock"),
        "numlock" => Ok("numlock"),
        "scrolllock" => Ok("scrolllock"),
        _ => Err("lock name must be capslock, numlock, or scrolllock".to_owned()),
    }
}

fn release_all(
    backend: &dyn InputBackend,
    held_inputs: &mut BTreeSet<HeldInput>,
    errors: &mut Vec<String>,
) {
    let inputs: Vec<HeldInput> = held_inputs.iter().copied().collect();
    for input in inputs {
        match input {
            HeldInput::Mouse(button) => match backend.release_mouse_button(button) {
                Ok(()) => { held_inputs.remove(&input); }
                Err(error) => errors.push(format!("button {button}: {error}")),
            },
            HeldInput::Key(key) => match backend.release_key(key) {
                Ok(()) => { held_inputs.remove(&input); }
                Err(error) => errors.push(format!("key scan code {}: {error}", key.scan_code)),
            },
        }
    }
}

fn validate_config(config: &ProcessConfig) -> Result<(), ProcessError> {
    if config.startup_timeout.is_zero() {
        return Err(ProcessError::InvalidConfiguration("startup_timeout must be greater than zero".to_owned()));
    }
    if config.stop_grace.is_zero() {
        return Err(ProcessError::InvalidConfiguration("stop_grace must be greater than zero".to_owned()));
    }
    if config.stop_timeout.is_zero() || config.stop_timeout < config.stop_grace {
        return Err(ProcessError::InvalidConfiguration(
            "stop_timeout must be greater than or equal to stop_grace".to_owned(),
        ));
    }
    if config.backend_call_timeout.is_zero() {
        return Err(ProcessError::InvalidConfiguration("backend_call_timeout must be greater than zero".to_owned()));
    }
    if config.event_queue_capacity == 0 {
        return Err(ProcessError::InvalidConfiguration("event_queue_capacity must be greater than zero".to_owned()));
    }
    if config.event_queue_capacity > protocol::MAX_HOST_COMMANDS {
        return Err(ProcessError::InvalidConfiguration(format!(
            "event_queue_capacity must not exceed {}",
            protocol::MAX_HOST_COMMANDS
        )));
    }
    if config.runtime.memory_limit_bytes == 0
        || config.runtime.instruction_check_interval == 0
        || config.runtime.event_queue_capacity == 0
        || config.runtime.shutdown_grace.is_zero()
    {
        return Err(ProcessError::InvalidConfiguration(
            "all ghub-lua runtime limits must be greater than zero".to_owned(),
        ));
    }
    Ok(())
}

fn canonical_worker_path(path: &Path) -> Result<PathBuf, ProcessError> {
    if !path.is_absolute() {
        return Err(ProcessError::InvalidExecutablePath("an absolute path is required".to_owned()));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|error| ProcessError::InvalidExecutablePath(error.to_string()))?;
    if is_unc_path(&canonical) {
        return Err(ProcessError::InvalidExecutablePath(
            "UNC network paths are not accepted for the worker executable".to_owned(),
        ));
    }
    let metadata = fs::metadata(&canonical)
        .map_err(|error| ProcessError::InvalidExecutablePath(error.to_string()))?;
    if !metadata.is_file() {
        return Err(ProcessError::InvalidExecutablePath("path does not identify a regular file".to_owned()));
    }
    Ok(canonical)
}

#[cfg(windows)]
fn is_unc_path(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _))
    )
}

#[cfg(not(windows))]
fn is_unc_path(_path: &Path) -> bool {
    false
}

fn spawn_worker(path: &Path) -> io::Result<Child> {
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd.spawn()
}

fn read_worker_messages(stdout: impl Read, sender: SyncSender<OutputFrame>) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_bounded_line(&mut reader, MAX_FRAME_BYTES) {
            Ok(Some(line)) => match serde_json::from_slice::<WorkerMessage>(&line) {
                Ok(message) => {
                    if sender.send(OutputFrame::Message(message)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = sender.send(OutputFrame::Invalid(format!("invalid JSON frame: {error}")));
                    return;
                }
            },
            Ok(None) => {
                let _ = sender.send(OutputFrame::Eof);
                return;
            }
            Err(error) => {
                let _ = sender.send(OutputFrame::Invalid(error.to_string()));
                return;
            }
        }
    }
}

fn write_frame(writer: &mut impl Write, message: &impl serde::Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len().saturating_add(1) > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "JSON frame exceeds the IPC limit",
        ));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn drain_stderr(mut stderr: impl Read, tail: Arc<std::sync::Mutex<VecDeque<u8>>>) {
    let mut buffer = [0_u8; 1024];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let mut tail = tail.lock().unwrap_or_else(|poison| poison.into_inner());
                for byte in &buffer[..count] {
                    tail.push_back(*byte);
                    while tail.len() > MAX_STDERR_TAIL_BYTES {
                        tail.pop_front();
                    }
                }
            }
        }
    }
}

fn read_stderr_tail(tail: &Arc<std::sync::Mutex<VecDeque<u8>>>) -> String {
    let bytes: Vec<u8> = tail
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .copied()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn push_message(messages: &mut Vec<String>, message: &str) {
    if messages.len() < MAX_REPORT_MESSAGES {
        messages.push(bounded_text(message, MAX_ERROR_BYTES));
    }
}

fn bounded_errors(errors: Vec<String>) -> Vec<String> {
    errors
        .into_iter()
        .take(MAX_REPORT_MESSAGES)
        .map(|error| bounded_text(&error, MAX_ERROR_BYTES))
        .collect()
}

fn empty_report(exit: ProcessExit) -> ProcessReport {
    ProcessReport {
        exit,
        cleanup_errors: Vec::new(),
        messages: Vec::new(),
        stderr_tail: String::new(),
        forced_kill: false,
    }
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn bounded_shutdown_grace_ms(runtime_grace_ms: u64, parent_stop_grace: Duration) -> u64 {
    let parent_callback_budget = parent_stop_grace
        .saturating_sub(Duration::from_millis(200))
        .max(Duration::from_millis(1));
    runtime_grace_ms
        .min(duration_millis(parent_callback_budget))
        .max(1)
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use ghub_lua::BackendError;
    use std::io::Cursor;

    #[test]
    fn read_bounded_line_rejects_oversized_and_unterminated_frames() {
        let mut oversized = BufReader::new(Cursor::new(b"123456\n".to_vec()));
        assert!(read_bounded_line(&mut oversized, 5).is_err());

        let mut unterminated = BufReader::new(Cursor::new(b"123".to_vec()));
        assert_eq!(
            read_bounded_line(&mut unterminated, 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn input_operations_are_validated_before_reaching_backend() {
        let mut held = BTreeSet::new();
        let backend = NoopBackend::default();
        let stop_requested = AtomicBool::new(false);
        let mut capture_enabled = false;
        let err = execute_operation(
            &backend,
            &BackendOperation::PressMouseButton { button: 6 },
            true,
            &stop_requested,
            &mut held,
            &mut capture_enabled,
        )
        .unwrap_err();
        assert!(err.contains("range 1 through 5"));
        assert!(held.is_empty());
    }

    #[test]
    fn release_without_a_brokered_press_never_reaches_backend() {
        let mut held = BTreeSet::new();
        let backend = NoopBackend::default();
        let stop_requested = AtomicBool::new(false);
        let mut capture_enabled = false;
        let err = execute_operation(
            &backend,
            &BackendOperation::ReleaseMouseButton { button: 1 },
            false,
            &stop_requested,
            &mut held,
            &mut capture_enabled,
        )
        .unwrap_err();
        assert!(err.contains("stopping process may release only brokered pressed buttons"));
        assert_eq!(backend.releases.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[derive(Default)]
    struct NoopBackend {
        releases: std::sync::atomic::AtomicUsize,
    }
    impl InputBackend for NoopBackend {
        fn press_mouse_button(&self, _: u8) -> Result<(), BackendError> { Ok(()) }
        fn release_mouse_button(&self, _: u8) -> Result<(), BackendError> {
            self.releases.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn is_mouse_button_pressed(&self, _: u8) -> Result<bool, BackendError> { Ok(false) }
        fn move_mouse_relative(&self, _: i32, _: i32) -> Result<(), BackendError> { Ok(()) }
        fn output_log_message(&self, _: &str) -> Result<(), BackendError> { Ok(()) }
        fn clear_log(&self) -> Result<(), BackendError> { Ok(()) }
    }

    #[test]
    fn wire_script_exit_round_trips() {
        let exit = WireScriptExit::Failed("synthetic test".to_owned());
        let json = serde_json::to_string(&exit).unwrap();
        let decoded: WireScriptExit = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, WireScriptExit::Failed(message) if message == "synthetic test"));
    }

    #[test]
    fn event_validation_rejects_out_of_range_keyboard_scan_codes() {
        let event = WireEvent {
            name: "KEY_PRESSED".to_owned(),
            arg: Some(0),
            family: Some("keyboard".to_owned()),
            key: Some(ghub_lua::KeyCode::new(0, false)),
        };
        assert_eq!(
            validate_event(&event),
            Err("key scan code must be between 1 and 255")
        );
    }

    #[test]
    fn callback_grace_stays_inside_parent_kill_window() {
        assert_eq!(
            bounded_shutdown_grace_ms(700, Duration::from_secs(1)),
            700
        );
        assert_eq!(
            bounded_shutdown_grace_ms(700, Duration::from_millis(250)),
            50
        );
        assert_eq!(
            bounded_shutdown_grace_ms(700, Duration::from_millis(100)),
            1
        );
    }
}
