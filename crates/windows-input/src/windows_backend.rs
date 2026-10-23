use super::{
    absolute_move_input, check_send_input_count, encode_unicode_text, key_event_flags, mouse_button_input,
    raw_input_record_is_complete, raw_input_storage_capacity, raw_key_parts,
    raw_mouse_button_transitions, raw_wheel_deltas, relative_move_input,
    should_capture_device, unicode_event_flags, virtual_key_for_button, wheel_input,
    BoundedLog, LogEntry, MouseInputSpec, StopShortcut, INPUT_INJECTION_TAG, MAX_RAW_INPUT_BYTES,
};
use ghub_lua::{
    BackendError, InputBackend, InputCapabilities, KeyCode, MousePosition, ScreenRect,
    ScriptEvent,
};
use std::fmt;
use std::mem::{size_of, MaybeUninit};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{GetLastError, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetKeyState, GetKeyboardLayout, MapVirtualKeyExW, SendInput,
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_UNICODE,
    MAPVK_VSC_TO_VK_EX, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
    MOD_NOREPEAT,
    MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RAWKEYBOARD, RAWMOUSE, RIDEV_INPUTSINK, RIDEV_REMOVE,
    RIDI_DEVICENAME, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
    GetMessageW, GetSystemMetrics, PeekMessageW, PostMessageW, PostThreadMessageW, RegisterClassExW,
    TranslateMessage, UnregisterClassW, HMENU, MSG, WNDCLASSEXW, HWND_MESSAGE,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_HOTKEY, WM_INPUT, WM_QUIT, PM_NOREMOVE, PM_REMOVE,
};
use windows::Win32::Foundation::POINT;

const STOP_LISTENER_MESSAGE: u32 = WM_APP + 0x521;
const STOP_HOTKEY_ID_A: i32 = 0x4A21;
const STOP_HOTKEY_ID_B: i32 = 0x4A22;
const STOP_HOTKEY_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_HOTKEY_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_PENDING: u8 = 0;
const REQUEST_CANCELLED: u8 = 1;
const REQUEST_COMMITTING: u8 = 2;
static RAW_INPUT_LISTENER_ACTIVE: AtomicBool = AtomicBool::new(false);

enum StopHotkeyRequest {
    Set {
        shortcut: StopShortcut,
        lifecycle: Arc<AtomicU8>,
        deadline: Instant,
        reply: SyncSender<Result<(), String>>,
    },
    Shutdown(SyncSender<Result<(), String>>),
}

/// Owns the dedicated Win32 message thread for a system-wide Lua stop shortcut.
pub struct GlobalStopShortcutManager {
    requests: mpsc::Sender<StopHotkeyRequest>,
    #[cfg(test)]
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl GlobalStopShortcutManager {
    /// Starts the hotkey message thread and attempts the initial registration.
    /// A registration conflict is returned separately so the app can keep
    /// running and report the actual Windows error to the user.
    pub fn start(
        initial: StopShortcut,
        on_hotkey: impl Fn() + Send + Sync + 'static,
    ) -> Result<(Self, Result<(), String>), String> {
        let (requests, receiver) = mpsc::channel();
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(on_hotkey);
        let thread = thread::Builder::new()
            .name("atk-lua-stop-hotkey".to_owned())
            .spawn(move || stop_hotkey_message_thread(receiver, startup_sender, initial, callback))
            .map_err(|error| format!("não foi possível iniciar a thread do atalho global: {error}"))?;
        let (thread_id, registration) = startup_receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| format!("a thread do atalho global não inicializou a tempo: {error}"))?;
        #[cfg(not(test))]
        let _ = thread_id;
        Ok((
            Self {
                requests,
                #[cfg(test)]
                thread_id,
                thread: Some(thread),
            },
            registration,
        ))
    }

    /// Registers a replacement before releasing the current key combination.
    /// A conflict leaves the previously registered shortcut active.
    pub fn set(&self, shortcut: StopShortcut) -> Result<(), String> {
        if self.thread.as_ref().map_or(true, JoinHandle::is_finished) {
            return Err("a thread do atalho global não está ativa".to_owned());
        }
        let lifecycle = Arc::new(AtomicU8::new(REQUEST_PENDING));
        let deadline = Instant::now() + STOP_HOTKEY_COMMAND_TIMEOUT;
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self.requests
            .send(StopHotkeyRequest::Set {
                shortcut,
                lifecycle: Arc::clone(&lifecycle),
                deadline,
                reply: reply_sender,
            })
            .map_err(|_| "a thread do atalho global não está ativa".to_owned())?;
        match reply_receiver.recv_timeout(STOP_HOTKEY_COMMAND_TIMEOUT + Duration::from_secs(1)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if lifecycle
                    .compare_exchange(
                        REQUEST_PENDING,
                        REQUEST_CANCELLED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    Err("o registro do atalho expirou; a troca foi cancelada".to_owned())
                } else {
                    // A request that has claimed its commit slot is not expired.
                    // Give the Win32 operation a final bounded interval to report
                    // whether it preserved or replaced the prior registration.
                    match reply_receiver.recv_timeout(Duration::from_millis(500)) {
                        Ok(result) => result,
                        Err(_) => Err(
                            "a troca do atalho entrou na etapa final, mas o resultado não pôde ser confirmado"
                                .to_owned(),
                        ),
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("a thread do atalho global terminou sem responder".to_owned())
            }
        }
    }

    /// Unregisters the active shortcut and joins the message thread.
    pub fn stop(&mut self) -> Result<(), String> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(());
        };
        if thread.is_finished() {
            return self
                .thread
                .take()
                .expect("thread checked above")
                .join()
                .map_err(|_| "a thread do atalho global entrou em panic".to_owned());
        }
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        self
            .requests
            .send(StopHotkeyRequest::Shutdown(reply_sender))
            .map_err(|_| "a thread do atalho global não está ativa".to_owned())?;
        let shutdown_result = match reply_receiver.recv_timeout(STOP_HOTKEY_SHUTDOWN_TIMEOUT) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err("a limpeza do atalho global excedeu o prazo".to_owned())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("a thread do atalho terminou sem confirmar a limpeza".to_owned())
            }
        };
        let deadline = Instant::now() + Duration::from_millis(500);
        while !self.thread.as_ref().is_some_and(JoinHandle::is_finished)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(5));
        }
        if !self.thread.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err("a thread confirmou a limpeza, mas ainda não encerrou".to_owned());
        }
        let join_result = self
            .thread
            .take()
            .expect("thread reported finished")
            .join()
            .map_err(|_| "a thread do atalho global entrou em panic".to_owned());
        match (shutdown_result, join_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(join_error)) => Err(format!("{error}; {join_error}")),
        }
    }
}

