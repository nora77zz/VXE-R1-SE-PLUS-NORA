use mlua::{
    Error as LuaError, Function, HookTriggers, Lua, MultiValue, Table, Value, Variadic, VmState,
};
use mlua::thread::ThreadStatus;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEFAULT_MEMORY_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_INSTRUCTION_CHECK_INTERVAL: u32 = 1_000;
const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 128;
const MAX_LOG_BYTES: usize = 8_192;

/// A physical keyboard scan code. Numeric Lua key values use the same Set-1
/// scan-code convention; they are not virtual-key codes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
pub struct KeyCode {
    pub scan_code: u16,
    pub extended: bool,
}

impl KeyCode {
    pub const fn new(scan_code: u16, extended: bool) -> Self {
        Self { scan_code, extended }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MousePosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputCapabilities {
    pub keyboard: bool,
    pub unicode_text: bool,
    pub absolute_mouse: bool,
    pub mouse_position: bool,
    pub scroll: bool,
    pub keyboard_events: bool,
}

/// The boundary through which a running script can observe or request input.
/// Implementations must return promptly and must not panic. The default
/// implementations preserve compatibility with mouse-only backends and fail
/// explicitly when an unsupported capability is requested.
pub trait InputBackend: Send + Sync + 'static {
    fn press_mouse_button(&self, button: u8) -> Result<(), BackendError>;
    fn release_mouse_button(&self, button: u8) -> Result<(), BackendError>;
    fn is_mouse_button_pressed(&self, button: u8) -> Result<bool, BackendError>;
    fn move_mouse_relative(&self, x: i32, y: i32) -> Result<(), BackendError>;
    fn output_log_message(&self, message: &str) -> Result<(), BackendError>;
    fn clear_log(&self) -> Result<(), BackendError>;

    fn press_key(&self, _key: KeyCode) -> Result<(), BackendError> {
        Err(unsupported_backend("keyboard input"))
    }
    fn release_key(&self, _key: KeyCode) -> Result<(), BackendError> {
        Err(unsupported_backend("keyboard input"))
    }
    fn is_key_pressed(&self, _key: KeyCode) -> Result<bool, BackendError> {
        Err(unsupported_backend("keyboard state"))
    }
    fn is_key_lock_on(&self, _lock: &str) -> Result<bool, BackendError> {
        Err(unsupported_backend("keyboard lock state"))
    }
    fn type_text(&self, _text: &str) -> Result<(), BackendError> {
        Err(unsupported_backend("Unicode keyboard input"))
    }
    fn move_mouse_to(&self, _x: i32, _y: i32) -> Result<(), BackendError> {
        Err(unsupported_backend("absolute mouse movement"))
    }
    fn mouse_position(&self) -> Result<MousePosition, BackendError> {
        Err(unsupported_backend("mouse position"))
    }
    fn screen_rect(&self) -> Result<ScreenRect, BackendError> {
        Err(unsupported_backend("screen bounds"))
    }
    /// Scroll values use multiples of 120, matching one Windows wheel notch.
    fn scroll_mouse(&self, _vertical: i32, _horizontal: i32) -> Result<(), BackendError> {
        Err(unsupported_backend("mouse wheel input"))
    }
    fn enable_keyboard_events(&self, _enabled: bool) -> Result<(), BackendError> {
        Err(unsupported_backend("physical keyboard events"))
    }
    fn capabilities(&self) -> InputCapabilities {
        InputCapabilities::default()
    }
    /// Lets an out-of-process parent start its force-termination deadline
    /// before orderly callbacks run, including after an asynchronous failure.
    fn shutdown_started(&self) -> Result<(), BackendError> {
        Ok(())
    }
}

fn unsupported_backend(capability: &str) -> BackendError {
    BackendError(format!("backend does not support {capability}"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendError(pub String);

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BackendError {}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Maximum memory used by one Lua state, in bytes.
    pub memory_limit_bytes: usize,
    /// How often the Lua VM checks whether the host requested cancellation.
    pub instruction_check_interval: u32,
    /// Maximum number of queued events. Full queues reject new events.
    pub event_queue_capacity: usize,
    /// Maximum time granted to profile-deactivation and `atk.on_stop` handlers.
    pub shutdown_grace: Duration,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            memory_limit_bytes: DEFAULT_MEMORY_LIMIT,
            instruction_check_interval: DEFAULT_INSTRUCTION_CHECK_INTERVAL,
            event_queue_capacity: DEFAULT_EVENT_QUEUE_CAPACITY,
            shutdown_grace: Duration::from_millis(700),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    InvalidConfiguration(&'static str),
    WorkerSpawn(String),
    EventQueueFull,
    WorkerStopped,
    StartupTimeout,
    FlushTimeout,
    ScriptFailed(String),
    WorkerPanicked,
    ShutdownTimeout,
    AlreadyJoined,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => write!(f, "invalid runtime configuration: {message}"),
            Self::WorkerSpawn(message) => write!(f, "could not start Lua worker: {message}"),
            Self::EventQueueFull => f.write_str("Lua event queue is full"),
            Self::WorkerStopped => f.write_str("Lua script worker has stopped"),
            Self::StartupTimeout => f.write_str("Lua script did not finish startup before timeout"),
            Self::FlushTimeout => f.write_str("Lua event queue did not drain before timeout"),
            Self::ScriptFailed(message) => write!(f, "Lua script failed: {message}"),
            Self::WorkerPanicked => f.write_str("Lua script worker panicked"),
            Self::ShutdownTimeout => f.write_str("Lua shutdown callback exceeded its deadline"),
            Self::AlreadyJoined => f.write_str("Lua script worker was already joined"),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptEvent {
    pub name: String,
    pub arg: Option<i64>,
    pub family: Option<String>,
    pub key: Option<KeyCode>,
}

impl ScriptEvent {
    pub fn new(
        name: impl Into<String>,
        arg: Option<i64>,
        family: Option<impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            arg,
            family: family.map(Into::into),
            key: None,
        }
    }

    pub fn with_key(mut self, key: KeyCode) -> Self {
        self.key = Some(key);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptExit {
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerReport {
    pub exit: ScriptExit,
    /// Release errors are reported while every known pressed button is still
    /// attempted. A failed release remains in the worker's private tracker
    /// until the worker exits.
    pub cleanup_errors: Vec<String>,
}

/// Creates workers but never starts a script as a side effect of construction.
pub struct GHubRuntime {
    backend: Arc<dyn InputBackend>,
    config: RuntimeConfig,
}

impl GHubRuntime {
    pub fn new(backend: Arc<dyn InputBackend>) -> Self {
        Self::with_config(backend, RuntimeConfig::default())
    }

    pub fn with_config(backend: Arc<dyn InputBackend>, config: RuntimeConfig) -> Self {
        Self { backend, config }
    }

    /// Explicitly starts one script on a dedicated worker. The Lua chunk runs
    /// on that worker; `wait_until_ready` reports compile or top-level errors.
    pub fn start(&self, source: impl Into<String>) -> Result<ScriptHandle, RuntimeError> {
        validate_config(&self.config)?;

        let (command_tx, command_rx) = mpsc::sync_channel(self.config.event_queue_capacity);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let control = Arc::new(WorkerControl::default());
        let worker_control = Arc::clone(&control);
        let backend = Arc::clone(&self.backend);
        let config = self.config.clone();
        let source = source.into();

        let join = thread::Builder::new()
            .name("ghub-lua-worker".to_owned())
            .spawn(move || {
                let tracker = Arc::new(SyntheticInputTracker::new(Arc::clone(&backend)));
                let capture = Arc::new(KeyboardCaptureState::default());
                let cleanup_guard = CleanupGuard {
                    tracker: Arc::clone(&tracker),
                    capture: Arc::clone(&capture),
                    backend: Arc::clone(&backend),
                };
                let mut notifier = StartupNotifier::new(startup_tx);
                let worker_backend = Arc::clone(&backend);
                let execution = catch_unwind(AssertUnwindSafe(|| {
                    worker_main(
                        &source,
                        &config,
                        worker_backend,
                        worker_control,
                        tracker.clone(),
                        capture.clone(),
                        command_rx,
                        &mut notifier,
                    )
                }));

                let (exit, mut shutdown_errors) = match execution {
                    Ok(result) => result,
                    Err(_) => {
                        notifier.fail_if_pending("worker panicked".to_owned());
                        (ScriptExit::Failed("worker panicked".to_owned()), Vec::new())
                    }
                };
                let mut cleanup_errors = tracker.release_all();
                shutdown_errors.append(&mut cleanup_errors);
                let capture_errors = capture.disable(&*backend);
                shutdown_errors.extend(capture_errors);
                drop(cleanup_guard);
                WorkerReport {
                    exit,
                    cleanup_errors: shutdown_errors,
                }
            })
            .map_err(|error| RuntimeError::WorkerSpawn(error.to_string()))?;

        Ok(ScriptHandle {
            commands: command_tx,
            control,
            startup: Some(startup_rx),
            startup_result: None,
            join: Some(join),
            shutdown_grace: self.config.shutdown_grace,
        })
    }
}

pub struct ScriptHandle {
    commands: SyncSender<WorkerCommand>,
    control: Arc<WorkerControl>,
    startup: Option<Receiver<Result<(), String>>>,
    startup_result: Option<Result<(), String>>,
    join: Option<JoinHandle<WorkerReport>>,
    shutdown_grace: Duration,
}

impl ScriptHandle {
    /// Waits for the source chunk to compile and complete its top-level code.
    pub fn wait_until_ready(&mut self, timeout: Duration) -> Result<(), RuntimeError> {
        if let Some(result) = &self.startup_result {
            return result.clone().map_err(RuntimeError::ScriptFailed);
        }
        let receiver = self.startup.take().ok_or(RuntimeError::WorkerStopped)?;
        match receiver.recv_timeout(timeout) {
            Ok(result) => {
                self.startup_result = Some(result.clone());
                result.map_err(RuntimeError::ScriptFailed)
            }
            Err(RecvTimeoutError::Timeout) => {
                self.startup = Some(receiver);
                Err(RuntimeError::StartupTimeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(RuntimeError::WorkerStopped),
        }
    }

    /// Queues one event. When the bounded queue is full, the event is rejected
    /// so a blocked script cannot grow host memory without limit.
    pub fn dispatch_event(&self, event: ScriptEvent) -> Result<(), RuntimeError> {
        self.try_send(WorkerCommand::Event(event))
    }

    /// Waits until commands already ahead of this barrier have been handled.
    /// This is useful for orderly profile changes and deterministic tests.
    pub fn flush(&self, timeout: Duration) -> Result<(), RuntimeError> {
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        self.try_send(WorkerCommand::Barrier(ack_tx))?;
        match ack_rx.recv_timeout(timeout) {
            Ok(()) => Ok(()),
            Err(RecvTimeoutError::Timeout) => Err(RuntimeError::FlushTimeout),
            Err(RecvTimeoutError::Disconnected) => Err(RuntimeError::WorkerStopped),
        }
    }

    /// Requests orderly shutdown, delivers `PROFILE_DEACTIVATED`, runs the
    /// registered `atk.on_stop` callback, and releases held synthetic inputs.
    /// Lua callbacks that exceed the grace period are interrupted by the hook;
    /// non-yieldable C or backend calls cannot be interrupted in-process.
    pub fn stop(&mut self) -> Result<WorkerReport, RuntimeError> {
        self.control.request_shutdown(self.shutdown_grace);
        let _ = self.commands.try_send(WorkerCommand::Stop);
        let Some(join) = self.join.as_ref() else {
            return Err(RuntimeError::AlreadyJoined);
        };
        let deadline = Instant::now() + self.shutdown_grace + Duration::from_millis(200);
        while !join.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(2));
        }
        if !join.is_finished() {
            self.control.force_cancel();
            let settle_deadline = Instant::now() + Duration::from_millis(200);
            while !join.is_finished() && Instant::now() < settle_deadline {
                thread::sleep(Duration::from_millis(2));
            }
        }
        if !join.is_finished() {
            drop(self.join.take());
            return Err(RuntimeError::ShutdownTimeout);
        }
        self.join_worker()
    }

    /// Joins a worker that has already stopped because of a script error.
    /// Calling this for a running worker waits until it exits.
    pub fn join(&mut self) -> Result<WorkerReport, RuntimeError> {
        self.join_worker()
    }

    /// Reports whether the Lua worker thread has completed without waiting.
    /// A handle that has already been joined is considered finished.
    pub fn is_finished(&self) -> bool {
        self.join.as_ref().map_or(true, |join| join.is_finished())
    }

    /// Joins the Lua worker only when its thread has completed.
    ///
    /// Returns `Ok(None)` while the worker is still running and
    /// `Ok(Some(report))` once its final report is available. Returns
    /// `AlreadyJoined` after another call has consumed the worker handle.
    pub fn try_join(&mut self) -> Result<Option<WorkerReport>, RuntimeError> {
        let Some(join) = self.join.as_ref() else {
            return Err(RuntimeError::AlreadyJoined);
        };
        if !join.is_finished() {
            return Ok(None);
        }
        self.join_worker().map(Some)
    }

    fn try_send(&self, command: WorkerCommand) -> Result<(), RuntimeError> {
        match self.commands.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(RuntimeError::EventQueueFull),
            Err(TrySendError::Disconnected(_)) => Err(RuntimeError::WorkerStopped),
        }
    }

    fn join_worker(&mut self) -> Result<WorkerReport, RuntimeError> {
        let join = self.join.take().ok_or(RuntimeError::AlreadyJoined)?;
        join.join().map_err(|_| RuntimeError::WorkerPanicked)
    }
}

impl Drop for ScriptHandle {
    fn drop(&mut self) {
        if self.join.is_some() {
            self.control.request_shutdown(self.shutdown_grace);
            let _ = self.commands.try_send(WorkerCommand::Stop);
            // Dropping JoinHandle detaches the worker. Do not make an arbitrary
            // caller's Drop block on Lua or a backend call; cleanup still runs
            // if and when the worker returns.
            drop(self.join.take());
        }
    }
}

fn validate_config(config: &RuntimeConfig) -> Result<(), RuntimeError> {
    if config.memory_limit_bytes == 0 {
        return Err(RuntimeError::InvalidConfiguration(
            "memory_limit_bytes must be greater than zero",
        ));
    }
    if config.instruction_check_interval == 0 {
        return Err(RuntimeError::InvalidConfiguration(
            "instruction_check_interval must be greater than zero",
        ));
    }
    if config.event_queue_capacity == 0 {
        return Err(RuntimeError::InvalidConfiguration(
            "event_queue_capacity must be greater than zero",
        ));
    }
    if config.shutdown_grace.is_zero() {
        return Err(RuntimeError::InvalidConfiguration(
            "shutdown_grace must be greater than zero",
        ));
    }
    Ok(())
}

enum WorkerCommand {
    Event(ScriptEvent),
    Barrier(SyncSender<()>),
    Stop,
}

#[derive(Default)]
struct WorkerControl {
    shutdown_requested: AtomicBool,
    hard_cancelled: AtomicBool,
    in_shutdown_callback: AtomicBool,
    shutdown_started: AtomicBool,
    shutdown_deadline: Mutex<Option<Instant>>,
    sleep_lock: Mutex<()>,
    sleep_wake: Condvar,
}

impl WorkerControl {
    fn request_shutdown(&self, grace: Duration) {
        let _guard = self.lock_sleep();
        let mut deadline = self
            .shutdown_deadline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if deadline.is_none() {
            *deadline = Some(Instant::now() + grace);
        }
        self.shutdown_requested.store(true, Ordering::Release);
        self.sleep_wake.notify_all();
    }

    fn force_cancel(&self) {
        let _guard = self.lock_sleep();
        self.hard_cancelled.store(true, Ordering::Release);
        self.sleep_wake.notify_all();
    }

    fn should_interrupt_event(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire) || self.is_hard_cancelled()
    }

    fn is_hard_cancelled(&self) -> bool {
        if self.hard_cancelled.load(Ordering::Acquire) {
            return true;
        }
        let expired = self
            .shutdown_deadline
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some_and(|deadline| Instant::now() >= deadline);
        if expired {
            self.hard_cancelled.store(true, Ordering::Release);
        }
        expired
    }

    fn is_shutting_down(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    fn sleep(&self, duration: Duration) -> Result<(), LuaError> {
        if self.is_hard_cancelled()
            || (self.shutdown_requested.load(Ordering::Acquire)
                && !self.in_shutdown_callback.load(Ordering::Acquire))
        {
            return Err(cancelled_lua_error());
        }
        let started_at = Instant::now();
        let mut guard = self.lock_sleep();
        loop {
            if self.is_hard_cancelled()
                || (self.shutdown_requested.load(Ordering::Acquire)
                    && !self.in_shutdown_callback.load(Ordering::Acquire))
            {
                return Err(cancelled_lua_error());
            }
            let mut remaining = duration.saturating_sub(started_at.elapsed());
            if self.shutdown_requested.load(Ordering::Acquire) {
                let shutdown_remaining = self
                    .shutdown_deadline
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or(remaining);
                remaining = remaining.min(shutdown_remaining);
            }
            if remaining.is_zero() {
                if self.is_hard_cancelled() {
                    return Err(cancelled_lua_error());
                }
                return Ok(());
            }
            let result = self.sleep_wake.wait_timeout(guard, remaining);
            let (next_guard, _) = result.unwrap_or_else(|poison| poison.into_inner());
            guard = next_guard;
        }
    }

    fn lock_sleep(&self) -> MutexGuard<'_, ()> {
        self.sleep_lock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn begin_shutdown_callbacks(&self) -> bool {
        !self.shutdown_started.swap(true, Ordering::AcqRel)
    }
}

struct StartupNotifier {
    sender: SyncSender<Result<(), String>>,
    sent: bool,
}

impl StartupNotifier {
    fn new(sender: SyncSender<Result<(), String>>) -> Self {
        Self {
            sender,
            sent: false,
        }
    }

    fn send(&mut self, result: Result<(), String>) {
        if !self.sent {
            let _ = self.sender.send(result);
            self.sent = true;
        }
    }

    fn fail_if_pending(&mut self, message: String) {
        self.send(Err(message));
    }
}

impl Drop for StartupNotifier {
    fn drop(&mut self) {
        self.fail_if_pending("worker stopped before startup completed".to_owned());
    }
}

fn worker_main(
    source: &str,
    config: &RuntimeConfig,
    backend: Arc<dyn InputBackend>,
    control: Arc<WorkerControl>,
    tracker: Arc<SyntheticInputTracker>,
    capture: Arc<KeyboardCaptureState>,
    commands: Receiver<WorkerCommand>,
    notifier: &mut StartupNotifier,
) -> (ScriptExit, Vec<String>) {
    let started_at = Instant::now();
    let legacy_primary_mouse_events = Arc::new(AtomicBool::new(false));
    let lua = match build_lua(
        config,
        Arc::clone(&backend),
        Arc::clone(&control),
        Arc::clone(&tracker),
        Arc::clone(&capture),
        Arc::clone(&legacy_primary_mouse_events),
        started_at,
    ) {
        Ok(lua) => lua,
        Err(error) => {
            let message = error.to_string();
            notifier.send(Err(message.clone()));
            return (failed_or_cancelled(&control, message), Vec::new());
        }
    };

    let chunk = match lua.load(source).set_name("G HUB script").into_function() {
        Ok(chunk) => chunk,
        Err(error) => {
            let message = error.to_string();
            notifier.send(Err(message.clone()));
            let exit = failed_or_cancelled(&control, message);
            control.request_shutdown(config.shutdown_grace);
            let globals = lua.globals();
            return shutdown_callbacks(
                &lua,
                &globals,
                &control,
                &backend,
                config.shutdown_grace,
                started_at,
                exit,
            );
        }
    };
    let startup_result = resume_lua_function(&lua, chunk, MultiValue::new(), &control, true);
    if let Err(error) = startup_result {
        if control.should_interrupt_event() {
            notifier.send(Err("Lua script stopped during startup".to_owned()));
            let globals = lua.globals();
            return shutdown_callbacks(
                &lua,
                &globals,
                &control,
                &backend,
                config.shutdown_grace,
                started_at,
                ScriptExit::Cancelled,
            );
        }
        let message = error.to_string();
        notifier.send(Err(message.clone()));
        control.request_shutdown(config.shutdown_grace);
        return shutdown_callbacks(
            &lua,
            &lua.globals(),
            &control,
            &backend,
            config.shutdown_grace,
            started_at,
            ScriptExit::Failed(message),
        );
    }
    if control.should_interrupt_event() {
        notifier.send(Err("Lua script stopped during startup".to_owned()));
        let globals = lua.globals();
        return shutdown_callbacks(
            &lua,
            &globals,
            &control,
            &backend,
            config.shutdown_grace,
            started_at,
            ScriptExit::Cancelled,
        );
    }

    notifier.send(Ok(()));
    let globals = lua.globals();

    loop {
        if control.is_shutting_down() {
            return shutdown_callbacks(&lua, &globals, &control, &backend, config.shutdown_grace, started_at, ScriptExit::Cancelled);
        }
        match commands.recv() {
            Ok(WorkerCommand::Event(event)) => {
                if is_primary_mouse_event(&event)
                    && !has_atk_event_handler(&globals).unwrap_or(false)
                    && !legacy_primary_mouse_events.load(Ordering::Acquire)
                {
                    continue;
                }
                match dispatch_to_lua(&lua, &globals, &event, &control, started_at, true) {
                    Ok(()) => {}
                    Err(error) => {
                        if control.is_shutting_down() {
                            return shutdown_callbacks(&lua, &globals, &control, &backend, config.shutdown_grace, started_at, ScriptExit::Cancelled);
                        }
                        let message = error.to_string();
                        control.request_shutdown(config.shutdown_grace);
                        return shutdown_callbacks(
                            &lua,
                            &globals,
                            &control,
                            &backend,
                            config.shutdown_grace,
                            started_at,
                            ScriptExit::Failed(message),
                        );
                    }
                }
                if control.is_shutting_down() {
                    return shutdown_callbacks(&lua, &globals, &control, &backend, config.shutdown_grace, started_at, ScriptExit::Cancelled);
                }
            }
            Ok(WorkerCommand::Barrier(ack)) => {
                let _ = ack.send(());
            }
            Ok(WorkerCommand::Stop) => {
                control.request_shutdown(config.shutdown_grace);
                return shutdown_callbacks(&lua, &globals, &control, &backend, config.shutdown_grace, started_at, ScriptExit::Cancelled);
            }
            Err(_) => {
                control.request_shutdown(config.shutdown_grace);
                return shutdown_callbacks(&lua, &globals, &control, &backend, config.shutdown_grace, started_at, ScriptExit::Cancelled);
            }
        }
        if control.is_shutting_down() {
            return shutdown_callbacks(&lua, &globals, &control, &backend, config.shutdown_grace, started_at, ScriptExit::Cancelled);
        }
    }
}

fn build_lua(
    config: &RuntimeConfig,
    backend: Arc<dyn InputBackend>,
    control: Arc<WorkerControl>,
    tracker: Arc<SyntheticInputTracker>,
    capture: Arc<KeyboardCaptureState>,
    legacy_primary_mouse_events: Arc<AtomicBool>,
    started_at: Instant,
) -> mlua::Result<Lua> {
    let lua = Lua::new();
    lua.set_memory_limit(config.memory_limit_bytes)?;

    let hook_control = Arc::clone(&control);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(config.instruction_check_interval),
        move |_, _| {
            if hook_control.is_hard_cancelled()
                || (hook_control.is_shutting_down()
                    && !hook_control.in_shutdown_callback.load(Ordering::Acquire))
            {
                Ok(VmState::Yield)
            } else {
                Ok(VmState::Continue)
            }
        },
    )?;

    // `Lua::new` loads the safe standard libraries. File and process helpers
    // are removed because scripts are supplied by the application. Script
    // functions run in mlua-created coroutines so the global count hook can
    // yield them to the worker when cancellation is requested.
    let globals = lua.globals();
    for name in ["debug", "dofile", "io", "loadfile", "os", "package", "require"] {
        globals.set(name, Value::Nil)?;
    }
    register_coroutine_apis(&lua)?;

    register_api(
        &lua,
        backend,
        control,
        tracker,
        capture,
        legacy_primary_mouse_events,
        started_at,
    )?;
    Ok(lua)
}

fn register_api(
    lua: &Lua,
    backend: Arc<dyn InputBackend>,
    control: Arc<WorkerControl>,
    tracker: Arc<SyntheticInputTracker>,
    capture: Arc<KeyboardCaptureState>,
    legacy_primary_mouse_events: Arc<AtomicBool>,
    started_at: Instant,
) -> mlua::Result<()> {
    register_legacy_api(
        lua,
        Arc::clone(&backend),
        Arc::clone(&control),
        Arc::clone(&tracker),
        legacy_primary_mouse_events,
        started_at,
    )?;
    register_atk_api(lua, backend, control, tracker, capture, started_at)
}

fn register_legacy_api(
    lua: &Lua,
    backend: Arc<dyn InputBackend>,
    control: Arc<WorkerControl>,
    tracker: Arc<SyntheticInputTracker>,
    legacy_primary_mouse_events: Arc<AtomicBool>,
    started_at: Instant,
) -> mlua::Result<()> {
    let globals = lua.globals();

    let sleep_control = Arc::clone(&control);
    globals.set(
        "Sleep",
        lua.create_function(move |_, milliseconds: u64| {
            sleep_control.sleep(Duration::from_millis(milliseconds))
        })?,
    )?;

    globals.set(
        "GetRunningTime",
        lua.create_function(move |_, ()| {
            let millis = started_at.elapsed().as_millis();
            Ok(millis.min(i64::MAX as u128) as i64)
        })?,
    )?;

    let log_backend = Arc::clone(&backend);
    let log_control = Arc::clone(&control);
    globals.set(
        "OutputLogMessage",
        lua.create_function(move |lua, (format, args): (String, Variadic<Value>)| {
            ensure_not_hard_cancelled(&log_control)?;
            let string_table: Table = lua.globals().get("string")?;
            let format_fn: Function = string_table.get("format")?;
            let mut values = Vec::with_capacity(args.len() + 1);
            values.push(Value::String(lua.create_string(format)?));
            values.extend(args.into_iter());
            let message: String = format_fn.call(mlua::MultiValue::from_vec(values))?;
            log_backend
                .output_log_message(&message)
                .map_err(backend_lua_error)
        })?,
    )?;

    let clear_backend = Arc::clone(&backend);
    let clear_control = Arc::clone(&control);
    globals.set(
        "ClearLog",
        lua.create_function(move |_, ()| {
            ensure_not_hard_cancelled(&clear_control)?;
            clear_backend.clear_log().map_err(backend_lua_error)
        })?,
    )?;

    register_primary_event_gate(lua, legacy_primary_mouse_events)?;

    let press_tracker = Arc::clone(&tracker);
    let press_control = Arc::clone(&control);
    globals.set(
        "PressMouseButton",
        lua.create_function(move |_, button: u8| {
            validate_button(button)?;
            ensure_input_allowed(&press_control)?;
            press_tracker.press_mouse(button)
        })?,
    )?;

    let release_tracker = Arc::clone(&tracker);
    let release_control = Arc::clone(&control);
    globals.set(
        "ReleaseMouseButton",
        lua.create_function(move |_, button: u8| {
            validate_button(button)?;
            ensure_not_hard_cancelled(&release_control)?;
            release_tracker.release_mouse(button)
        })?,
    )?;

    let click_tracker = Arc::clone(&tracker);
    let click_control = Arc::clone(&control);
    globals.set(
        "PressAndReleaseMouseButton",
        lua.create_function(move |_, button: u8| {
            validate_button(button)?;
            ensure_input_allowed(&click_control)?;
            if click_tracker.mouse_is_held(button) {
                return Err(lua_runtime_error("click cannot take ownership of a mouse button already held by the script"));
            }
            click_tracker.press_mouse(button)?;
            ensure_not_hard_cancelled(&click_control)?;
            click_tracker.release_mouse(button)
        })?,
    )?;

    let query_backend = Arc::clone(&backend);
    let query_control = Arc::clone(&control);
    globals.set(
        "IsMouseButtonPressed",
        lua.create_function(move |_, button: u8| {
            validate_button(button)?;
            ensure_not_hard_cancelled(&query_control)?;
            query_backend
                .is_mouse_button_pressed(button)
                .map_err(backend_lua_error)
        })?,
    )?;

    let move_backend = Arc::clone(&backend);
    let move_control = Arc::clone(&control);
    globals.set(
        "MoveMouseRelative",
        lua.create_function(move |_, (x, y): (i32, i32)| {
            ensure_input_allowed(&move_control)?;
            move_backend
                .move_mouse_relative(x, y)
                .map_err(backend_lua_error)
        })?,
    )?;

    Ok(())
}

fn register_atk_api(
    lua: &Lua,
    backend: Arc<dyn InputBackend>,
    control: Arc<WorkerControl>,
    tracker: Arc<SyntheticInputTracker>,
    capture: Arc<KeyboardCaptureState>,
    started_at: Instant,
) -> mlua::Result<()> {
    let atk = lua.create_table()?;
    atk.set("api_version", 1)?;

    let capabilities = backend.capabilities();
    let capability_table = lua.create_table()?;
    capability_table.set("api_version", 1)?;
    capability_table.set("keyboard", capabilities.keyboard)?;
    capability_table.set("unicode_text", capabilities.unicode_text)?;
    capability_table.set("absolute_mouse", capabilities.absolute_mouse)?;
    capability_table.set("mouse_position", capabilities.mouse_position)?;
    capability_table.set("scroll", capabilities.scroll)?;
    capability_table.set("keyboard_events", capabilities.keyboard_events)?;
    atk.set("capabilities", capability_table)?;

    let mouse = lua.create_table()?;
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        mouse.set("press", lua.create_function(move |_, button: Value| {
            let button = parse_button(button)?;
            ensure_input_allowed(&control)?;
            tracker.press_mouse(button)
        })?)?;
    }
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        mouse.set("release", lua.create_function(move |_, button: Value| {
            let button = parse_button(button)?;
            ensure_not_hard_cancelled(&control)?;
            tracker.release_mouse(button)
        })?)?;
    }
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        mouse.set("click", lua.create_function(move |_, (button, duration): (Value, Option<u64>)| {
            let button = parse_button(button)?;
            let duration = bounded_tap_duration(duration)?;
            ensure_input_allowed(&control)?;
            if tracker.mouse_is_held(button) {
                return Err(lua_runtime_error("click cannot take ownership of a mouse button already held by the script"));
            }
            tracker.press_mouse(button)?;
            let sleep_result = control.sleep(Duration::from_millis(duration));
            let release_result = tracker.release_mouse(button);
            combine_tap_results(sleep_result, release_result)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        mouse.set("is_pressed", lua.create_function(move |_, button: Value| {
            let button = parse_button(button)?;
            ensure_not_hard_cancelled(&control)?;
            backend.is_mouse_button_pressed(button).map_err(backend_lua_error)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        mouse.set("move", lua.create_function(move |_, (x, y): (Value, Value)| {
            let x = parse_bounded_delta(x, "x")?;
            let y = parse_bounded_delta(y, "y")?;
            ensure_input_allowed(&control)?;
            backend.move_mouse_relative(x, y).map_err(backend_lua_error)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        mouse.set("move_to", lua.create_function(move |_, (x, y): (i32, i32)| {
            ensure_input_allowed(&control)?;
            let rect = backend.screen_rect().map_err(backend_lua_error)?;
            validate_screen_rect(rect)?;
            if !screen_contains(rect, x, y) {
                return Err(lua_runtime_error("absolute mouse point is outside the virtual desktop"));
            }
            backend.move_mouse_to(x, y).map_err(backend_lua_error)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        mouse.set("position", lua.create_function(move |lua, ()| {
            ensure_not_hard_cancelled(&control)?;
            let position = backend.mouse_position().map_err(backend_lua_error)?;
            let result = lua.create_table()?;
            result.set("x", position.x)?;
            result.set("y", position.y)?;
            Ok(result)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        mouse.set("scroll", lua.create_function(move |_, (vertical, horizontal): (Value, Option<Value>)| {
            let vertical = parse_bounded_scroll(vertical, "vertical")?;
            let horizontal = horizontal.map_or(Ok(0), |value| parse_bounded_scroll(value, "horizontal"))?;
            ensure_input_allowed(&control)?;
            backend.scroll_mouse(vertical, horizontal).map_err(backend_lua_error)
        })?)?;
    }
    atk.set("mouse", mouse)?;

    let keyboard = lua.create_table()?;
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        keyboard.set("press", lua.create_function(move |_, key: Value| {
            let key = parse_key_value(key)?;
            ensure_input_allowed(&control)?;
            tracker.press_key(key)
        })?)?;
    }
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        keyboard.set("release", lua.create_function(move |_, key: Value| {
            let key = parse_key_value(key)?;
            ensure_not_hard_cancelled(&control)?;
            tracker.release_key(key)
        })?)?;
    }
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        keyboard.set("tap", lua.create_function(move |_, (key, duration): (Value, Option<u64>)| {
            let key = parse_key_value(key)?;
            let duration = bounded_tap_duration(duration)?;
            ensure_input_allowed(&control)?;
            if tracker.key_is_held(key) {
                return Err(lua_runtime_error("tap cannot take ownership of a key already held by the script"));
            }
            tracker.press_key(key)?;
            let sleep_result = control.sleep(Duration::from_millis(duration));
            let release_result = tracker.release_key(key);
            combine_tap_results(sleep_result, release_result)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        keyboard.set("is_pressed", lua.create_function(move |_, key: Value| {
            let key = parse_key_value(key)?;
            ensure_not_hard_cancelled(&control)?;
            backend.is_key_pressed(key).map_err(backend_lua_error)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        keyboard.set("is_modifier_pressed", lua.create_function(move |_, modifier: String| {
            ensure_not_hard_cancelled(&control)?;
            let keys = modifier_keys(&modifier)?;
            for key in keys {
                if backend.is_key_pressed(key).map_err(backend_lua_error)? {
                    return Ok(true);
                }
            }
            Ok(false)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        keyboard.set("is_lock_on", lua.create_function(move |_, lock: String| {
            ensure_not_hard_cancelled(&control)?;
            let lock = normalize_lock_name(&lock)?;
            backend.is_key_lock_on(lock).map_err(backend_lua_error)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        keyboard.set("type_text", lua.create_function(move |_, text: String| {
            if text.len() > MAX_TYPED_TEXT_BYTES {
                return Err(lua_runtime_error("text exceeds the 16384-byte input limit"));
            }
            ensure_input_allowed(&control)?;
            backend.type_text(&text).map_err(backend_lua_error)
        })?)?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        let capture = Arc::clone(&capture);
        let capabilities = capabilities;
        keyboard.set("capture_events", lua.create_function(move |_, enabled: bool| {
            if enabled && !capabilities.keyboard_events {
                return Err(lua_runtime_error("backend does not support physical keyboard events"));
            }
            if enabled {
                ensure_input_allowed(&control)?;
            } else {
                ensure_not_hard_cancelled(&control)?;
            }
            backend.enable_keyboard_events(enabled).map_err(backend_lua_error)?;
            capture.enabled.store(enabled, Ordering::Release);
            Ok(())
        })?)?;
    }
    {
        let tracker = Arc::clone(&tracker);
        let control = Arc::clone(&control);
        keyboard.set("chord", lua.create_function(move |_lua, (keys, callback): (Table, Function)| {
            let keys = parse_key_array(&keys, MAX_CHORD_KEYS)?;
            if keys.is_empty() {
                return Err(lua_runtime_error("a keyboard chord must contain at least one key"));
            }
            let mut unique = BTreeSet::new();
            if keys.iter().any(|key| !unique.insert(*key)) {
                return Err(lua_runtime_error("a keyboard chord cannot contain a duplicate key"));
            }
            ensure_input_allowed(&control)?;
            let mut pressed_here = Vec::new();
            for key in &keys {
                if tracker.key_is_held(*key) {
                    continue;
                }
                if let Err(error) = tracker.press_key(*key) {
                    let release_errors = release_keys_reverse(&tracker, &pressed_here);
                    return Err(append_cleanup_error(error, release_errors));
                }
                pressed_here.push(*key);
            }
            let callback_result = callback.call::<MultiValue>(());
            let release_errors = release_keys_reverse(&tracker, &pressed_here);
            match (callback_result, release_errors.is_empty()) {
                (Ok(values), true) => Ok(values),
                (Ok(_), false) => Err(lua_runtime_error(&release_errors.join("; "))),
                (Err(error), true) => Err(error),
                (Err(error), false) => Err(append_cleanup_error(error, release_errors)),
            }
        })?)?;
    }
    atk.set("keyboard", keyboard)?;

    let time = lua.create_table()?;
    {
        let control = Arc::clone(&control);
        time.set("sleep", lua.create_function(move |_, milliseconds: u64| {
            validate_sleep_millis(milliseconds)?;
            control.sleep(Duration::from_millis(milliseconds))
        })?)?;
    }
    {
        let control = Arc::clone(&control);
        time.set("sleep_us", lua.create_function(move |_, microseconds: u64| {
            validate_sleep_micros(microseconds)?;
            control.sleep(Duration::from_micros(microseconds))
        })?)?;
    }
    time.set("now_us", lua.create_function(move |_, ()| {
        Ok(started_at.elapsed().as_micros().min(i64::MAX as u128) as i64)
    })?)?;
    atk.set("time", time)?;

    let log = lua.create_table()?;
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        log.set(
            "write",
            lua.create_function(
                move |lua, (format, args): (String, Variadic<Value>)| {
                    ensure_not_hard_cancelled(&control)?;
                    let message = if args.is_empty() {
                        format
                    } else {
                        let string_table: Table = lua.globals().get("string")?;
                        let format_fn: Function = string_table.get("format")?;
                        let mut values = Vec::with_capacity(args.len() + 1);
                        values.push(Value::String(lua.create_string(format)?));
                        values.extend(args.into_iter());
                        format_fn.call::<String>(MultiValue::from_vec(values))?
                    };
                    if message.len() > MAX_LOG_BYTES {
                        return Err(lua_runtime_error(
                            "formatted log message exceeds the 8192-byte limit",
                        ));
                    }
                    backend.output_log_message(&message).map_err(backend_lua_error)
                },
            )?,
        )?;
    }
    {
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        log.set("clear", lua.create_function(move |_, ()| {
            ensure_not_hard_cancelled(&control)?;
            backend.clear_log().map_err(backend_lua_error)
        })?)?;
    }
    atk.set("log", log)?;

    let globals = lua.globals();
    globals.set("__atk_on_event", Value::Nil)?;
    globals.set("__atk_on_stop", Value::Nil)?;
    atk.set("on_event", lua.create_function(|lua, callback: Function| {
        lua.globals().set("__atk_on_event", callback)
    })?)?;
    atk.set("on_stop", lua.create_function(|lua, callback: Function| {
        lua.globals().set("__atk_on_stop", callback)
    })?)?;

    {
        let tracker = Arc::clone(&tracker);
        let backend = Arc::clone(&backend);
        let control = Arc::clone(&control);
        atk.set("sequence", lua.create_function(move |_, actions: Table| {
            let actions = parse_sequence(&actions)?;
            for action in actions {
                if control.is_shutting_down() {
                    return Err(cancelled_lua_error());
                }
                execute_sequence_action(&action, &backend, &tracker, &control)?;
            }
            Ok(())
        })?)?;
    }

    globals.set("atk", atk)
}

fn has_atk_event_handler(globals: &Table) -> mlua::Result<bool> {
    Ok(matches!(globals.get::<Value>("__atk_on_event")?, Value::Function(_)))
}

fn shutdown_callbacks(
    lua: &Lua,
    globals: &Table,
    control: &WorkerControl,
    backend: &Arc<dyn InputBackend>,
    shutdown_grace: Duration,
    started_at: Instant,
    exit: ScriptExit,
) -> (ScriptExit, Vec<String>) {
    control.request_shutdown(shutdown_grace);
    if !control.begin_shutdown_callbacks() {
        return (exit, Vec::new());
    }
    let mut errors = Vec::new();
    if let Err(error) = backend.shutdown_started() {
        errors.push(format!("notify parent that Lua shutdown started: {error}"));
    }
    if control.is_hard_cancelled() {
        errors.push("shutdown callbacks were skipped after the hard cancellation deadline".to_owned());
        return (exit, errors);
    }

    control.in_shutdown_callback.store(true, Ordering::Release);
    let deactivated = ScriptEvent::new("PROFILE_DEACTIVATED", None, None::<String>);
    if let Err(error) = dispatch_to_lua(lua, globals, &deactivated, control, started_at, false) {
        errors.push(format!("PROFILE_DEACTIVATED callback failed: {error}"));
    }

    if control.is_hard_cancelled() {
        errors.push("shutdown callback was interrupted at the deadline; atk.on_stop was not run".to_owned());
    } else {
        match globals.get::<Value>("__atk_on_stop") {
            Ok(Value::Function(callback)) => {
                if let Err(error) = resume_lua_function(lua, callback, MultiValue::new(), control, false) {
                    errors.push(format!("atk.on_stop callback failed: {error}"));
                }
            }
            Ok(Value::Nil) => {}
            Ok(_) => errors.push("atk.on_stop callback is not a function".to_owned()),
            Err(error) => errors.push(format!("could not read atk.on_stop callback: {error}")),
        }
    }
    if control.is_hard_cancelled()
        && !errors.iter().any(|error| error.contains("deadline"))
    {
        errors.push("shutdown callbacks were interrupted at the hard deadline".to_owned());
    }
    control.in_shutdown_callback.store(false, Ordering::Release);
    (exit, errors)
}

fn register_primary_event_gate(
    lua: &Lua,
    enabled: Arc<AtomicBool>,
) -> mlua::Result<()> {
    lua.globals().set(
        "EnablePrimaryMouseButtonEvents",
        lua.create_function(move |_, value: Value| {
            let enable = match value {
                Value::Boolean(enable) => enable,
                Value::Integer(0) => false,
                Value::Integer(1) => true,
                Value::Number(number) if number == 0.0 => false,
                Value::Number(number) if number == 1.0 => true,
                _ => {
                    return Err(LuaError::RuntimeError(
                        "EnablePrimaryMouseButtonEvents expects true, false, 1, or 0".to_owned(),
                    ))
                }
            };
            enabled.store(enable, Ordering::Release);
            Ok(())
        })?,
    )
}

fn dispatch_to_lua(
    lua: &Lua,
    globals: &Table,
    event: &ScriptEvent,
    control: &WorkerControl,
    started_at: Instant,
    interrupt_on_shutdown: bool,
) -> mlua::Result<()> {
    let atk_handler: Value = globals.get("__atk_on_event")?;
    let using_atk_handler = !matches!(atk_handler, Value::Nil);
    let handler = if using_atk_handler {
        atk_handler
    } else {
        globals.get("OnEvent")?
    };
    match handler {
        Value::Nil => Ok(()),
        Value::Function(handler) => {
            let args = if using_atk_handler {
                let event_table = lua.create_table()?;
                event_table.set("name", event.name.as_str())?;
                event_table.set("arg", event.arg)?;
                event_table.set("family", event.family.as_deref())?;
                let time_us = started_at.elapsed().as_micros().min(i64::MAX as u128) as i64;
                event_table.set("time_us", time_us)?;
                if let Some(key) = event.key {
                    let key_table = lua.create_table()?;
                    key_table.set("scan_code", key.scan_code)?;
                    key_table.set("extended", key.extended)?;
                    event_table.set("key", key_table)?;
                }
                MultiValue::from_vec(vec![Value::Table(event_table)])
            } else {
                let family = match &event.family {
                    Some(family) => Value::String(lua.create_string(family.as_str())?),
                    None => Value::Nil,
                };
                MultiValue::from_vec(vec![
                    Value::String(lua.create_string(event.name.as_str())?),
                    event.arg.map_or(Value::Nil, Value::Integer),
                    family,
                ])
            };
            resume_lua_function(lua, handler, args, control, interrupt_on_shutdown)
        }
        _ => Err(LuaError::RuntimeError(
            "OnEvent must be a function when a host event is delivered".to_owned(),
        )),
    }
}

/// Drive one Lua function in an mlua-created thread so it inherits the global
/// instruction hook. Lua-level yields are resumed with no values, matching the
/// host's event callback contract. When cancellation is requested, the count
/// hook yields the thread and this driver returns to the host without exposing
/// a Lua error that `pcall` could swallow.
fn resume_lua_function(
    lua: &Lua,
    function: Function,
    mut args: MultiValue,
    control: &WorkerControl,
    interrupt_on_shutdown: bool,
) -> mlua::Result<()> {
    if control.is_hard_cancelled()
        || (interrupt_on_shutdown && control.is_shutting_down())
    {
        return Ok(());
    }
    let thread = lua.create_thread(function)?;
    loop {
        if control.is_hard_cancelled()
            || (interrupt_on_shutdown && control.is_shutting_down())
        {
            return Ok(());
        }

        let _ = thread.resume::<MultiValue>(args)?;
        if control.is_hard_cancelled()
            || (interrupt_on_shutdown && control.is_shutting_down())
        {
            return Ok(());
        }

        match thread.status() {
            ThreadStatus::Finished => return Ok(()),
            ThreadStatus::Resumable => args = MultiValue::new(),
            ThreadStatus::Error => {
                return Err(LuaError::RuntimeError(
                    "Lua coroutine entered an error state without an error result".to_owned(),
                ))
            }
            ThreadStatus::Running | ThreadStatus::Normal => {
                return Err(LuaError::RuntimeError(
                    "Lua coroutine did not return control to its host".to_owned(),
                ))
            }
        }
    }
}

/// Lua's built-in `coroutine.create` and `coroutine.wrap` make raw Lua threads,
/// which do not inherit mlua's global hook. Replacing only these constructors
/// with `Lua::create_thread` keeps the usual coroutine API while applying the
/// host cancellation hook to script-created coroutines too.
fn register_coroutine_apis(lua: &Lua) -> mlua::Result<()> {
    let coroutine: Table = lua.globals().get("coroutine")?;
    let create = lua.create_function(|lua, function: Function| lua.create_thread(function))?;
    coroutine.set("create", create)?;

    let wrap = lua.create_function(|lua, function: Function| {
        let thread = lua.create_thread(function)?;
        lua.create_function(move |_, args: MultiValue| thread.resume::<MultiValue>(args))
    })?;
    coroutine.set("wrap", wrap)?;
    Ok(())
}

fn is_primary_mouse_event(event: &ScriptEvent) -> bool {
    event.arg == Some(1)
        && matches!(
            event.name.as_str(),
            "MOUSE_BUTTON_PRESSED" | "MOUSE_BUTTON_RELEASED"
        )
}

fn validate_button(button: u8) -> mlua::Result<()> {
    if (1..=5).contains(&button) {
        Ok(())
    } else {
        Err(LuaError::RuntimeError(
            "mouse button must be in the G HUB range 1 through 5".to_owned(),
        ))
    }
}

fn ensure_input_allowed(control: &WorkerControl) -> mlua::Result<()> {
    if control.should_interrupt_event() {
        Err(cancelled_lua_error())
    } else {
        Ok(())
    }
}

fn ensure_not_hard_cancelled(control: &WorkerControl) -> mlua::Result<()> {
    if control.is_hard_cancelled() {
        Err(cancelled_lua_error())
    } else {
        Ok(())
    }
}

fn cancelled_lua_error() -> LuaError {
    LuaError::RuntimeError("Lua script cancelled by host".to_owned())
}

fn backend_lua_error(error: BackendError) -> LuaError {
    LuaError::RuntimeError(format!("input backend: {error}"))
}

fn failed_or_cancelled(control: &WorkerControl, message: String) -> ScriptExit {
    if control.should_interrupt_event() {
        ScriptExit::Cancelled
    } else {
        ScriptExit::Failed(message)
    }
}

const MAX_TYPED_TEXT_BYTES: usize = 16_384;
const MAX_CHORD_KEYS: usize = 16;
const MAX_SEQUENCE_ACTIONS: usize = 256;
const MAX_INPUT_DELTA: i64 = 1_000_000;
const MAX_SCROLL_VALUE: i64 = 1_200_000;
const MAX_TAP_MILLIS: u64 = 60_000;
const MAX_SLEEP_MILLIS: u64 = 3_600_000;

fn lua_runtime_error(message: impl Into<String>) -> LuaError {
    LuaError::RuntimeError(message.into())
}

fn parse_integer(value: Value, label: &str) -> mlua::Result<i64> {
    match value {
        Value::Integer(value) => Ok(value),
        Value::Number(value) if value.is_finite() && value.fract() == 0.0
            && value >= i64::MIN as f64 && value <= i64::MAX as f64 => Ok(value as i64),
        _ => Err(lua_runtime_error(format!("{label} must be an integer"))),
    }
}

fn parse_button(value: Value) -> mlua::Result<u8> {
    let number = parse_integer(value, "mouse button")?;
    if (1..=5).contains(&number) {
        Ok(number as u8)
    } else {
        Err(lua_runtime_error("mouse button must be in the G HUB range 1 through 5"))
    }
}

fn parse_bounded_delta(value: Value, label: &str) -> mlua::Result<i32> {
    let number = parse_integer(value, label)?;
    if !(-MAX_INPUT_DELTA..=MAX_INPUT_DELTA).contains(&number) {
        return Err(lua_runtime_error(format!("{label} movement exceeds ±{MAX_INPUT_DELTA} pixels")));
    }
    Ok(number as i32)
}

fn parse_bounded_scroll(value: Value, label: &str) -> mlua::Result<i32> {
    let number = parse_integer(value, label)?;
    if !(-MAX_SCROLL_VALUE..=MAX_SCROLL_VALUE).contains(&number) {
        return Err(lua_runtime_error(format!("{label} scroll exceeds ±{MAX_SCROLL_VALUE} wheel units")));
    }
    Ok(number as i32)
}

fn bounded_tap_duration(duration: Option<u64>) -> mlua::Result<u64> {
    let duration = duration.unwrap_or(10);
    if duration > MAX_TAP_MILLIS {
        return Err(lua_runtime_error("tap duration must be at most 60000 ms"));
    }
    Ok(duration)
}

fn validate_sleep_millis(milliseconds: u64) -> mlua::Result<()> {
    if milliseconds > MAX_SLEEP_MILLIS {
        return Err(lua_runtime_error("sleep duration must be at most 3600000 ms"));
    }
    Ok(())
}

fn validate_sleep_micros(microseconds: u64) -> mlua::Result<()> {
    if microseconds > MAX_SLEEP_MILLIS * 1_000 {
        return Err(lua_runtime_error("sleep duration must be at most 3600000000 us"));
    }
    Ok(())
}

fn combine_tap_results(
    sleep_result: mlua::Result<()>,
    release_result: mlua::Result<()>,
) -> mlua::Result<()> {
    match (sleep_result, release_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(sleep_error), Err(release_error)) => Err(lua_runtime_error(format!(
            "{sleep_error}; release also failed: {release_error}"
        ))),
    }
}

fn parse_key_value(value: Value) -> mlua::Result<KeyCode> {
    match value {
        Value::String(value) => key_from_name(value.to_str()?.as_ref()),
        Value::Integer(value) if (1..=255).contains(&value) => Ok(KeyCode::new(value as u16, false)),
        Value::Number(value) if value.is_finite() && value.fract() == 0.0 && (1.0..=255.0).contains(&value) => {
            Ok(KeyCode::new(value as u16, false))
        }
        Value::Table(table) => {
            let scan_code: Value = table.get("scan_code")?;
            let scan_code = parse_integer(scan_code, "key scan_code")?;
            if !(1..=255).contains(&scan_code) {
                return Err(lua_runtime_error("key scan_code must be between 1 and 255"));
            }
            let extended = table.get::<Option<bool>>("extended")?.unwrap_or(false);
            Ok(KeyCode::new(scan_code as u16, extended))
        }
        _ => Err(lua_runtime_error("key must be a supported name, scan code, or {scan_code, extended} table")),
    }
}

fn key_from_name(name: &str) -> mlua::Result<KeyCode> {
    let token = name.trim().to_ascii_lowercase().replace(' ', "").replace('-', "_");
    if token.len() == 1 {
        let byte = token.as_bytes()[0];
        let scan = match byte {
            b'a' => 0x1e, b'b' => 0x30, b'c' => 0x2e, b'd' => 0x20,
            b'e' => 0x12, b'f' => 0x21, b'g' => 0x22, b'h' => 0x23,
            b'i' => 0x17, b'j' => 0x24, b'k' => 0x25, b'l' => 0x26,
            b'm' => 0x32, b'n' => 0x31, b'o' => 0x18, b'p' => 0x19,
            b'q' => 0x10, b'r' => 0x13, b's' => 0x1f, b't' => 0x14,
            b'u' => 0x16, b'v' => 0x2f, b'w' => 0x11, b'x' => 0x2d,
            b'y' => 0x15, b'z' => 0x2c,
            b'1' => 0x02, b'2' => 0x03, b'3' => 0x04, b'4' => 0x05,
            b'5' => 0x06, b'6' => 0x07, b'7' => 0x08, b'8' => 0x09,
            b'9' => 0x0a, b'0' => 0x0b,
            _ => return Err(lua_runtime_error(format!("unsupported key name: {name}"))),
        };
        return Ok(KeyCode::new(scan, false));
    }
    if let Some(number) = token.strip_prefix('f').and_then(|value| value.parse::<u8>().ok()) {
        let scan = match number { 1..=10 => 0x3a + number as u16, 11 => 0x57, 12 => 0x58, _ => 0 };
        if scan != 0 {
            return Ok(KeyCode::new(scan, false));
        }
    }
    if let Some(number) = token.strip_prefix("numpad").and_then(|value| value.parse::<u8>().ok()) {
        let scan = match number {
            0 => 0x52, 1 => 0x4f, 2 => 0x50, 3 => 0x51, 4 => 0x4b,
            5 => 0x4c, 6 => 0x4d, 7 => 0x47, 8 => 0x48, 9 => 0x49,
            _ => 0,
        };
        if scan != 0 {
            return Ok(KeyCode::new(scan, false));
        }
    }
    let key = match token.as_str() {
        "esc" | "escape" => KeyCode::new(0x01, false),
        "backspace" => KeyCode::new(0x0e, false),
        "tab" => KeyCode::new(0x0f, false),
        "enter" | "return" => KeyCode::new(0x1c, false),
        "space" => KeyCode::new(0x39, false),
        "capslock" | "caps_lock" => KeyCode::new(0x3a, false),
        "numlock" | "num_lock" => KeyCode::new(0x45, false),
        "scrolllock" | "scroll_lock" => KeyCode::new(0x46, false),
        "minus" => KeyCode::new(0x0c, false),
        "equal" | "equals" => KeyCode::new(0x0d, false),
        "leftbracket" | "left_bracket" => KeyCode::new(0x1a, false),
        "rightbracket" | "right_bracket" => KeyCode::new(0x1b, false),
        "semicolon" => KeyCode::new(0x27, false),
        "apostrophe" => KeyCode::new(0x28, false),
        "grave" | "backtick" => KeyCode::new(0x29, false),
        "backslash" => KeyCode::new(0x2b, false),
        "comma" => KeyCode::new(0x33, false),
        "period" => KeyCode::new(0x34, false),
        "slash" => KeyCode::new(0x35, false),
        "rightshift" | "rshift" => KeyCode::new(0x36, false),
        "shift" | "leftshift" | "lshift" => KeyCode::new(0x2a, false),
        "ctrl" | "control" | "leftctrl" | "leftcontrol" | "lctrl" => KeyCode::new(0x1d, false),
        "rightctrl" | "rightcontrol" | "rctrl" => KeyCode::new(0x1d, true),
        "alt" | "leftalt" | "lalt" => KeyCode::new(0x38, false),
        "rightalt" | "ralt" | "altgr" => KeyCode::new(0x38, true),
        "win" | "meta" | "leftwin" | "leftmeta" | "lwin" => KeyCode::new(0x5b, true),
        "rightwin" | "rightmeta" | "rwin" => KeyCode::new(0x5c, true),
        "insert" => KeyCode::new(0x52, true),
        "home" => KeyCode::new(0x47, true),
        "pageup" | "pgup" => KeyCode::new(0x49, true),
        "delete" | "del" => KeyCode::new(0x53, true),
        "end" => KeyCode::new(0x4f, true),
        "pagedown" | "pgdn" => KeyCode::new(0x51, true),
        "up" | "uparrow" => KeyCode::new(0x48, true),
        "left" | "leftarrow" => KeyCode::new(0x4b, true),
        "down" | "downarrow" => KeyCode::new(0x50, true),
        "right" | "rightarrow" => KeyCode::new(0x4d, true),
        "numpad_enter" => KeyCode::new(0x1c, true),
        "numpad_slash" => KeyCode::new(0x35, true),
        "numpad_multiply" => KeyCode::new(0x37, false),
        "numpad_minus" => KeyCode::new(0x4a, false),
        "numpad_plus" => KeyCode::new(0x4e, false),
        "numpad_decimal" => KeyCode::new(0x53, false),
        _ => return Err(lua_runtime_error(format!("unsupported key name: {name}"))),
    };
    Ok(key)
}

fn modifier_keys(name: &str) -> mlua::Result<Vec<KeyCode>> {
    let token = name.trim().to_ascii_lowercase().replace(' ', "").replace('-', "_");
    let left_right = match token.as_str() {
        "ctrl" | "control" => Some((0x1d, 0x1d, false, true)),
        "shift" => Some((0x2a, 0x36, false, false)),
        "alt" => Some((0x38, 0x38, false, true)),
        "win" | "meta" => Some((0x5b, 0x5c, true, true)),
        _ => None,
    };
    if let Some((left, right, left_extended, right_extended)) = left_right {
        return Ok(vec![
            KeyCode::new(left, left_extended),
            KeyCode::new(right, right_extended),
        ]);
    }
    let key = key_from_name(&token)?;
    if !matches!(key.scan_code, 0x1d | 0x2a | 0x36 | 0x38 | 0x5b | 0x5c) {
        return Err(lua_runtime_error("modifier must be ctrl, shift, alt, win, or a left/right modifier name"));
    }
    Ok(vec![key])
}

fn normalize_lock_name(name: &str) -> mlua::Result<&'static str> {
    match name.trim().to_ascii_lowercase().replace('_', "").as_str() {
        "capslock" => Ok("capslock"),
        "numlock" => Ok("numlock"),
        "scrolllock" => Ok("scrolllock"),
        _ => Err(lua_runtime_error("lock name must be capslock, numlock, or scrolllock")),
    }
}

fn parse_key_array(table: &Table, maximum: usize) -> mlua::Result<Vec<KeyCode>> {
    let length = table.raw_len();
    if length > maximum {
        return Err(lua_runtime_error(format!("key list exceeds {maximum} entries")));
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    let mut count = 0usize;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        if !matches!(key, Value::Integer(index) if index >= 1 && index as usize <= length) {
            return Err(lua_runtime_error("key list must be a dense array without extra fields"));
        }
        count += 1;
    }
    if count != length {
        return Err(lua_runtime_error("key list must be a dense array"));
    }
    (1..=length)
        .map(|index| parse_key_value(table.get::<Value>(index)?))
        .collect()
}

fn release_keys_reverse(tracker: &SyntheticInputTracker, keys: &[KeyCode]) -> Vec<String> {
    let mut errors = Vec::new();
    for key in keys.iter().rev() {
        if let Err(error) = tracker.release_key(*key) {
            errors.push(format!("release key {}: {error}", key.scan_code));
        }
    }
    errors
}

fn append_cleanup_error(error: LuaError, cleanup_errors: Vec<String>) -> LuaError {
    if cleanup_errors.is_empty() {
        error
    } else {
        lua_runtime_error(format!("{error}; cleanup failed: {}", cleanup_errors.join("; ")))
    }
}

fn validate_screen_rect(rect: ScreenRect) -> mlua::Result<()> {
    if rect.width <= 0 || rect.height <= 0
        || i64::from(rect.left) + i64::from(rect.width) > i64::from(i32::MAX) + 1
        || i64::from(rect.top) + i64::from(rect.height) > i64::from(i32::MAX) + 1
    {
        return Err(lua_runtime_error("backend returned invalid virtual desktop bounds"));
    }
    Ok(())
}

fn screen_contains(rect: ScreenRect, x: i32, y: i32) -> bool {
    i64::from(x) >= i64::from(rect.left)
        && i64::from(y) >= i64::from(rect.top)
        && i64::from(x) < i64::from(rect.left) + i64::from(rect.width)
        && i64::from(y) < i64::from(rect.top) + i64::from(rect.height)
}

#[derive(Clone, Debug)]
enum SequenceAction {
    MousePress(u8),
    MouseRelease(u8),
    MouseClick(u8, u64),
    KeyPress(KeyCode),
    KeyRelease(KeyCode),
    KeyTap(KeyCode, u64),
    Move(i32, i32),
    Scroll(i32, i32),
    SleepMillis(u64),
}

fn parse_sequence(table: &Table) -> mlua::Result<Vec<SequenceAction>> {
    let length = table.raw_len();
    if length == 0 || length > MAX_SEQUENCE_ACTIONS {
        return Err(lua_runtime_error(format!("sequence must contain 1 through {MAX_SEQUENCE_ACTIONS} actions")));
    }
    let mut count = 0usize;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, value) = pair?;
        if !matches!(key, Value::Integer(index) if index >= 1 && index as usize <= length)
            || !matches!(value, Value::Table(_))
        {
            return Err(lua_runtime_error("sequence must be a dense array of action tables"));
        }
        count += 1;
    }
    if count != length {
        return Err(lua_runtime_error("sequence must be a dense array without gaps"));
    }
    let mut actions = Vec::with_capacity(length);
    for index in 1..=length {
        let action: Table = table.get(index)?;
        let kind: String = action.get("kind")?;
        let operation: String = action.get("action").unwrap_or_default();
        let parsed = match kind.as_str() {
            "mouse" => {
                let button = parse_button(action.get::<Value>("button")?)?;
                match operation.as_str() {
                    "press" => SequenceAction::MousePress(button),
                    "release" => SequenceAction::MouseRelease(button),
                    "click" => SequenceAction::MouseClick(button, bounded_tap_duration(action.get::<Option<u64>>("duration_ms")?)?),
                    _ => return Err(lua_runtime_error("mouse sequence action must be press, release, or click")),
                }
            }
            "key" => {
                let key = parse_key_value(action.get::<Value>("key")?)?;
                match operation.as_str() {
                    "press" => SequenceAction::KeyPress(key),
                    "release" => SequenceAction::KeyRelease(key),
                    "tap" => SequenceAction::KeyTap(key, bounded_tap_duration(action.get::<Option<u64>>("duration_ms")?)?),
                    _ => return Err(lua_runtime_error("key sequence action must be press, release, or tap")),
                }
            }
            "move" => SequenceAction::Move(
                parse_bounded_delta(action.get::<Value>("x")?, "x")?,
                parse_bounded_delta(action.get::<Value>("y")?, "y")?,
            ),
            "scroll" => SequenceAction::Scroll(
                parse_bounded_scroll(action.get::<Value>("vertical")?, "vertical")?,
                action.get::<Option<Value>>("horizontal")?.map_or(Ok(0), |value| parse_bounded_scroll(value, "horizontal"))?,
            ),
            "sleep" => {
                let milliseconds = action.get::<u64>("ms")?;
                validate_sleep_millis(milliseconds)?;
                SequenceAction::SleepMillis(milliseconds)
            }
            _ => return Err(lua_runtime_error("sequence action kind must be mouse, key, move, scroll, or sleep")),
        };
        actions.push(parsed);
    }
    Ok(actions)
}

fn execute_sequence_action(
    action: &SequenceAction,
    backend: &Arc<dyn InputBackend>,
    tracker: &Arc<SyntheticInputTracker>,
    control: &Arc<WorkerControl>,
) -> mlua::Result<()> {
    match action {
        SequenceAction::MousePress(button) => {
            ensure_input_allowed(control)?;
            tracker.press_mouse(*button)
        }
        SequenceAction::MouseRelease(button) => {
            ensure_not_hard_cancelled(control)?;
            tracker.release_mouse(*button)
        }
        SequenceAction::MouseClick(button, duration) => {
            ensure_input_allowed(control)?;
            if tracker.mouse_is_held(*button) {
                return Err(lua_runtime_error("click cannot take ownership of a mouse button already held by the script"));
            }
            tracker.press_mouse(*button)?;
            let sleep = control.sleep(Duration::from_millis(*duration));
            let release = tracker.release_mouse(*button);
            combine_tap_results(sleep, release)
        }
        SequenceAction::KeyPress(key) => {
            ensure_input_allowed(control)?;
            tracker.press_key(*key)
        }
        SequenceAction::KeyRelease(key) => {
            ensure_not_hard_cancelled(control)?;
            tracker.release_key(*key)
        }
        SequenceAction::KeyTap(key, duration) => {
            ensure_input_allowed(control)?;
            if tracker.key_is_held(*key) {
                return Err(lua_runtime_error("tap cannot take ownership of a key already held by the script"));
            }
            tracker.press_key(*key)?;
            let sleep = control.sleep(Duration::from_millis(*duration));
            let release = tracker.release_key(*key);
            combine_tap_results(sleep, release)
        }
        SequenceAction::Move(x, y) => {
            ensure_input_allowed(control)?;
            backend.move_mouse_relative(*x, *y).map_err(backend_lua_error)
        }
        SequenceAction::Scroll(vertical, horizontal) => {
            ensure_input_allowed(control)?;
            backend.scroll_mouse(*vertical, *horizontal).map_err(backend_lua_error)
        }
        SequenceAction::SleepMillis(milliseconds) => control.sleep(Duration::from_millis(*milliseconds)),
    }
}

struct SyntheticInputTracker {
    backend: Arc<dyn InputBackend>,
    pressed_mouse: Mutex<BTreeSet<u8>>,
    pressed_keys: Mutex<BTreeSet<KeyCode>>,
}

impl SyntheticInputTracker {
    fn new(backend: Arc<dyn InputBackend>) -> Self {
        Self {
            backend,
            pressed_mouse: Mutex::new(BTreeSet::new()),
            pressed_keys: Mutex::new(BTreeSet::new()),
        }
    }

    fn press_mouse(&self, button: u8) -> mlua::Result<()> {
        if self.lock_mouse().contains(&button) {
            return Ok(());
        }
        self.backend
            .press_mouse_button(button)
            .map_err(backend_lua_error)?;
        self.lock_mouse().insert(button);
        Ok(())
    }

    fn release_mouse(&self, button: u8) -> mlua::Result<()> {
        self.backend
            .release_mouse_button(button)
            .map_err(backend_lua_error)?;
        self.lock_mouse().remove(&button);
        Ok(())
    }

    fn press_key(&self, key: KeyCode) -> mlua::Result<()> {
        if self.lock_keys().contains(&key) {
            return Ok(());
        }
        self.backend.press_key(key).map_err(backend_lua_error)?;
        self.lock_keys().insert(key);
        Ok(())
    }

    fn release_key(&self, key: KeyCode) -> mlua::Result<()> {
        self.backend.release_key(key).map_err(backend_lua_error)?;
        self.lock_keys().remove(&key);
        Ok(())
    }

    fn mouse_is_held(&self, button: u8) -> bool {
        self.lock_mouse().contains(&button)
    }

    fn key_is_held(&self, key: KeyCode) -> bool {
        self.lock_keys().contains(&key)
    }

    fn release_all(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let buttons: Vec<u8> = self.lock_mouse().iter().copied().collect();
        for button in buttons {
            match self.backend.release_mouse_button(button) {
                Ok(()) => {
                    self.lock_mouse().remove(&button);
                }
                Err(error) => errors.push(format!("button {button}: {error}")),
            }
        }
        let keys: Vec<KeyCode> = self.lock_keys().iter().copied().collect();
        for key in keys {
            match self.backend.release_key(key) {
                Ok(()) => {
                    self.lock_keys().remove(&key);
                }
                Err(error) => errors.push(format!(
                    "key scan code {}{}: {error}",
                    key.scan_code,
                    if key.extended { " extended" } else { "" }
                )),
            }
        }
        errors
    }

    fn lock_mouse(&self) -> MutexGuard<'_, BTreeSet<u8>> {
        self.pressed_mouse
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn lock_keys(&self) -> MutexGuard<'_, BTreeSet<KeyCode>> {
        self.pressed_keys
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[derive(Default)]
struct KeyboardCaptureState {
    enabled: AtomicBool,
}

impl KeyboardCaptureState {
    fn disable(&self, backend: &dyn InputBackend) -> Vec<String> {
        if !self.enabled.swap(false, Ordering::AcqRel) {
            return Vec::new();
        }
        backend
            .enable_keyboard_events(false)
            .err()
            .map(|error| vec![format!("disable keyboard event capture: {error}")])
            .unwrap_or_default()
    }
}

struct CleanupGuard {
    tracker: Arc<SyntheticInputTracker>,
    capture: Arc<KeyboardCaptureState>,
    backend: Arc<dyn InputBackend>,
}

impl Drop for CleanupGuard {
    fn drop(&mut self) {
        let _ = self.tracker.release_all();
        let _ = self.capture.disable(&*self.backend);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Action {
        Press(u8),
        Release(u8),
        PressKey(KeyCode),
        ReleaseKey(KeyCode),
        Move(i32, i32),
        MoveTo(i32, i32),
        Scroll(i32, i32),
        TypeText(String),
        Capture(bool),
        ClearLog,
    }

    #[derive(Default)]
    struct MockBackend {
        actions: Mutex<Vec<Action>>,
        logs: Mutex<Vec<String>>,
        pressed: Mutex<BTreeSet<u8>>,
        pressed_keys: Mutex<BTreeSet<KeyCode>>,
        changed: Condvar,
    }

    impl MockBackend {
        fn wait_for_log_count(&self, count: usize) {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut logs = self.lock(&self.logs);
            while logs.len() < count {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "timed out waiting for Lua log output");
                let (next, result) = self
                    .changed
                    .wait_timeout(logs, remaining)
                    .unwrap_or_else(|poison| poison.into_inner());
                logs = next;
                assert!(!result.timed_out() || logs.len() >= count);
            }
        }

        fn action_snapshot(&self) -> Vec<Action> {
            self.lock(&self.actions).clone()
        }

        fn log_snapshot(&self) -> Vec<String> {
            self.lock(&self.logs).clone()
        }

        fn lock<'a, T>(&self, mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
            mutex.lock().unwrap_or_else(|poison| poison.into_inner())
        }
    }

    impl InputBackend for MockBackend {
        fn press_mouse_button(&self, button: u8) -> Result<(), BackendError> {
            self.lock(&self.pressed).insert(button);
            self.lock(&self.actions).push(Action::Press(button));
            Ok(())
        }

        fn release_mouse_button(&self, button: u8) -> Result<(), BackendError> {
            self.lock(&self.pressed).remove(&button);
            self.lock(&self.actions).push(Action::Release(button));
            Ok(())
        }

        fn is_mouse_button_pressed(&self, button: u8) -> Result<bool, BackendError> {
            Ok(self.lock(&self.pressed).contains(&button))
        }

        fn move_mouse_relative(&self, x: i32, y: i32) -> Result<(), BackendError> {
            self.lock(&self.actions).push(Action::Move(x, y));
            Ok(())
        }

        fn press_key(&self, key: KeyCode) -> Result<(), BackendError> {
            self.lock(&self.pressed_keys).insert(key);
            self.lock(&self.actions).push(Action::PressKey(key));
            Ok(())
        }

        fn release_key(&self, key: KeyCode) -> Result<(), BackendError> {
            self.lock(&self.pressed_keys).remove(&key);
            self.lock(&self.actions).push(Action::ReleaseKey(key));
            Ok(())
        }

        fn is_key_pressed(&self, key: KeyCode) -> Result<bool, BackendError> {
            Ok(self.lock(&self.pressed_keys).contains(&key))
        }

        fn is_key_lock_on(&self, _lock: &str) -> Result<bool, BackendError> {
            Ok(false)
        }

        fn type_text(&self, text: &str) -> Result<(), BackendError> {
            self.lock(&self.actions).push(Action::TypeText(text.to_owned()));
            Ok(())
        }

        fn move_mouse_to(&self, x: i32, y: i32) -> Result<(), BackendError> {
            self.lock(&self.actions).push(Action::MoveTo(x, y));
            Ok(())
        }

        fn mouse_position(&self) -> Result<MousePosition, BackendError> {
            Ok(MousePosition { x: 10, y: 20 })
        }

        fn screen_rect(&self) -> Result<ScreenRect, BackendError> {
            Ok(ScreenRect {
                left: -1920,
                top: -1080,
                width: 3840,
                height: 2160,
            })
        }

        fn scroll_mouse(&self, vertical: i32, horizontal: i32) -> Result<(), BackendError> {
            self.lock(&self.actions).push(Action::Scroll(vertical, horizontal));
            Ok(())
        }

        fn enable_keyboard_events(&self, enabled: bool) -> Result<(), BackendError> {
            self.lock(&self.actions).push(Action::Capture(enabled));
            Ok(())
        }

        fn capabilities(&self) -> InputCapabilities {
            InputCapabilities {
                keyboard: true,
                unicode_text: true,
                absolute_mouse: true,
                mouse_position: true,
                scroll: true,
                keyboard_events: true,
            }
        }

        fn output_log_message(&self, message: &str) -> Result<(), BackendError> {
            self.lock(&self.logs).push(message.to_owned());
            self.changed.notify_all();
            Ok(())
        }

        fn clear_log(&self) -> Result<(), BackendError> {
            self.lock(&self.logs).clear();
            self.lock(&self.actions).push(Action::ClearLog);
            self.changed.notify_all();
            Ok(())
        }
    }

    fn runtime(backend: &Arc<MockBackend>) -> GHubRuntime {
        GHubRuntime::new(Arc::clone(backend) as Arc<dyn InputBackend>)
    }

    fn wait_ready(handle: &mut ScriptHandle) {
        handle
            .wait_until_ready(Duration::from_secs(2))
            .expect("script should complete its startup chunk");
    }

    #[test]
    fn stop_interrupts_sleep_and_releases_pressed_mouse_button() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
PressMouseButton(1)
function OnEvent(event)
    if event == "SLEEP" then
        OutputLogMessage("sleeping")
        Sleep(60000)
    end
end
"#,
            )
            .expect("worker starts");
        wait_ready(&mut handle);
        handle
            .dispatch_event(ScriptEvent::new("SLEEP", None, None::<String>))
            .expect("sleep event queued");
        backend.wait_for_log_count(1);

        let started = Instant::now();
        let report = handle.stop().expect("worker joins");

        assert_eq!(report.exit, ScriptExit::Cancelled);
        assert!(report.cleanup_errors.is_empty());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            backend.action_snapshot(),
            vec![Action::Press(1), Action::Release(1)]
        );
    }

    #[test]
    fn instruction_hook_cancels_a_tight_loop_inside_protected_call() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
OutputLogMessage("%s:%s", type(pcall), type(xpcall))
pcall(function()
    while true do end
end)
"#,
            )
            .expect("worker starts");
        backend.wait_for_log_count(1);
        assert_eq!(backend.log_snapshot(), vec!["function:function"]);

        let started = Instant::now();
        let report = handle.stop().expect("hook yields through pcall");

        assert_eq!(report.exit, ScriptExit::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn lua_created_coroutines_inherit_the_cancellation_hook() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
OutputLogMessage("coroutines:%s:%s", type(coroutine.create), type(coroutine.wrap))
function OnEvent(event)
    if event == "RUN" then
        OutputLogMessage("running")
        local created = coroutine.create(function()
            while true do end
        end)
        local wrapped = coroutine.wrap(function()
            while true do end
        end)
        while true do
            coroutine.resume(created)
            wrapped()
        end
    end
end
"#,
            )
            .expect("worker starts");
        wait_ready(&mut handle);
        assert_eq!(backend.log_snapshot(), vec!["coroutines:function:function"]);

        handle
            .dispatch_event(ScriptEvent::new("RUN", None, None::<String>))
            .expect("run event queued");
        backend.wait_for_log_count(2);

        let started = Instant::now();
        let report = handle.stop().expect("worker joins after coroutine yields");

        assert_eq!(report.exit, ScriptExit::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(backend.log_snapshot(), vec!["coroutines:function:function", "running"]);
    }

    #[test]
    fn primary_button_events_are_gated_until_the_script_enables_them() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let source = r#"
function OnEvent(event, arg, family)
    if event == "ENABLE" then
        EnablePrimaryMouseButtonEvents(true)
        OutputLogMessage("enabled")
    elseif event == "MOUSE_BUTTON_PRESSED" then
        OutputLogMessage("%d:%s", arg, family or "")
    end
end
"#;
        let mut handle = runtime.start(source).expect("worker starts");
        wait_ready(&mut handle);

        handle
            .dispatch_event(ScriptEvent::new(
                "MOUSE_BUTTON_PRESSED",
                Some(1),
                Some("mouse"),
            ))
            .expect("primary event queued");
        handle
            .dispatch_event(ScriptEvent::new(
                "MOUSE_BUTTON_PRESSED",
                Some(2),
                Some("mouse"),
            ))
            .expect("secondary event queued");
        handle.flush(Duration::from_secs(2)).expect("events handled");
        assert_eq!(backend.log_snapshot(), vec!["2:mouse"]);

        handle
            .dispatch_event(ScriptEvent::new("ENABLE", None, None::<String>))
            .expect("enable event queued");
        handle.flush(Duration::from_secs(2)).expect("enable handled");
        handle
            .dispatch_event(ScriptEvent::new(
                "MOUSE_BUTTON_PRESSED",
                Some(1),
                Some("mouse"),
            ))
            .expect("primary event queued after enabling");
        handle.flush(Duration::from_secs(2)).expect("primary event handled");
        assert_eq!(backend.log_snapshot(), vec!["2:mouse", "enabled", "1:mouse"]);

        let report = handle.stop().expect("worker joins");
        assert_eq!(report.exit, ScriptExit::Cancelled);
    }

    #[test]
    fn mouse_calls_format_logs_and_clear_log_through_the_mock_backend() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let source = r#"
function OnEvent(event, arg, family)
    ClearLog()
    MoveMouseRelative(12, -3)
    PressMouseButton(2)
    if IsMouseButtonPressed(2) then
        OutputLogMessage("%s:%d:%s:%d", event, arg, family, GetRunningTime())
    end
    PressAndReleaseMouseButton(4)
    ReleaseMouseButton(2)
end
"#;
        let mut handle = runtime.start(source).expect("worker starts");
        wait_ready(&mut handle);
        handle
            .dispatch_event(ScriptEvent::new(
                "MOUSE_BUTTON_PRESSED",
                Some(3),
                Some("mouse"),
            ))
            .expect("event queued");
        handle.flush(Duration::from_secs(2)).expect("event handled");

        let actions = backend.action_snapshot();
        assert!(actions.starts_with(&[
            Action::ClearLog,
            Action::Move(12, -3),
            Action::Press(2),
        ]));
        assert!(actions.ends_with(&[
            Action::Press(4),
            Action::Release(4),
            Action::Release(2),
        ]));
        let logs = backend.log_snapshot();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].starts_with("MOUSE_BUTTON_PRESSED:3:mouse:"));

        let report = handle.stop().expect("worker joins");
        assert_eq!(report.exit, ScriptExit::Cancelled);
    }

    #[test]
    fn script_error_releases_mouse_button_pressed_by_the_script() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let source = r#"
function OnEvent(event)
    if event == "FAIL" then
        PressMouseButton(5)
        error("requested test failure")
    end
end
"#;
        let mut handle = runtime.start(source).expect("worker starts");
        wait_ready(&mut handle);
        handle
            .dispatch_event(ScriptEvent::new("FAIL", None, None::<String>))
            .expect("event queued");

        let report = handle.join().expect("failed worker joins");

        assert!(matches!(report.exit, ScriptExit::Failed(message) if message.contains("requested test failure")));
        assert!(report.cleanup_errors.is_empty());
        assert_eq!(
            backend.action_snapshot(),
            vec![Action::Press(5), Action::Release(5)]
        );
    }

    #[test]
    fn atk_sequence_validates_all_actions_and_absolute_bounds_before_input() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let source = r#"
local sequence_ok = pcall(function()
    atk.sequence({
        { kind = "mouse", action = "press", button = 1 },
        { kind = "mouse", action = "press", button = 9 },
    })
end)
local bounds_ok = pcall(function() atk.mouse.move_to(1920, 0) end)
atk.log.write("%s:%s:%s:%s",
    tostring(atk.capabilities.keyboard),
    tostring(atk.capabilities.keyboard_events),
    tostring(not sequence_ok),
    tostring(not bounds_ok))
"#;
        let mut handle = runtime.start(source).expect("worker starts");
        wait_ready(&mut handle);

        assert_eq!(backend.log_snapshot(), vec!["true:true:true:true"]);
        assert!(backend.action_snapshot().is_empty());
        assert_eq!(handle.stop().expect("worker joins").exit, ScriptExit::Cancelled);
    }

    #[test]
    fn atk_sequence_composes_input_queries_and_unicode_text() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
atk.sequence({
    { kind = "mouse", action = "click", button = 1, duration_ms = 0 },
    { kind = "key", action = "tap", key = "a", duration_ms = 0 },
    { kind = "move", x = 12, y = -3 },
    { kind = "scroll", vertical = 120, horizontal = -120 },
    { kind = "sleep", ms = 0 },
})
atk.mouse.move_to(-1920, -1080)
atk.keyboard.type_text("hé")
local position = atk.mouse.position()
atk.log.write("%d:%d:%s:%s:%s",
    position.x, position.y,
    tostring(atk.mouse.is_pressed(1)),
    tostring(atk.keyboard.is_pressed("a")),
    tostring(atk.keyboard.is_lock_on("caps_lock")))
"#,
            )
            .expect("worker starts");
        wait_ready(&mut handle);

        assert_eq!(backend.log_snapshot(), vec!["10:20:false:false:false"]);
        assert_eq!(
            backend.action_snapshot(),
            vec![
                Action::Press(1),
                Action::Release(1),
                Action::PressKey(KeyCode::new(0x1e, false)),
                Action::ReleaseKey(KeyCode::new(0x1e, false)),
                Action::Move(12, -3),
                Action::Scroll(120, -120),
                Action::MoveTo(-1920, -1080),
                Action::TypeText("hé".to_owned()),
            ]
        );
        assert_eq!(handle.stop().expect("worker joins").exit, ScriptExit::Cancelled);
    }

    #[test]
    fn atk_event_shape_has_priority_over_legacy_event_without_double_dispatch() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
function OnEvent(event)
    if event == "KEY_PRESSED" then atk.log.write("legacy") end
end
atk.on_event(function(event)
    if event.name == "KEY_PRESSED" then
        atk.log.write("%s:%d:%s:%d:%s", event.name, event.arg,
            event.family, event.key.scan_code, tostring(event.key.extended))
    end
end)
"#,
            )
            .expect("worker starts");
        wait_ready(&mut handle);
        handle
            .dispatch_event(
                ScriptEvent::new("KEY_PRESSED", Some(30), Some("keyboard"))
                    .with_key(KeyCode::new(30, false)),
            )
            .expect("key event queues");
        handle.flush(Duration::from_secs(2)).expect("event handler completes");

        assert_eq!(backend.log_snapshot(), vec!["KEY_PRESSED:30:keyboard:30:false"]);
        assert_eq!(handle.stop().expect("worker joins").exit, ScriptExit::Cancelled);
    }

    #[test]
    fn atk_keyboard_chord_preserves_keys_already_owned_by_the_script() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
atk.keyboard.press("ctrl")
atk.keyboard.chord({"ctrl", "a"}, function()
    atk.log.write("chord")
end)
"#,
            )
            .expect("worker starts");
        wait_ready(&mut handle);

        let ctrl = KeyCode::new(0x1d, false);
        let a = KeyCode::new(0x1e, false);
        assert_eq!(backend.log_snapshot(), vec!["chord"]);
        assert_eq!(
            backend.action_snapshot(),
            vec![Action::PressKey(ctrl), Action::PressKey(a), Action::ReleaseKey(a)]
        );
        assert!(backend.is_key_pressed(ctrl).unwrap());
        assert!(!backend.is_key_pressed(a).unwrap());

        let report = handle.stop().expect("worker joins");
        assert_eq!(report.exit, ScriptExit::Cancelled);
        assert_eq!(
            backend.action_snapshot(),
            vec![
                Action::PressKey(ctrl),
                Action::PressKey(a),
                Action::ReleaseKey(a),
                Action::ReleaseKey(ctrl),
            ]
        );
    }

    #[test]
    fn stop_during_atk_sleep_delivers_deactivation_and_stop_once() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
atk.on_event(function(event)
    if event.name == "WAIT" then
        atk.log.write("sleeping")
        atk.time.sleep(60000)
    elseif event.name == "PROFILE_DEACTIVATED" then
        atk.log.write("deactivated")
    end
end)
atk.on_stop(function() atk.log.write("stopped") end)
"#,
            )
            .expect("worker starts");
        wait_ready(&mut handle);
        handle
            .dispatch_event(ScriptEvent::new("WAIT", None, None::<String>))
            .expect("wait event queues");
        backend.wait_for_log_count(1);

        let report = handle.stop().expect("worker joins after cooperative stop");

        assert_eq!(report.exit, ScriptExit::Cancelled);
        assert!(report.cleanup_errors.is_empty());
        assert_eq!(backend.log_snapshot(), vec!["sleeping", "deactivated", "stopped"]);
    }

    #[test]
    fn top_level_script_error_runs_registered_shutdown_callbacks_and_preserves_failure() {
        let backend = Arc::new(MockBackend::default());
        let runtime = runtime(&backend);
        let mut handle = runtime
            .start(
                r#"
atk.on_event(function(event)
    if event.name == "PROFILE_DEACTIVATED" then atk.log.write("deactivated") end
end)
atk.on_stop(function() atk.log.write("stopped") end)
error("top-level failure")
"#,
            )
            .expect("worker starts");

        assert!(matches!(
            handle.wait_until_ready(Duration::from_secs(2)),
            Err(RuntimeError::ScriptFailed(message)) if message.contains("top-level failure")
        ));
        let report = handle.join().expect("failed worker joins after callbacks");

        assert!(matches!(report.exit, ScriptExit::Failed(message) if message.contains("top-level failure")));
        assert_eq!(backend.log_snapshot(), vec!["deactivated", "stopped"]);
    }
}