impl Drop for GlobalStopShortcutManager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn stop_hotkey_message_thread(
    requests: Receiver<StopHotkeyRequest>,
    startup: SyncSender<(u32, Result<(), String>)>,
    initial: StopShortcut,
    callback: Arc<dyn Fn() + Send + Sync>,
) {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, HWND::default(), 0, 0, PM_NOREMOVE);
    }
    let thread_id = unsafe { GetCurrentThreadId() };
    let mut registered = None::<(StopShortcut, i32)>;
    let initial_id = STOP_HOTKEY_ID_A;
    let initial_result = register_stop_hotkey(&initial, initial_id).map(|()| {
        registered = Some((initial, initial_id));
    });
    if startup.send((thread_id, initial_result.clone())).is_err() {
        if let Some((_, id)) = registered {
            let _ = unsafe { UnregisterHotKey(HWND::default(), id) };
        }
        return;
    }

    loop {
        while unsafe { PeekMessageW(&mut message, HWND::default(), 0, 0, PM_REMOVE).0 != 0 } {
            if message.message == WM_QUIT {
                if let Some((_, id)) = registered.take() {
                    let _ = unsafe { UnregisterHotKey(HWND::default(), id) };
                }
                return;
            }
            if message.message == WM_HOTKEY
                && registered
                    .as_ref()
                    .is_some_and(|(_, id)| message.wParam.0 as i32 == *id)
            {
                callback();
            } else {
                unsafe {
                    let _ = TranslateMessage(&message);
                    let _ = DispatchMessageW(&message);
                }
            }
        }

        let request = match requests.recv_timeout(Duration::from_millis(20)) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match request {
            StopHotkeyRequest::Set {
                shortcut,
                lifecycle,
                deadline,
                reply,
            } => {
                let result = apply_stop_hotkey(shortcut, &mut registered, &lifecycle, deadline);
                let _ = reply.send(result);
            }
            StopHotkeyRequest::Shutdown(reply) => {
                let result = registered.take().map_or(Ok(()), |(_, id)| {
                    unsafe { UnregisterHotKey(HWND::default(), id) }
                        .map_err(|error| format!("UnregisterHotKey falhou ao sair: {error}"))
                });
                let _ = reply.send(result);
                return;
            }
        }
    }

    if let Some((_, id)) = registered {
        let _ = unsafe { UnregisterHotKey(HWND::default(), id) };
    }
}

fn apply_stop_hotkey(
    shortcut: StopShortcut,
    registered: &mut Option<(StopShortcut, i32)>,
    lifecycle: &AtomicU8,
    deadline: Instant,
) -> Result<(), String> {
    if lifecycle.load(Ordering::Acquire) != REQUEST_PENDING || Instant::now() >= deadline {
        return Err("o registro do atalho expirou; a troca foi cancelada".to_owned());
    }
    if registered
        .as_ref()
        .is_some_and(|(current, _)| current == &shortcut)
    {
        return Ok(());
    }
    let next_id = match registered.as_ref().map(|(_, id)| *id) {
        Some(STOP_HOTKEY_ID_A) => STOP_HOTKEY_ID_B,
        _ => STOP_HOTKEY_ID_A,
    };
    register_stop_hotkey(&shortcut, next_id)?;
    let claimed_commit = Instant::now() < deadline
        && lifecycle
            .compare_exchange(
                REQUEST_PENDING,
                REQUEST_COMMITTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok();
    if !claimed_commit || Instant::now() >= deadline {
        if claimed_commit {
            let _ = lifecycle.compare_exchange(
                REQUEST_COMMITTING,
                REQUEST_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        let rollback = unsafe { UnregisterHotKey(HWND::default(), next_id) };
        let cleanup = rollback
            .err()
            .map(|error| format!("; falha ao liberar o registro temporário: {error}"))
            .unwrap_or_default();
        return Err(format!("o registro do atalho expirou; a troca foi cancelada{cleanup}"));
    }
    if let Some((_, old_id)) = registered.as_ref() {
        if let Err(error) = unsafe { UnregisterHotKey(HWND::default(), *old_id) } {
            let rollback = unsafe { UnregisterHotKey(HWND::default(), next_id) };
            let cleanup = rollback
                .err()
                .map(|error| format!("; falha ao remover também o novo atalho: {error}"))
                .unwrap_or_default();
            return Err(format!("não foi possível substituir o atalho anterior: {error}{cleanup}"));
        }
    }
    *registered = Some((shortcut, next_id));
    Ok(())
}

fn register_stop_hotkey(shortcut: &StopShortcut, id: i32) -> Result<(), String> {
    let modifiers = shortcut.modifiers | MOD_NOREPEAT.0;
    match unsafe {
        RegisterHotKey(
            HWND::default(),
            id,
            HOT_KEY_MODIFIERS(modifiers),
            shortcut.virtual_key,
        )
    } {
        Ok(()) => Ok(()),
        Err(error) => {
            let hresult = error.code().0 as u32;
            let code = if hresult & 0xFFFF_0000 == 0x8007_0000 {
                hresult & 0x0000_FFFF
            } else {
                unsafe { GetLastError().0 }
            };
            if code == 1409 {
                Err(format!(
                    "não foi possível registrar {}: outro aplicativo já registrou essa combinação (Windows error 1409)",
                    shortcut.shortcut
                ))
            } else {
                Err(format!(
                    "RegisterHotKey falhou para {} (Windows error 0x{code:08X}): {error}",
                    shortcut.shortcut
                ))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum ListenerError {
    ThreadSpawn(String),
    RegistrationFailed(String),
    WindowClassFailed(String),
    WindowCreateFailed(String),
    MessageLoopFailed(String),
    CleanupFailed(String),
    ShutdownSignalFailed(String),
    ThreadPanicked,
    InvalidCapacity,
    AlreadyRunning,
}

impl fmt::Display for ListenerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(msg) => write!(f, "failed to spawn listener thread: {msg}"),
            Self::RegistrationFailed(msg) => write!(f, "failed to register raw input: {msg}"),
            Self::WindowClassFailed(msg) => write!(f, "failed to register window class: {msg}"),
            Self::WindowCreateFailed(msg) => write!(f, "failed to create message window: {msg}"),
            Self::MessageLoopFailed(msg) => write!(f, "Raw Input message loop failed: {msg}"),
            Self::CleanupFailed(msg) => write!(f, "Raw Input listener cleanup failed: {msg}"),
            Self::ShutdownSignalFailed(msg) => write!(f, "could not stop Raw Input listener: {msg}"),
            Self::ThreadPanicked => f.write_str("Raw Input listener thread panicked"),
            Self::InvalidCapacity => f.write_str("event queue capacity must be greater than zero"),
            Self::AlreadyRunning => f.write_str("a Raw Input mouse listener is already active"),
        }
    }
}

impl std::error::Error for ListenerError {}

#[derive(Default)]
pub struct WindowsInputBackend {
    logs: Mutex<BoundedLog>,
    keyboard_capture: Arc<AtomicBool>,
}

impl WindowsInputBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the current bounded log for display in the script editor.
    pub fn log_snapshot(&self) -> Vec<String> {
        self.lock_logs().snapshot()
    }

    /// Returns only log entries newer than a process-wide sequence cursor.
    pub fn log_entries_since(&self, sequence: u64) -> Vec<LogEntry> {
        self.lock_logs().entries_since(sequence)
    }

    /// Returns the oldest and newest retained process-wide log sequence.
    pub fn log_sequence_bounds(&self) -> (Option<u64>, Option<u64>) {
        self.lock_logs().bounds()
    }

    /// Takes a bounded incremental snapshot and its retention range atomically.
    pub fn log_delta_since(&self, sequence: u64) -> (Option<u64>, Option<u64>, Vec<LogEntry>) {
        let logs = self.lock_logs();
        let (oldest, newest) = logs.bounds();
        (oldest, newest, logs.entries_since(sequence))
    }

    /// Copies retained entries when they must be archived at script shutdown.
    pub fn log_entries_snapshot(&self) -> Vec<LogEntry> {
        self.lock_logs().entries_since(0)
    }

    fn lock_logs(&self) -> MutexGuard<'_, BoundedLog> {
        self.logs.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn send_mouse_input(spec: MouseInputSpec) -> Result<(), BackendError> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: spec.dx,
                    dy: spec.dy,
                    mouseData: spec.data,
                    dwFlags: MOUSE_EVENT_FLAGS(spec.flags),
                    time: 0,
                    dwExtraInfo: INPUT_INJECTION_TAG as usize,
                },
            },
        };

        let count = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        let last_error = if count == 0 {
            unsafe { GetLastError().0 }
        } else {
            0
        };
        check_send_input_count(
            1,
            count,
            (count == 0).then_some(last_error),
        )
        .map_err(BackendError)
    }

    fn send_keyboard_input(key: KeyCode, key_up: bool) -> Result<(), BackendError> {
        if key.scan_code == 0 || key.scan_code > 0x00FF {
            return Err(BackendError("scan code must be between 1 and 255".into()));
        }
        let flags = key_event_flags(key.extended, key_up);
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: key.scan_code,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: INPUT_INJECTION_TAG as usize,
                },
            },
        };
        Self::send_inputs(&[input])
    }

    fn send_inputs(inputs: &[INPUT]) -> Result<(), BackendError> {
        let requested = u32::try_from(inputs.len())
            .map_err(|_| BackendError("too many Windows input events".into()))?;
        if requested == 0 {
            return Ok(());
        }
        let inserted = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
        let last_error = if inserted == 0 {
            unsafe { GetLastError().0 }
        } else {
            0
        };
        check_send_input_count(
            requested,
            inserted,
            (inserted == 0).then_some(last_error),
        )
        .map_err(BackendError)
    }

    fn send_unicode_unit(unit: u16, key_up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(
                        unicode_event_flags(key_up) | KEYEVENTF_UNICODE.0,
                    ),
                    time: 0,
                    dwExtraInfo: INPUT_INJECTION_TAG as usize,
                },
            },
        }
    }

    pub fn keyboard_capture_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.keyboard_capture)
    }

    fn virtual_key_for_scan_code(key: KeyCode) -> Result<u16, BackendError> {
        if key.scan_code == 0 || key.scan_code > 0x00FF {
            return Err(BackendError("scan code must be between 1 and 255".into()));
        }
        let scan = u32::from(key.scan_code) | if key.extended { 0xE000 } else { 0 };
        let layout = unsafe { GetKeyboardLayout(0) };
        let virtual_key = unsafe { MapVirtualKeyExW(scan, MAPVK_VSC_TO_VK_EX, layout) };
        let virtual_key = virtual_key as u16;
        if virtual_key == 0 {
            return Err(BackendError(format!(
                "Windows has no virtual-key mapping for scan code 0x{:02X}{}",
                key.scan_code,
                if key.extended { " (extended)" } else { "" }
            )));
        }
        Ok(virtual_key)
    }
}

impl InputBackend for WindowsInputBackend {
    fn press_mouse_button(&self, button: u8) -> Result<(), BackendError> {
        let spec = mouse_button_input(button, true)
            .map_err(|e| BackendError(e.to_string()))?;
        Self::send_mouse_input(spec)
    }

    fn release_mouse_button(&self, button: u8) -> Result<(), BackendError> {
        let spec = mouse_button_input(button, false)
            .map_err(|e| BackendError(e.to_string()))?;
        Self::send_mouse_input(spec)
    }

    fn is_mouse_button_pressed(&self, button: u8) -> Result<bool, BackendError> {
        let vk = virtual_key_for_button(button)
            .ok_or_else(|| BackendError(format!("invalid button {button}")))?;
        let state = unsafe { GetAsyncKeyState(vk as i32) };
        Ok((state as u16 & 0x8000) != 0)
    }

    fn move_mouse_relative(&self, x: i32, y: i32) -> Result<(), BackendError> {
        let spec = relative_move_input(x, y);
        Self::send_mouse_input(spec)
    }

    fn output_log_message(&self, message: &str) -> Result<(), BackendError> {
        self.lock_logs().push(message);
        Ok(())
    }

    fn clear_log(&self) -> Result<(), BackendError> {
        self.lock_logs().clear();
        Ok(())
    }

    fn press_key(&self, key: KeyCode) -> Result<(), BackendError> {
        Self::send_keyboard_input(key, false)
    }

    fn release_key(&self, key: KeyCode) -> Result<(), BackendError> {
        Self::send_keyboard_input(key, true)
    }

    fn is_key_pressed(&self, key: KeyCode) -> Result<bool, BackendError> {
        let virtual_key = Self::virtual_key_for_scan_code(key)?;
        let state = unsafe { GetAsyncKeyState(i32::from(virtual_key)) };
        Ok((state as u16 & 0x8000) != 0)
    }

    fn is_key_lock_on(&self, key: &str) -> Result<bool, BackendError> {
        let virtual_key = match key.to_ascii_lowercase().as_str() {
            "capslock" | "caps_lock" => 0x14,
            "numlock" | "num_lock" => 0x90,
            "scrolllock" | "scroll_lock" => 0x91,
            _ => return Err(BackendError(format!("unsupported lock key '{key}'"))),
        };
        let state = unsafe { GetKeyState(virtual_key) };
        Ok((state as u16 & 1) != 0)
    }

    fn type_text(&self, text: &str) -> Result<(), BackendError> {
        let units = encode_unicode_text(text)
            .map_err(|error| BackendError(error.into()))?;
        let mut inputs = Vec::with_capacity(units.len().saturating_mul(2));
        for unit in units.iter().copied() {
            inputs.push(Self::send_unicode_unit(unit, false));
            inputs.push(Self::send_unicode_unit(unit, true));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let requested = inputs.len() as u32;
        let inserted = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
        if inserted == requested {
            return Ok(());
        }
        let mut cleanup_error = None;
        // Inputs are paired down/up. A partial insertion can leave the last
        // inserted Unicode unit down, so always send its matching up event.
        if inserted != 0 && inserted < requested && inserted % 2 == 1 {
            let unit_index = (inserted / 2) as usize;
            let release = Self::send_unicode_unit(units[unit_index], true);
            let released = unsafe { SendInput(&[release], size_of::<INPUT>() as i32) };
            let last_error = if released == 0 {
                unsafe { GetLastError().0 }
            } else {
                0
            };
            if let Err(error) = check_send_input_count(
                1,
                released,
                (released == 0).then_some(last_error),
            ) {
                cleanup_error = Some(error);
            }
        }
        let last_error = if inserted == 0 {
            unsafe { GetLastError().0 }
        } else {
            0
        };
        let error = check_send_input_count(
            requested,
            inserted,
            (inserted == 0).then_some(last_error),
        )
        .expect_err("the input count was already checked as partial");
        Err(BackendError(match cleanup_error {
            Some(cleanup_error) => format!("{error}; Unicode key release also failed: {cleanup_error}"),
            None => error,
        }))
    }

    fn move_mouse_to(&self, x: i32, y: i32) -> Result<(), BackendError> {
        let bounds = screen_rect()?;
        let spec = absolute_move_input(
            x,
            y,
            (bounds.left, bounds.top, bounds.width, bounds.height),
        )
        .map_err(|error| BackendError(error.into()))?;
        Self::send_mouse_input(spec)
    }

    fn mouse_position(&self) -> Result<MousePosition, BackendError> {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }
            .map_err(|error| BackendError(format!("GetCursorPos failed: {error}")))?;
        Ok(MousePosition { x: point.x, y: point.y })
    }

    fn screen_rect(&self) -> Result<ScreenRect, BackendError> {
        screen_rect()
    }

    fn scroll_mouse(&self, vertical: i32, horizontal: i32) -> Result<(), BackendError> {
        let mut inputs = Vec::with_capacity(2);
        if vertical != 0 {
            inputs.push(mouse_input(
                wheel_input(vertical, false)
                    .map_err(|error| BackendError(error.into()))?,
            ));
        }
        if horizontal != 0 {
            inputs.push(mouse_input(
                wheel_input(horizontal, true)
                    .map_err(|error| BackendError(error.into()))?,
            ));
        }
        Self::send_inputs(&inputs)
    }

    fn enable_keyboard_events(&self, enabled: bool) -> Result<(), BackendError> {
        self.keyboard_capture.store(enabled, Ordering::Release);
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
}

fn mouse_input(spec: MouseInputSpec) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: spec.dx,
                dy: spec.dy,
                mouseData: spec.data,
                dwFlags: MOUSE_EVENT_FLAGS(spec.flags),
                time: 0,
                dwExtraInfo: INPUT_INJECTION_TAG as usize,
            },
        },
    }
}

fn screen_rect() -> Result<ScreenRect, BackendError> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return Err(BackendError(format!(
            "Windows returned invalid virtual desktop bounds {width}x{height}"
        )));
    }
    Ok(ScreenRect { left, top, width, height })
}

pub struct RawInputListener {
    events: Receiver<RawInputEvent>,
    window: isize,
    thread_id: u32,
    thread_handle: Option<JoinHandle<Result<(), ListenerError>>>,
    dropped_events: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawInputEvent {
    MouseButton(super::MouseButtonTransition),
    Wheel { vertical: i32, horizontal: i32 },
    Key { key: KeyCode, pressed: bool },
}

impl RawInputEvent {
    fn into_script_event(self) -> ScriptEvent {
        match self {
            Self::MouseButton(transition) => ScriptEvent::new(
                transition.event_name(),
                Some(i64::from(transition.button)),
                Some(transition.family()),
            ),
            Self::Wheel { vertical, horizontal } => ScriptEvent::new(
                if vertical != 0 { "MOUSE_WHEEL" } else { "MOUSE_HWHEEL" },
                Some(i64::from(if vertical != 0 { vertical } else { horizontal })),
                Some("mouse"),
            ),
            Self::Key { key, pressed } => ScriptEvent::new(
                if pressed { "KEY_PRESSED" } else { "KEY_RELEASED" },
                Some(i64::from(key.scan_code)),
                Some("keyboard"),
            )
            .with_key(key),
        }
    }
}

impl RawInputListener {
    /// Explicitly starts a bounded Raw Input capture queue.
    pub fn start(queue_capacity: usize) -> Result<Self, ListenerError> {
        Self::start_with_keyboard_capture(queue_capacity, Arc::new(AtomicBool::new(false)))
    }

    /// Starts a bounded mouse and optional keyboard Raw Input capture queue.
    /// Keyboard transitions are ignored unless the shared flag is enabled.
    pub fn start_with_keyboard_capture(
        queue_capacity: usize,
        keyboard_capture: Arc<AtomicBool>,
    ) -> Result<Self, ListenerError> {
        if queue_capacity == 0 {
            return Err(ListenerError::InvalidCapacity);
        }
        if RAW_INPUT_LISTENER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ListenerError::AlreadyRunning);
        }

        let (event_sender, events) = mpsc::sync_channel(queue_capacity);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let thread_dropped = Arc::clone(&dropped_events);
        let (startup_sender, startup_receiver) = mpsc::sync_channel(1);
        let join = match thread::Builder::new()
            .name("windows-raw-input-listener".to_owned())
            .spawn(move || {
                let _active = ActiveListenerGuard;
                run_listener_loop(
                    event_sender,
                    thread_dropped,
                    startup_sender,
                    keyboard_capture,
                )
            }) {
            Ok(join) => join,
            Err(error) => {
                // The worker guard has not been constructed if spawning fails.
                RAW_INPUT_LISTENER_ACTIVE.store(false, Ordering::Release);
                return Err(ListenerError::ThreadSpawn(error.to_string()));
            }
        };

        let (window, thread_id) = match startup_receiver.recv() {
            Ok(Ok(startup)) => startup,
            Ok(Err(error)) => {
                let _ = join.join();
                return Err(error);
            }
            Err(_) => {
                return match join.join() {
                    Ok(Err(error)) => Err(error),
                    Ok(Ok(())) => Err(ListenerError::ThreadSpawn(
                        "listener exited before reporting startup".into(),
                    )),
                    Err(_) => Err(ListenerError::ThreadPanicked),
                };
            }
        };

        Ok(Self {
            events,
            window,
            thread_id,
            thread_handle: Some(join),
            dropped_events,
        })
    }

    /// Receives the next G HUB-compatible event without blocking.
    pub fn try_recv(&self) -> Result<ScriptEvent, TryRecvError> {
        self.try_recv_raw().map(RawInputEvent::into_script_event)
    }

    /// Receives a typed physical input event without blocking.
    pub fn try_recv_raw(&self) -> Result<RawInputEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Reports physical transitions rejected because the bounded queue was full.
    pub fn dropped_event_count(&self) -> u64 {
        self.dropped_events.load(Ordering::Relaxed)
    }

    /// Signals the message window, unregisters Raw Input, and joins the listener thread.
    pub fn stop(&mut self) -> Result<(), ListenerError> {
        let Some(handle) = self.thread_handle.as_ref() else {
            return Ok(());
        };

        if !handle.is_finished() {
            let window = HWND(self.window as *mut core::ffi::c_void);
            let window_result = unsafe {
                PostMessageW(window, STOP_LISTENER_MESSAGE, WPARAM(0), LPARAM(0))
            };
            if let Err(window_error) = window_result {
                let thread_result = unsafe {
                    PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
                };
                if let Err(thread_error) = thread_result {
                    if !handle.is_finished() {
                        return Err(ListenerError::ShutdownSignalFailed(format!(
                            "window post {}; thread post {}",
                            window_error,
                            thread_error
                        )));
                    }
                }
            }
        }

        let handle = self.thread_handle.take().expect("checked above");
        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(ListenerError::ThreadPanicked),
        }
    }
}

impl Drop for RawInputListener {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct ActiveListenerGuard;

impl Drop for ActiveListenerGuard {
    fn drop(&mut self) {
        RAW_INPUT_LISTENER_ACTIVE.store(false, Ordering::Release);
    }
}

fn get_device_name(handle: HANDLE) -> Option<String> {
    if handle.0.is_null() || handle.is_invalid() {
        return None;
    }
    let mut size = 0u32;
    unsafe {
        let res = GetRawInputDeviceInfoW(
            handle,
            RIDI_DEVICENAME,
            None,
            &mut size,
        );
        if res == u32::MAX || size == 0 {
            return None;
        }

        let mut buffer = vec![0u16; size as usize + 1];
        let mut capacity = buffer.len().min(u32::MAX as usize) as u32;
        let res = GetRawInputDeviceInfoW(
            handle,
            RIDI_DEVICENAME,
            Some(buffer.as_mut_ptr() as *mut _),
            &mut capacity,
        );
        if res == u32::MAX {
            return None;
        }

        let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..len]))
    }
}

unsafe extern "system" fn listener_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn run_listener_loop(
    sender: SyncSender<RawInputEvent>,
    dropped_events: Arc<AtomicU64>,
    startup: SyncSender<Result<(isize, u32), ListenerError>>,
    keyboard_capture: Arc<AtomicBool>,
) -> Result<(), ListenerError> {
    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(handle) => handle,
        Err(error) => {
            let listener_error = ListenerError::WindowCreateFailed(error.to_string());
            let _ = startup.send(Err(ListenerError::WindowCreateFailed(error.to_string())));
            return Err(listener_error);
        }
    };
    let class_name = w!("AtkRawInputMessageWindow");

    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: Default::default(),
        lpfnWndProc: Some(listener_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance.into(),
        hIcon: Default::default(),
        hCursor: Default::default(),
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: class_name,
        hIconSm: Default::default(),
    };

    let class_atom = unsafe { RegisterClassExW(&wnd_class) };
    if class_atom == 0 {
        let error = ListenerError::WindowClassFailed(format!(
            "RegisterClassExW failed with 0x{:08X}",
            unsafe { GetLastError().0 }
        ));
        let _ = startup.send(Err(ListenerError::WindowClassFailed(error.to_string())));
        return Err(error);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            HMENU::default(),
            hinstance,
            None,
        )
    };

    let hwnd = match hwnd {
        Ok(h) => h,
        Err(e) => {
            let primary = ListenerError::WindowCreateFailed(e.to_string());
            let cleanup = unsafe { UnregisterClassW(class_name, hinstance) }
                .map_err(|error| ListenerError::CleanupFailed(format!(
                    "UnregisterClassW failed after window creation error: {error}"
                )));
            let error = combine_listener_results(Err(primary), cleanup)
                .expect_err("the window creation error remains primary");
            let _ = startup.send(Err(error.clone()));
            return Err(error);
        }
    };

    // Register mouse and keyboard. Keyboard records are filtered against the
    // shared opt-in flag before entering the bounded event queue.
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01, // Generic Desktop Controls
            usUsage: 0x02,     // Mouse
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01, // Generic Desktop Controls
            usUsage: 0x06,     // Keyboard
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
    ];

    let reg_ok = unsafe {
        RegisterRawInputDevices(
            &devices,
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
    };

    if let Err(e) = reg_ok {
        let primary = ListenerError::RegistrationFailed(e.to_string());
        let cleanup = destroy_window_and_class(hwnd, class_name, hinstance);
        let error = combine_listener_results(Err(primary), cleanup)
            .expect_err("the registration error remains primary");
        let _ = startup.send(Err(error.clone()));
        return Err(error);
    }

    let thread_id = unsafe { GetCurrentThreadId() };
    if startup.send(Ok((hwnd.0 as isize, thread_id))).is_err() {
        return cleanup_listener(hwnd, class_name, hinstance);
    }

    let message_result = raw_input_message_loop(hwnd, &sender, &dropped_events, &keyboard_capture);
    let cleanup = cleanup_listener(hwnd, class_name, hinstance);
    combine_listener_results(message_result, cleanup)
}

fn raw_input_message_loop(
    hwnd: HWND,
    sender: &SyncSender<RawInputEvent>,
    dropped_events: &AtomicU64,
    keyboard_capture: &AtomicBool,
) -> Result<(), ListenerError> {
    loop {
        let mut message = MSG::default();
        let result = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
        match result.0 {
            -1 => {
                return Err(ListenerError::MessageLoopFailed(format!(
                    "GetMessageW failed with 0x{:08X}",
                    unsafe { GetLastError().0 }
                )));
            }
            0 => return Ok(()),
            _ if message.message == STOP_LISTENER_MESSAGE && message.hwnd == hwnd => {
                return Ok(())
            }
            _ if message.message == WM_QUIT => return Ok(()),
            _ if message.message == WM_INPUT => {
                dispatch_raw_input(
                    message.lParam,
                    sender,
                    dropped_events,
                    keyboard_capture.load(Ordering::Acquire),
                );
                // DefWindowProc performs required foreground WM_INPUT cleanup.
                let _ = unsafe {
                    DefWindowProcW(hwnd, message.message, message.wParam, message.lParam)
                };
            }
            _ => unsafe {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            },
        }
    }
}

fn dispatch_raw_input(
    raw_handle: LPARAM,
    sender: &SyncSender<RawInputEvent>,
    dropped_events: &AtomicU64,
    capture_keyboard: bool,
) {
    let raw_handle = HRAWINPUT(raw_handle.0 as *mut core::ffi::c_void);
    let mut reported_size = 0u32;
    let query_result = unsafe {
        GetRawInputData(
            raw_handle,
            RID_INPUT,
            None,
            &mut reported_size,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if query_result == u32::MAX {
        return;
    }

    let Some(storage_capacity) = raw_input_storage_capacity(
        reported_size as usize,
        size_of::<RAWINPUTHEADER>(),
        size_of::<RAWINPUT>(),
        MAX_RAW_INPUT_BYTES,
    ) else {
        return;
    };

    // RAWINPUT contains native pointers/unions. Allocate at least the full
    // union size for alignment, even when the actual keyboard record is shorter.
    let word_count = storage_capacity.div_ceil(size_of::<usize>());
    let mut aligned_storage = vec![MaybeUninit::<usize>::zeroed(); word_count];
    let storage_bytes = word_count.saturating_mul(size_of::<usize>());
    let mut available = match u32::try_from(storage_bytes) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let copied = unsafe {
        GetRawInputData(
            raw_handle,
            RID_INPUT,
            Some(aligned_storage.as_mut_ptr().cast()),
            &mut available,
            size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    if copied == u32::MAX
        || copied == 0
        || copied as usize > storage_bytes
        || available as usize > storage_bytes
    {
        return;
    }

    let record_size = copied as usize;
    if record_size < size_of::<RAWINPUTHEADER>() {
        return;
    }
    // Only read the header after validating its actual byte count. Do not form
    // a reference to RAWINPUT until the type-specific payload length is known.
    let header = unsafe {
        aligned_storage
            .as_ptr()
            .cast::<RAWINPUTHEADER>()
            .read_unaligned()
    };
    if header.dwSize as usize != record_size
        || !raw_input_record_is_complete(
            record_size,
            header.dwType,
            size_of::<RAWINPUTHEADER>(),
            size_of::<RAWMOUSE>(),
            size_of::<RAWKEYBOARD>(),
        )
    {
        return;
    }

    let device = header.hDevice;
    let payload = unsafe {
        aligned_storage
            .as_ptr()
            .cast::<u8>()
            .add(size_of::<RAWINPUTHEADER>())
    };
    match header.dwType {
        kind if kind == RIM_TYPEMOUSE.0 => {
            if device.0.is_null() || device.is_invalid() {
                return;
            }
            let device_path = get_device_name(device);
            if !should_capture_device(true, device_path.as_deref()) {
                return;
            }
            let mouse = unsafe { payload.cast::<RAWMOUSE>().read_unaligned() };
            if mouse.ulExtraInformation == INPUT_INJECTION_TAG {
                return;
            }
            let button_data = unsafe { mouse.Anonymous.Anonymous };
            for transition in raw_mouse_button_transitions(u32::from(button_data.usButtonFlags)) {
                send_raw_event(sender, dropped_events, RawInputEvent::MouseButton(transition));
            }
            let (vertical, horizontal) =
                raw_wheel_deltas(button_data.usButtonFlags, button_data.usButtonData);
            if let Some(vertical) = vertical {
                send_raw_event(
                    sender,
                    dropped_events,
                    RawInputEvent::Wheel {
                        vertical,
                        horizontal: 0,
                    },
                );
            }
            if let Some(horizontal) = horizontal {
                send_raw_event(
                    sender,
                    dropped_events,
                    RawInputEvent::Wheel {
                        vertical: 0,
                        horizontal,
                    },
                );
            }
        }
        kind if kind == RIM_TYPEKEYBOARD.0 && capture_keyboard => {
            if device.0.is_null() || device.is_invalid() {
                return;
            }
            let keyboard = unsafe { payload.cast::<RAWKEYBOARD>().read_unaligned() };
            if let Some((scan_code, extended, pressed)) = raw_key_parts(
                keyboard.MakeCode,
                keyboard.Flags,
                keyboard.ExtraInformation,
            ) {
                send_raw_event(
                    sender,
                    dropped_events,
                    RawInputEvent::Key {
                        key: KeyCode {
                            scan_code,
                            extended,
                        },
                        pressed,
                    },
                );
            }
        }
        _ => {}
    }
}

fn send_raw_event(
    sender: &SyncSender<RawInputEvent>,
    dropped_events: &AtomicU64,
    event: RawInputEvent,
) {
    match sender.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            dropped_events.fetch_add(1, Ordering::Relaxed);
        }
        Err(TrySendError::Disconnected(_)) => {}
    }
}

fn cleanup_listener(hwnd: HWND, class_name: PCWSTR, hinstance: windows::Win32::Foundation::HMODULE) -> Result<(), ListenerError> {
    let removals = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        },
    ];
    let unregister_input = unsafe {
        RegisterRawInputDevices(&removals, size_of::<RAWINPUTDEVICE>() as u32)
    }
    .map_err(|error| ListenerError::CleanupFailed(format!(
        "RIDEV_REMOVE failed: {error}"
    )));
    let destroy_window = unsafe { DestroyWindow(hwnd) }
        .map_err(|error| ListenerError::CleanupFailed(format!("DestroyWindow failed: {error}")));
    let unregister_class = unsafe { UnregisterClassW(class_name, hinstance) }
        .map_err(|error| ListenerError::CleanupFailed(format!("UnregisterClassW failed: {error}")));

    combine_listener_results(
        combine_listener_results(unregister_input, destroy_window),
        unregister_class,
    )
}

fn destroy_window_and_class(
    hwnd: HWND,
    class_name: PCWSTR,
    hinstance: windows::Win32::Foundation::HMODULE,
) -> Result<(), ListenerError> {
    let destroy_window = unsafe { DestroyWindow(hwnd) }
        .map_err(|error| ListenerError::CleanupFailed(format!("DestroyWindow failed: {error}")));
    let unregister_class = unsafe { UnregisterClassW(class_name, hinstance) }
        .map_err(|error| ListenerError::CleanupFailed(format!("UnregisterClassW failed: {error}")));
    combine_listener_results(destroy_window, unregister_class)
}

fn combine_listener_results(
    primary: Result<(), ListenerError>,
    cleanup: Result<(), ListenerError>,
) -> Result<(), ListenerError> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(ListenerError::CleanupFailed(format!(
            "{error}; cleanup also failed: {cleanup}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GlobalStopShortcutManager, RawInputEvent, STOP_HOTKEY_ID_A, WM_HOTKEY,
    };
    use ghub_lua::KeyCode;
    use crate::{parse_stop_shortcut, MouseButtonTransition, StopShortcut};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;

    #[test]
    fn new_api_raw_event_conversion_preserves_scan_code_and_wheel_axis() {
        let key = KeyCode {
            scan_code: 0x1D,
            extended: true,
        };
        let press = RawInputEvent::Key { key, pressed: true }.into_script_event();
        assert_eq!(press.name, "KEY_PRESSED");
        assert_eq!(press.arg, Some(0x1D));
        assert_eq!(press.family.as_deref(), Some("keyboard"));
        assert_eq!(press.key, Some(key));

        let vertical = RawInputEvent::Wheel {
            vertical: -120,
            horizontal: 0,
        }
        .into_script_event();
        assert_eq!(vertical.name, "MOUSE_WHEEL");
        assert_eq!(vertical.arg, Some(-120));

        let horizontal = RawInputEvent::Wheel {
            vertical: 0,
            horizontal: 60,
        }
        .into_script_event();
        assert_eq!(horizontal.name, "MOUSE_HWHEEL");
        assert_eq!(horizontal.arg, Some(60));

        let button = RawInputEvent::MouseButton(MouseButtonTransition {
            button: 4,
            pressed: false,
        })
        .into_script_event();
        assert_eq!(button.name, "MOUSE_BUTTON_RELEASED");
        assert_eq!(button.arg, Some(4));
    }

    #[test]
    fn global_stop_shortcut_registration_conflict_rollback_hotkey_and_cleanup() {
        fn start_available(
            skip: Option<&str>,
            on_hotkey: impl Fn() + Send + Sync + 'static,
        ) -> Option<(GlobalStopShortcutManager, StopShortcut)> {
            let on_hotkey = Arc::new(on_hotkey);
            for number in 13..=24 {
                let shortcut = parse_stop_shortcut(&format!("Ctrl+Alt+Shift+F{number}")).unwrap();
                if skip == Some(shortcut.shortcut.as_str()) {
                    continue;
                }
                let callback = Arc::clone(&on_hotkey);
                let (mut manager, registration) = match GlobalStopShortcutManager::start(
                    shortcut.clone(),
                    move || {
                        callback();
                    },
                ) {
                    Ok(result) => result,
                    Err(_) => continue,
                };
                if registration.is_ok() {
                    return Some((manager, shortcut));
                }
                let _ = manager.stop();
            }
            None
        }

        let callbacks = Arc::new(AtomicU64::new(0));
        let callback_counter = Arc::clone(&callbacks);
        let Some((mut first, first_shortcut)) = start_available(None, move || {
            callback_counter.fetch_add(1, Ordering::AcqRel);
        }) else {
            eprintln!("skipping Win32 hotkey lifecycle checks: no free F13-F24 shortcut found");
            return;
        };
        let Some((mut second, second_shortcut)) = start_available(
            Some(&first_shortcut.shortcut),
            || {},
        ) else {
            let _ = first.stop();
            eprintln!("skipping Win32 hotkey lifecycle checks: only one free shortcut found");
            return;
        };

        unsafe {
            PostThreadMessageW(
                first.thread_id,
                WM_HOTKEY,
                super::WPARAM(STOP_HOTKEY_ID_A as usize),
                LPARAM(0),
            )
            .expect("posting an internal WM_HOTKEY message should succeed");
        }
        let callback_deadline = Instant::now() + Duration::from_secs(1);
        while callbacks.load(Ordering::Acquire) == 0 && Instant::now() < callback_deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(callbacks.load(Ordering::Acquire), 1);

        assert!(first.set(second_shortcut.clone()).is_err());
        let (mut rollback_probe, rollback_registration) =
            GlobalStopShortcutManager::start(first_shortcut.clone(), || {}).unwrap();
        assert!(rollback_registration.is_err());
        rollback_probe.stop().unwrap();

        second.stop().unwrap();
        first.set(second_shortcut.clone()).unwrap();
        first.set(second_shortcut.clone()).unwrap();
        first.stop().unwrap();

        let (mut cleanup_probe, cleanup_registration) =
            GlobalStopShortcutManager::start(second_shortcut, || {}).unwrap();
        cleanup_registration.expect("stop should unregister the current global shortcut");
        cleanup_probe.stop().unwrap();
    }
}
