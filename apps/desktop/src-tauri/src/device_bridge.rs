use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::cell::RefCell;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

const SELECTION_FILE: &str = "device_selection.json";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);
const HOTPLUG_SETTLE_INTERVAL: Duration = Duration::from_millis(60);
const IDLE_REPORT_POLL_INTERVAL: Duration = Duration::from_millis(30);
const RECONNECT_INTERVAL: Duration = Duration::from_millis(750);
const ACTOR_QUEUE_CAPACITY: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceMemoryState {
    /// Only strict, physically enumerated target receivers are candidates.
    pub candidates: Vec<atk_device::TargetDevice>,
    /// Present only when the persisted identity resolves to exactly one
    /// currently connected receiver.
    pub selected: Option<atk_device::TargetDevice>,
    pub remembered: bool,
    pub connected: bool,
    pub opened: bool,
    pub ambiguous: bool,
    pub error: Option<String>,
    /// At least one compatible target receiver is physically enumerated.
    pub receiver_enumerated: bool,
    /// A valid CID/MID identity response belongs to the currently selected receiver.
    pub identity_responded: bool,
    pub diagnostic_status: DiagnosticStatus,
    pub last_check_at_ms: Option<u64>,
    pub communication_error: Option<String>,
    pub transport: Option<&'static str>,
    pub charging: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    #[default]
    NotChecked,
    Responded,
    Failed,
    Disconnected,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DeviceDiagnostics {
    /// Retained across disconnects so reconnecting cannot revive an invalid proof.
    target_device_key: Option<String>,
    identity_device_key: Option<String>,
    status: DiagnosticStatus,
    last_check_at_ms: Option<u64>,
    communication_error: Option<String>,
    charging: Option<bool>,
}

impl DeviceDiagnostics {
    fn record_response(&mut self, device_key: &str, identity: Option<(u8, u8)>) {
        self.target_device_key = Some(device_key.to_owned());
        self.last_check_at_ms = Some(unix_time_ms());
        match identity {
            Some(_) => {
                self.identity_device_key = Some(device_key.to_owned());
                self.status = DiagnosticStatus::Responded;
                self.communication_error = None;
            }
            None => {
                self.identity_device_key = None;
                self.status = DiagnosticStatus::Failed;
                self.communication_error = Some(
                    "A resposta recebida não incluiu uma identidade válida do mouse.".to_owned(),
                );
            }
        }
    }

    fn record_failure(&mut self, device_key: Option<&str>, error: &str) {
        if let Some(device_key) = device_key {
            self.target_device_key = Some(device_key.to_owned());
        }
        self.identity_device_key = None;
        self.charging = None;
        self.status = DiagnosticStatus::Failed;
        self.last_check_at_ms = Some(unix_time_ms());
        self.communication_error = Some(error.to_owned());
    }

    fn invalidate_for_disconnect(&mut self) {
        self.identity_device_key = None;
        self.charging = None;
        if self.target_device_key.is_some() {
            self.status = DiagnosticStatus::Disconnected;
        }
    }

    fn reset_for_device_change(&mut self) {
        *self = Self::default();
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceMemoryInputReport {
    device_key: String,
    report_id: u8,
    data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SavedSelection {
    schema_version: u8,
    device_key: String,
    serial_number: Option<String>,
    #[serde(default)]
    receiver_key: Option<String>,
    #[serde(default)]
    wired_key: Option<String>,
}

enum SelectionResolution<'a> {
    Unremembered,
    Connected(&'a atk_device::TargetDevice),
    Reidentified(&'a atk_device::TargetDevice),
    Disconnected,
    Ambiguous,
}

enum ActorRequest {
    State(SyncSender<Result<DeviceMemoryState, String>>),
    Remember {
        device_key: Option<String>,
        serial_number: Option<String>,
        reply: SyncSender<Result<DeviceMemoryState, String>>,
    },
    Clear(SyncSender<Result<DeviceMemoryState, String>>),
    Open {
        device_key: Option<String>,
        reply: SyncSender<Result<DeviceMemoryState, String>>,
    },
    Close {
        device_key: Option<String>,
        reply: SyncSender<Result<DeviceMemoryState, String>>,
    },
    SendReport {
        device_key: Option<String>,
        report_id: u8,
        data: Vec<u8>,
        reply: SyncSender<Result<(), String>>,
    },
    ReadTargetInfo {
        device_key: Option<String>,
        reply: SyncSender<Result<atk_device::DeviceInfo, String>>,
    },
    Shutdown,
    InventoryChanged,
}

struct ActorEnvelope {
    deadline: Instant,
    request: ActorRequest,
}

/// Owns the only native HID actor used by the WebHID adapter and `device_info`.
/// The actor serializes all reads and writes, including idle 0x0A reports.
pub struct DeviceMemoryBridge {
    sender: SyncSender<ActorEnvelope>,
    actor_thread: Mutex<Option<JoinHandle<()>>>,
    cached_state: Arc<Mutex<Option<DeviceMemoryState>>>,
}

impl DeviceMemoryBridge {
    pub fn new(app: AppHandle) -> Result<Self, String> {
        let selection_path = app
            .path()
            .app_local_data_dir()
            .map_err(|error| format!("Não foi possível localizar os dados locais do ATK HUB: {error}"))?
            .join(SELECTION_FILE);
        if let Some(parent) = selection_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("Não foi possível preparar a persistência do mouse: {error}"))?;
        }
        let (selection, startup_error) = match load_selection(&selection_path) {
            Ok(selection) => (selection, None),
            Err(error) => (None, Some(error)),
        };
        let (sender, receiver) = mpsc::sync_channel(ACTOR_QUEUE_CAPACITY);
        let cached_state = Arc::new(Mutex::new(None));
        let actor_cache = Arc::clone(&cached_state);
        let actor_sender = sender.clone();
        let actor_thread = thread::Builder::new()
            .name("atk-device-memory".to_owned())
            .spawn(move || actor_loop(app, selection_path, selection, startup_error, receiver, actor_sender, actor_cache))
            .map_err(|error| format!("Não foi possível iniciar o actor HID do mouse: {error}"))?;
        Ok(Self {
            sender,
            actor_thread: Mutex::new(Some(actor_thread)),
            cached_state,
        })
    }

    async fn request<T: Send + 'static>(
        &self,
        build: impl FnOnce(SyncSender<Result<T, String>>) -> ActorRequest,
    ) -> Result<T, String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let envelope = ActorEnvelope {
            deadline: Instant::now() + REQUEST_TIMEOUT,
            request: build(reply_tx),
        };
        match self.sender.try_send(envelope) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err("A fila de operações do mouse está cheia; tente novamente.".to_owned());
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err("O actor HID do mouse foi encerrado.".to_owned());
            }
        }
        tauri::async_runtime::spawn_blocking(move || reply_rx.recv_timeout(REQUEST_TIMEOUT))
            .await
            .map_err(|error| format!("A espera pela operação HID foi interrompida: {error}"))?
            .map_err(|error| match error {
                RecvTimeoutError::Timeout => "A operação HID excedeu o prazo de resposta.".to_owned(),
                RecvTimeoutError::Disconnected => "O actor HID do mouse encerrou antes de responder.".to_owned(),
            })?
    }

    pub async fn read_target_info_for(
        &self,
        device_key: Option<String>,
    ) -> Result<atk_device::DeviceInfo, String> {
        self.request(|reply| ActorRequest::ReadTargetInfo { device_key, reply }).await
    }
}

impl Drop for DeviceMemoryBridge {
    fn drop(&mut self) {
        let _ = self.sender.send(ActorEnvelope {
            deadline: Instant::now() + REQUEST_TIMEOUT,
            request: ActorRequest::Shutdown,
        });
        let actor = lock(&self.actor_thread).take();
        if let Some(actor) = actor {
            let _ = actor.join();
        }
    }
}

#[tauri::command]
pub async fn device_memory_state(
    bridge: State<'_, DeviceMemoryBridge>,
) -> Result<DeviceMemoryState, String> {
    if let Some(state) = lock(&bridge.cached_state).clone() {
        return Ok(state);
    }
    bridge.request(ActorRequest::State).await
}

#[tauri::command]
pub async fn device_memory_remember(
    device_key: Option<String>,
    serial_number: Option<String>,
    bridge: State<'_, DeviceMemoryBridge>,
) -> Result<DeviceMemoryState, String> {
    bridge.request(|reply| ActorRequest::Remember {
        device_key,
        serial_number,
        reply,
    }).await
}

#[tauri::command]
pub async fn device_memory_clear(
    bridge: State<'_, DeviceMemoryBridge>,
) -> Result<DeviceMemoryState, String> {
    bridge.request(ActorRequest::Clear).await
}

#[tauri::command]
pub async fn device_memory_open(
    device_key: Option<String>,
    bridge: State<'_, DeviceMemoryBridge>,
) -> Result<DeviceMemoryState, String> {
    bridge.request(|reply| ActorRequest::Open { device_key, reply }).await
}

#[tauri::command]
pub async fn device_memory_close(
    device_key: Option<String>,
    bridge: State<'_, DeviceMemoryBridge>,
) -> Result<DeviceMemoryState, String> {
    bridge.request(|reply| ActorRequest::Close { device_key, reply }).await
}

#[tauri::command]
pub async fn device_memory_send_report(
    device_key: Option<String>,
    report_id: u8,
    data: Vec<u8>,
    bridge: State<'_, DeviceMemoryBridge>,
) -> Result<(), String> {
    bridge.request(|reply| ActorRequest::SendReport {
        device_key,
        report_id,
        data,
        reply,
    }).await
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn load_selection(path: &PathBuf) -> Result<Option<SavedSelection>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let saved: SavedSelection = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Arquivo de seleção do mouse inválido: {error}"))?;
            if saved.schema_version != 1 || saved.device_key.is_empty() {
                return Err("Arquivo de seleção do mouse usa uma versão inválida.".to_owned());
            }
            Ok(Some(saved))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Não foi possível ler a seleção salva do mouse: {error}")),
    }
}

fn save_selection(path: &PathBuf, selection: &SavedSelection) -> Result<(), String> {
    let bytes = serde_json::to_vec(selection)
        .map_err(|error| format!("Não foi possível codificar a seleção do mouse: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Não foi possível gravar a seleção do mouse: {error}"));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Não foi possível ativar a seleção salva do mouse: {error}"));
    }
    Ok(())
}

fn clear_selection_file_then<T>(path: &Path, clear: impl FnOnce() -> T) -> Result<T, String> {
    fs::remove_file(path)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| format!("Não foi possível remover a seleção salva do mouse: {error}"))?;
    Ok(clear())
}

fn resolve_selection<'a>(
    saved: Option<&SavedSelection>,
    candidates: &'a [atk_device::TargetDevice],
) -> SelectionResolution<'a> {
    let Some(saved) = saved else {
        return SelectionResolution::Unremembered;
    };
    let wired: Vec<_> = candidates.iter().filter(|device| is_wired(device)).collect();
    let receivers: Vec<_> = candidates.iter().filter(|device| !is_wired(device)).collect();
    // The supported cable and receiver form one logical mouse only when the
    // physical inventory is unambiguous. Never choose among multiple cables.
    if wired.len() == 1 && receivers.len() <= 1 {
        let device = wired[0];
        return if device.device_key == saved.device_key {
            SelectionResolution::Connected(device)
        } else {
            SelectionResolution::Reidentified(device)
        };
    }
    if !wired.is_empty() {
        return SelectionResolution::Ambiguous;
    }
    if let Some(key) = &saved.receiver_key {
        if let Some(device) = receivers.iter().find(|device| device.device_key == *key) {
            return SelectionResolution::Connected(device);
        }
    } else if saved.wired_key.is_some() && receivers.len() == 1 {
        return SelectionResolution::Reidentified(receivers[0]);
    }
    let exact: Vec<_> = candidates
        .iter()
        .filter(|candidate| candidate.device_key == saved.device_key)
        .collect();
    match exact.as_slice() {
        [candidate] => return SelectionResolution::Connected(candidate),
        [_, _, ..] => return SelectionResolution::Ambiguous,
        [] => {}
    }

    if let Some(serial) = saved.serial_number.as_deref().filter(|serial| !serial.is_empty()) {
        let matching: Vec<_> = candidates
            .iter()
            .filter(|candidate| candidate.serial_number.as_deref() == Some(serial))
            .collect();
        return match matching.as_slice() {
            [candidate] => SelectionResolution::Reidentified(candidate),
            [_, _, ..] => SelectionResolution::Ambiguous,
            [] => SelectionResolution::Disconnected,
        };
    }

    match candidates {
        [candidate] => SelectionResolution::Reidentified(candidate),
        [] => SelectionResolution::Disconnected,
        _ => SelectionResolution::Ambiguous,
    }
}

fn is_wired(device: &atk_device::TargetDevice) -> bool {
    device.interface.vendor_id == atk_device::WIRED_VENDOR_ID
        && device.interface.product_id == atk_device::WIRED_PRODUCT_ID
}

fn state_from(
    saved: Option<&SavedSelection>,
    candidates: Vec<atk_device::TargetDevice>,
    opened: bool,
    error: Option<String>,
    diagnostics: &DeviceDiagnostics,
) -> DeviceMemoryState {
    let receiver_enumerated = !candidates.is_empty();
    let resolution = resolve_selection(saved, &candidates);
    let (selected, ambiguous) = match resolution {
        SelectionResolution::Connected(device) | SelectionResolution::Reidentified(device) => {
            (Some(device.clone()), false)
        }
        SelectionResolution::Ambiguous => (None, true),
        SelectionResolution::Unremembered | SelectionResolution::Disconnected => (None, false),
    };
    let connected = selected.is_some();
    let transport = selected.as_ref().map(|device| if is_wired(device) { "usb" } else { "receiver" });
    let identity_responded = selected.as_ref().is_some_and(|device| {
        connected && diagnostics.identity_device_key.as_deref() == Some(device.device_key.as_str())
    });
    let diagnostic_status = if diagnostics.status == DiagnosticStatus::Responded && !identity_responded {
        if diagnostics.target_device_key.is_some() {
            DiagnosticStatus::Disconnected
        } else {
            DiagnosticStatus::NotChecked
        }
    } else {
        diagnostics.status
    };
    DeviceMemoryState {
        candidates,
        selected,
        remembered: saved.is_some(),
        connected,
        opened: opened && connected,
        ambiguous,
        error,
        receiver_enumerated,
        identity_responded,
        diagnostic_status,
        last_check_at_ms: diagnostics.last_check_at_ms,
        communication_error: diagnostics.communication_error.clone(),
        transport,
        charging: identity_responded.then_some(diagnostics.charging).flatten(),
    }
}

thread_local! {
    static INVENTORY: RefCell<Option<Vec<atk_device::TargetDevice>>> = const { RefCell::new(None) };
}

fn invalidate_inventory() {
    INVENTORY.with(|inventory| *inventory.borrow_mut() = None);
}

fn enumerate_candidates() -> Result<Vec<atk_device::TargetDevice>, String> {
    INVENTORY.with(|inventory| {
        if let Some(candidates) = inventory.borrow().as_ref() {
            return Ok(candidates.clone());
        }
        let candidates = atk_device::list_target_devices().map_err(|error| error.to_string())?;
        *inventory.borrow_mut() = Some(candidates.clone());
        Ok(candidates)
    })
}

fn actor_loop(
    app: AppHandle,
    selection_path: PathBuf,
    mut saved: Option<SavedSelection>,
    startup_error: Option<String>,
    receiver: Receiver<ActorEnvelope>,
    sender: SyncSender<ActorEnvelope>,
    cached_state: Arc<Mutex<Option<DeviceMemoryState>>>,
) {
    let inventory_dirty = Arc::new(AtomicBool::new(false));
    let topology_revision = Arc::new(AtomicU64::new(0));
    #[cfg(windows)]
    let _watcher = {
        let dirty = Arc::clone(&inventory_dirty);
        let revision = Arc::clone(&topology_revision);
        atk_device::device_changes::DeviceChangeWatcher::new(move || {
            revision.fetch_add(1, Ordering::AcqRel);
            if !dirty.swap(true, Ordering::AcqRel) {
                let _ = sender.try_send(ActorEnvelope {
                    deadline: Instant::now() + REQUEST_TIMEOUT,
                    request: ActorRequest::InventoryChanged,
                });
            }
        }).ok()
    };
    let mut session: Option<atk_device::ReportSession> = None;
    let mut session_key: Option<String> = None;
    let mut reopen_requested = false;
    let mut last_error = startup_error;
    let mut diagnostics = DeviceDiagnostics::default();
    let mut next_status_poll = Instant::now();
    let mut next_reconnect = Instant::now();
    let mut previous_state: Option<DeviceMemoryState> = None;
    let mut hotplug_refresh: Option<Instant> = None;
    let mut hotplug_retry: Option<Instant> = None;

    loop {
        if inventory_dirty.swap(false, Ordering::AcqRel) {
            hotplug_refresh.get_or_insert(Instant::now() + HOTPLUG_SETTLE_INTERVAL);
            hotplug_retry = Some(Instant::now() + Duration::from_millis(350));
        }
        if hotplug_refresh.is_some_and(|deadline| Instant::now() >= deadline)
            || hotplug_retry.is_some_and(|deadline| Instant::now() >= deadline)
        {
            if hotplug_refresh.is_some_and(|deadline| Instant::now() >= deadline) { hotplug_refresh = None; }
            if hotplug_retry.is_some_and(|deadline| Instant::now() >= deadline) { hotplug_retry = None; }
            next_status_poll = Instant::now();
        }
        let received = receiver.recv_timeout(IDLE_REPORT_POLL_INTERVAL).map(|envelope| {
            request_before_deadline(envelope, Instant::now())
        });
        match received {
            Ok(Some(ActorRequest::State(reply))) => {
                let _ = reply.send(build_state(
                    saved.as_ref(),
                    session.is_some(),
                    last_error.clone(),
                    &mut diagnostics,
                ));
            }
            Ok(Some(ActorRequest::Remember {
                device_key,
                serial_number,
                reply,
            })) => {
                let old_selection = saved.clone();
                let result = remember_selection(
                    &selection_path,
                    &mut saved,
                    device_key,
                    serial_number,
                )
                .and_then(|()| {
                    if old_selection != saved {
                        session = None;
                        session_key = None;
                        reopen_requested = false;
                        diagnostics.reset_for_device_change();
                    }
                    build_state(saved.as_ref(), session.is_some(), last_error.clone(), &mut diagnostics)
                });
                if result.is_ok() {
                    last_error = None;
                } else if let Err(error) = &result {
                    last_error = Some(error.clone());
                }
                if let Ok(state) = &result {
                    emit_state(&app, state);
                    previous_state = Some(state.clone());
                }
                let _ = reply.send(result);
            }
            Ok(Some(ActorRequest::Clear(reply))) => {
                let result = clear_selection_file_then(&selection_path, || {
                        session = None;
                        session_key = None;
                        reopen_requested = false;
                        saved = None;
                        last_error = None;
                        diagnostics.reset_for_device_change();
                        build_state(None, false, None, &mut diagnostics)
                    })
                    .and_then(|result| result);
                if let Err(error) = &result {
                    last_error = Some(error.clone());
                    if let Ok(state) = build_state(saved.as_ref(), session.is_some(), last_error.clone(), &mut diagnostics) {
                        emit_state(&app, &state);
                        previous_state = Some(state);
                    }
                }
                if let Ok(state) = &result {
                    emit_device_lifecycle_events(&app, previous_state.as_ref(), state);
                    emit_state(&app, state);
                    previous_state = Some(state.clone());
                }
                let _ = reply.send(result);
            }
            Ok(Some(ActorRequest::Open { device_key, reply })) => {
                let result = open_saved_session(
                    saved.as_ref(),
                    &mut session,
                    &mut session_key,
                    device_key.as_deref(),
                    &topology_revision,
                )
                    .and_then(|()| {
                        reopen_requested = true;
                        last_error = None;
                        build_state(
                            saved.as_ref(),
                            session.is_some(),
                            None,
                            &mut diagnostics,
                        )
                    });
                if let Err(error) = &result {
                    last_error = Some(error.clone());
                }
                if let Ok(state) = &result {
                    emit_state(&app, state);
                    previous_state = Some(state.clone());
                }
                let _ = reply.send(result);
            }
            Ok(Some(ActorRequest::Close { device_key, reply })) => {
                let result = validate_close_target(
                    saved.as_ref(),
                    session_key.as_deref(),
                    device_key.as_deref(),
                )
                .and_then(|()| {
                    session = None;
                    session_key = None;
                    reopen_requested = false;
                    last_error = None;
                    build_state(saved.as_ref(), false, None, &mut diagnostics)
                });
                if let Err(error) = &result {
                    last_error = Some(error.clone());
                }
                if let Ok(state) = &result {
                    emit_state(&app, state);
                    previous_state = Some(state.clone());
                }
                let _ = reply.send(result);
            }
            Ok(Some(ActorRequest::SendReport {
                device_key,
                report_id,
                data,
                reply,
            })) => {
                let mut transaction_attempted = false;
                let result = selected_device_key(saved.as_ref()).and_then(|selected_key| {
                    validate_expected_device_key(device_key.as_deref(), &selected_key)?;
                    if session_key.as_deref() != Some(selected_key.as_str()) {
                        return Err("A conexão aberta não corresponde ao receptor atualmente selecionado.".to_owned());
                    }
                    match session.as_mut() {
                    Some(session) => {
                        let app_for_event = app.clone();
                        let event_key = selected_key.clone();
                        transaction_attempted = true;
                        match session.transact_hid_report(report_id, &data, move |report| {
                            emit_wire_report(&app_for_event, &event_key, &report);
                        }) {
                            Ok(report) => {
                                emit_wire_report(&app, &selected_key, &report);
                                Ok(())
                            }
                            Err(error) => {
                                transaction_attempted = report_error_invalidates_session(&error);
                                Err(error.to_string())
                            },
                        }
                    }
                    None => Err("O mouse ainda não foi aberto pelo ATK HUB.".to_owned()),
                    }
                });
                if let Err(error) = &result {
                    last_error = Some(error.clone());
                    if transaction_attempted {
                        diagnostics.record_failure(session_key.as_deref(), error);
                        session = None;
                        session_key = None;
                        next_reconnect = Instant::now();
                    }
                } else {
                    last_error = None;
                }
                let _ = reply.send(result);
            }
            Ok(Some(ActorRequest::ReadTargetInfo { device_key, reply })) => {
                let mut operation_started = false;
                let mut operation_device_key: Option<String> = None;
                let target_key = selected_device_key(saved.as_ref());
                let target_key_resolved = target_key.is_ok();
                let result = target_key.and_then(|target_key| {
                    // Reject a stale UI target before opening a handle or issuing
                    // any HID report. The actor keeps the selection stable while
                    // this closure performs the bounded read-only probe.
                    validate_expected_device_key(device_key.as_deref(), &target_key)?;
                    operation_started = true;
                    operation_device_key = Some(target_key.clone());
                    if session.is_none() {
                        open_saved_session(
                            saved.as_ref(),
                            &mut session,
                            &mut session_key,
                            Some(&target_key),
                            &topology_revision,
                        )?;
                        reopen_requested = true;
                    } else {
                        validate_expected_device_key(session_key.as_deref(), &target_key)?;
                    }
                    let session = session
                        .as_mut()
                        .ok_or_else(|| "O mouse selecionado não está conectado.".to_owned())?;
                    let app_for_event = app.clone();
                    let event_key = target_key.clone();
                    atk_device::read_info_with_report_session(session, move |report| {
                        emit_wire_report(&app_for_event, &event_key, &report);
                    })
                    .map(|info| (target_key, info))
                    .map_err(|error| error.to_string())
                });
                let result = match result {
                    Ok((target_key, info)) => {
                        diagnostics.record_response(&target_key, identity_from_device_info(&info));
                        diagnostics.charging = info.queries.iter().find(|query| query.command_id == 0x04)
                            .and_then(|query| query.known_values.battery_raw.as_ref())
                            .and_then(|battery| match battery.charging { 0 => Some(false), 1 => Some(true), _ => None });
                        last_error = None;
                        Ok(info)
                    }
                    Err(error) => {
                        if operation_started || !target_key_resolved {
                            let target_key = operation_device_key.as_deref().or(session_key.as_deref());
                            diagnostics.record_failure(target_key, &error);
                        }
                        last_error = Some(error.clone());
                        if operation_started && session.is_some() {
                            session = None;
                            session_key = None;
                            next_reconnect = Instant::now();
                        }
                        Err(error)
                    }
                };
                if let Ok(state) = build_state(
                    saved.as_ref(),
                    session.is_some(),
                    last_error.clone(),
                    &mut diagnostics,
                ) {
                    emit_state(&app, &state);
                    previous_state = Some(state);
                }
                let _ = reply.send(result);
            }
            Ok(Some(ActorRequest::Shutdown)) => break,
            Ok(Some(ActorRequest::InventoryChanged)) => {},
            Ok(None) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        service_background(
            &app,
            saved.as_ref(),
            &mut session,
            &mut session_key,
            reopen_requested,
            &mut last_error,
            &mut next_status_poll,
            &mut next_reconnect,
            &mut previous_state,
            &mut diagnostics,
            &topology_revision,
        );
        *lock(&cached_state) = previous_state.clone();
    }
}

fn request_before_deadline(envelope: ActorEnvelope, now: Instant) -> Option<ActorRequest> {
    if matches!(&envelope.request, ActorRequest::Shutdown) {
        return Some(envelope.request);
    }
    if envelope.deadline <= now {
        reject_expired_request(envelope.request);
        None
    } else {
        Some(envelope.request)
    }
}

fn reject_expired_request(request: ActorRequest) {
    const MESSAGE: &str = "A solicitação HID expirou na fila antes de ser executada.";
    match request {
        ActorRequest::State(reply) | ActorRequest::Remember { reply, .. }
        | ActorRequest::Clear(reply) | ActorRequest::Open { reply, .. }
        | ActorRequest::Close { reply, .. } => {
            let _ = reply.send(Err(MESSAGE.to_owned()));
        }
        ActorRequest::SendReport { reply, .. } => {
            let _ = reply.send(Err(MESSAGE.to_owned()));
        }
        ActorRequest::ReadTargetInfo { reply, .. } => {
            let _ = reply.send(Err(MESSAGE.to_owned()));
        }
        ActorRequest::Shutdown | ActorRequest::InventoryChanged => {}
    }
}

fn service_background(
    app: &AppHandle,
    saved: Option<&SavedSelection>,
    session: &mut Option<atk_device::ReportSession>,
    session_key: &mut Option<String>,
    reopen_requested: bool,
    last_error: &mut Option<String>,
    next_status_poll: &mut Instant,
    next_reconnect: &mut Instant,
    previous_state: &mut Option<DeviceMemoryState>,
    diagnostics: &mut DeviceDiagnostics,
    topology_revision: &Arc<AtomicU64>,
) {
    if Instant::now() >= *next_status_poll {
        invalidate_inventory();
        if let Ok(mut state) = build_state(saved, session.is_some(), last_error.clone(), diagnostics) {
            let selected_key = state.selected.as_ref().map(|device| device.device_key.as_str());
            if session.is_some() && session_key.as_deref() != selected_key {
                *session = None;
                *session_key = None;
                *next_reconnect = Instant::now();
            }
            if previous_state.as_ref().and_then(|old| old.selected.as_ref())
                .map(|device| device.device_key.as_str()) != selected_key {
                diagnostics.reset_for_device_change();
                *last_error = None;
                state = state_from(saved, state.candidates, false, None, diagnostics);
            }
            if previous_state.as_ref() != Some(&state) {
                // Publish the new routing target before connect/input events.
                emit_state(app, &state);
                emit_device_lifecycle_events(app, previous_state.as_ref(), &state);
                *previous_state = Some(state);
            }
        }
        *next_status_poll = Instant::now() + STATUS_POLL_INTERVAL;
    }
    if let Some(active) = session.as_mut() {
        match active.read_report(IDLE_REPORT_POLL_INTERVAL) {
            Ok(Some(wire_report)) => {
                if let Some(key) = session_key.as_deref() { route_idle_report(app, key, &wire_report); }
            },
            Ok(None) => {}
            Err(error) => {
                let error = error.to_string();
                diagnostics.record_failure(session_key.as_deref(), &error);
                *session = None;
                *session_key = None;
                *last_error = Some(error);
                *next_reconnect = Instant::now();
            }
        }
    }

    let now = Instant::now();
    if reopen_requested && session.is_none() && now >= *next_reconnect {
        match open_saved_session(saved, session, session_key, None, topology_revision) {
            Ok(()) => *last_error = None,
            Err(error) => {
                diagnostics.record_failure(session_key.as_deref(), &error);
                *last_error = Some(error);
            }
        }
        *next_reconnect = Instant::now() + RECONNECT_INTERVAL;
    }

    if let Ok(state) = build_state(saved, session.is_some(), last_error.clone(), diagnostics) {
        if previous_state.as_ref() != Some(&state) {
            emit_state(app, &state);
            *previous_state = Some(state);
        }
    }
}

fn build_state(
    saved: Option<&SavedSelection>,
    opened: bool,
    error: Option<String>,
    diagnostics: &mut DeviceDiagnostics,
) -> Result<DeviceMemoryState, String> {
    let candidates = enumerate_candidates()?;
    let mut state = state_from(saved, candidates, opened, error, diagnostics);
    let current_device_key = state.selected.as_ref().map(|device| device.device_key.as_str());
    if let Some(probed_key) = diagnostics.target_device_key.as_deref() {
        match current_device_key {
            Some(current_key) if current_key != probed_key => {
                diagnostics.reset_for_device_change();
            }
            None => diagnostics.invalidate_for_disconnect(),
            _ => {}
        }
    }
    state = state_from(saved, state.candidates.clone(), opened, state.error.clone(), diagnostics);
    Ok(state)
}

fn identity_from_device_info(info: &atk_device::DeviceInfo) -> Option<(u8, u8)> {
    let identity = info.queries.iter().find(|query| query.command_id == 0x10)?;
    if !(2..=10).contains(&identity.data_valid_len) {
        return None;
    }
    // `command_status_raw` has no established enum in atk-protocol, so the
    // diagnostic proves only that the bounded identity payload was returned.
    Some((
        identity.known_values.cid_raw?,
        identity.known_values.mid_raw?,
    ))
}

fn remember_selection(
    selection_path: &PathBuf,
    saved: &mut Option<SavedSelection>,
    requested_key: Option<String>,
    requested_serial: Option<String>,
) -> Result<(), String> {
    let candidates = enumerate_candidates()?;
    let serial = requested_serial.filter(|value| !value.is_empty());
    let matching: Vec<_> = candidates
        .iter()
        .filter(|candidate| {
            if let Some(key) = requested_key.as_deref() {
                candidate.device_key == key
            } else if let Some(serial) = serial.as_deref() {
                candidate.serial_number.as_deref() == Some(serial)
            } else {
                true
            }
        })
        .collect();
    let chosen = match matching.as_slice() {
        [candidate] => *candidate,
        [] => return Err("O dispositivo escolhido não está enumerado como um receptor ATK compatível.".to_owned()),
        many => return Err(format!("A seleção é ambígua: {} receptores correspondem.", many.len())),
    };
    if let Some(serial) = serial.as_deref() {
        if chosen.serial_number.as_deref() != Some(serial) {
            return Err("O número de série escolhido não corresponde ao receptor selecionado.".to_owned());
        }
    }
    let selection = SavedSelection {
        schema_version: 1,
        device_key: chosen.device_key.clone(),
        serial_number: chosen.serial_number.clone().filter(|value| !value.is_empty()),
        receiver_key: {
            let receivers: Vec<_> = candidates.iter().filter(|device| !is_wired(device)).collect();
            (receivers.len() == 1).then(|| receivers[0].device_key.clone())
        },
        wired_key: {
            let wired: Vec<_> = candidates.iter().filter(|device| is_wired(device)).collect();
            (wired.len() == 1).then(|| wired[0].device_key.clone())
        },
    };
    save_selection(selection_path, &selection)?;
    *saved = Some(selection);
    Ok(())
}

fn open_saved_session(
    saved: Option<&SavedSelection>,
    session: &mut Option<atk_device::ReportSession>,
    session_key: &mut Option<String>,
    expected_device_key: Option<&str>,
    topology_revision: &Arc<AtomicU64>,
) -> Result<(), String> {
    let candidates = enumerate_candidates()?;
    let selected = match resolve_selection(saved, &candidates) {
        SelectionResolution::Connected(device) | SelectionResolution::Reidentified(device) => device,
        SelectionResolution::Unremembered => {
            return Err("Nenhum mouse foi lembrado. Selecione o receptor uma vez pelo ATK HUB.".to_owned())
        }
        SelectionResolution::Disconnected => {
            return Err("O mouse lembrado não está conectado.".to_owned())
        }
        SelectionResolution::Ambiguous => {
            return Err("Há mais de um receptor compatível; selecione o dispositivo desejado.".to_owned())
        }
    };
    validate_expected_device_key(expected_device_key, &selected.device_key)?;
    if session.is_some() && session_key.as_deref() == Some(selected.device_key.as_str()) {
        return Ok(());
    }
    *session = None;
    *session_key = None;
    *session = Some(
        atk_device::open_report_session_for_key(
            &selected.device_key,
            atk_device::DEFAULT_QUERY_TIMEOUT,
        )
        .map_err(|error| error.to_string())?,
    );
    *session_key = Some(selected.device_key.clone());
    if let Some(active) = session.as_mut() { active.watch_topology_changes(Arc::clone(topology_revision)); }
    Ok(())
}

fn selected_device_key(saved: Option<&SavedSelection>) -> Result<String, String> {
    let candidates = enumerate_candidates()?;
    match resolve_selection(saved, &candidates) {
        SelectionResolution::Connected(device) | SelectionResolution::Reidentified(device) => {
            Ok(device.device_key.clone())
        }
        SelectionResolution::Unremembered => {
            Err("Nenhum mouse foi lembrado. Selecione o receptor uma vez pelo ATK HUB.".to_owned())
        }
        SelectionResolution::Disconnected => Err("O mouse lembrado não está conectado.".to_owned()),
        SelectionResolution::Ambiguous => {
            Err("Há mais de um receptor compatível; selecione o dispositivo desejado.".to_owned())
        }
    }
}

fn validate_expected_device_key(expected: Option<&str>, actual: &str) -> Result<(), String> {
    if expected.is_some_and(|expected| expected != actual) {
        return Err("Este controle pertence a outro receptor ATK; nenhuma operação foi enviada.".to_owned());
    }
    Ok(())
}

fn report_error_invalidates_session(error: &atk_device::DeviceError) -> bool {
    !matches!(error, atk_device::DeviceError::UnsupportedMouseCommand { .. }
        | atk_device::DeviceError::InvalidRequestReportLength { .. }
        | atk_device::DeviceError::InvalidRequestChecksum
        | atk_device::DeviceError::Protocol(atk_protocol::ParseError::DataLengthTooLarge { .. }))
}

fn validate_close_target(
    saved: Option<&SavedSelection>,
    active_key: Option<&str>,
    expected_key: Option<&str>,
) -> Result<(), String> {
    let actual_key = match active_key {
        Some(active_key) => active_key.to_owned(),
        None => selected_device_key(saved)?,
    };
    validate_expected_device_key(expected_key, &actual_key)
}

fn emit_wire_report(app: &AppHandle, device_key: &str, wire_report: &[u8]) {
    if wire_report.len() != atk_device::WIRE_REPORT_LEN {
        return;
    }
    let payload = DeviceMemoryInputReport {
        device_key: device_key.to_owned(),
        report_id: wire_report[0],
        data: wire_report[1..].to_vec(),
    };
    let _ = app.emit("device-memory-input-report", payload);
}

fn route_idle_report(app: &AppHandle, device_key: &str, wire_report: &[u8]) {
    if wire_report.len() != atk_device::WIRE_REPORT_LEN
        || wire_report[0] != atk_device::TRANSPORT_REPORT_ID
    {
        return;
    }
    let packet = &wire_report[1..];
    if packet.first() != Some(&0x0a) || packet[4] > 10 {
        return;
    }
    emit_wire_report(app, device_key, wire_report);
}

fn emit_state(app: &AppHandle, state: &DeviceMemoryState) {
    if let Some(bridge) = app.try_state::<DeviceMemoryBridge>() {
        *lock(&bridge.cached_state) = Some(state.clone());
    }
    let _ = app.emit("device-memory-state", state);
}

fn emit_device_lifecycle_events(
    app: &AppHandle,
    previous: Option<&DeviceMemoryState>,
    current: &DeviceMemoryState,
) {
    let old = previous.and_then(|state| state.selected.as_ref());
    let new = current.selected.as_ref();
    match (old, new) {
        (None, Some(device)) => emit_lifecycle_event(app, "connect", device),
        (Some(device), None) => emit_lifecycle_event(app, "disconnect", device),
        (Some(old), Some(new)) if old.device_key != new.device_key => {
            emit_lifecycle_event(app, "disconnect", old);
            emit_lifecycle_event(app, "connect", new);
        }
        _ => {}
    }
}

fn emit_lifecycle_event(app: &AppHandle, event_type: &str, device: &atk_device::TargetDevice) {
    #[derive(Clone, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct LifecycleEvent<'a> {
        event_type: &'a str,
        device: &'a atk_device::TargetDevice,
    }
    let _ = app.emit(
        "device-memory-lifecycle",
        LifecycleEvent {
            event_type,
            device,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::atomic::AtomicU64;

    static TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temporary_path(suffix: &str) -> PathBuf {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "atk-device-memory-{}-{id}-{suffix}",
            std::process::id()
        ))
    }

    fn target(key: &str, serial: Option<&str>) -> atk_device::TargetDevice {
        atk_device::TargetDevice {
            device_key: key.to_owned(),
            interface: atk_device::InterfaceMetadata::target(),
            product_name: Some("VXZR1 SE Plus".to_owned()),
            serial_number: serial.map(str::to_owned),
            collections: Vec::new(),
            descriptor_source: "evidence/hid_descriptor_diagnostics.json",
        }
    }

    fn wired(key: &str) -> atk_device::TargetDevice {
        let mut device = target(key, Some("wired-serial"));
        device.interface.vendor_id = atk_device::WIRED_VENDOR_ID;
        device.interface.product_id = atk_device::WIRED_PRODUCT_ID;
        device
    }

    #[test]
    fn rejected_outbound_commands_do_not_invalidate_a_working_connection() {
        assert!(!report_error_invalidates_session(&atk_device::DeviceError::UnsupportedMouseCommand { command_id: 0x09 }));
        assert!(!report_error_invalidates_session(&atk_device::DeviceError::InvalidRequestChecksum));
        assert!(report_error_invalidates_session(&atk_device::DeviceError::ReadFailed));
        assert!(report_error_invalidates_session(&atk_device::DeviceError::TransportChanged));
    }

    #[test]
    fn transport_handoff_prefers_cable_and_returns_to_saved_receiver() {
        let saved = SavedSelection {
            schema_version: 1,
            device_key: "receiver".into(),
            serial_number: Some("receiver-serial".into()),
            ..Default::default()
        };
        let receiver = target("receiver", Some("receiver-serial"));
        for candidates in [vec![receiver.clone()], vec![receiver.clone(), wired("usb")], vec![receiver]] {
            let expected = if candidates.len() == 2 { "usb" } else { "receiver" };
            assert!(matches!(resolve_selection(Some(&saved), &candidates),
                SelectionResolution::Connected(device) | SelectionResolution::Reidentified(device)
                if device.device_key == expected));
        }
        let originally_wired = SavedSelection {
            device_key: "usb".into(),
            serial_number: Some("wired-serial".into()),
            receiver_key: Some("receiver".into()),
            wired_key: Some("usb".into()),
            ..saved
        };
        assert!(matches!(resolve_selection(Some(&originally_wired), &[target("receiver", Some("receiver-serial"))]),
            SelectionResolution::Connected(device) if device.device_key == "receiver"));
    }

    #[test]
    fn transport_handoff_rejects_ambiguous_inventory_and_stale_controls() {
        let saved = SavedSelection { device_key: "receiver".into(), ..Default::default() };
        assert!(matches!(resolve_selection(Some(&saved), &[target("receiver", None), wired("one"), wired("two")]),
            SelectionResolution::Ambiguous));
        assert!(matches!(resolve_selection(Some(&saved), &[target("receiver", None), target("other", None), wired("usb")]),
            SelectionResolution::Ambiguous));
        assert!(validate_expected_device_key(Some("receiver"), "usb").is_err());
        assert!(validate_expected_device_key(Some("usb"), "receiver").is_err());
    }

    #[test]
    fn cached_inventory_is_reused_without_enumeration_and_does_not_share_mutable_results() {
        INVENTORY.with(|cache| *cache.borrow_mut() = Some(vec![wired("usb")]));
        let mut first = enumerate_candidates().unwrap();
        first.clear();
        assert_eq!(enumerate_candidates().unwrap()[0].device_key, "usb");
        invalidate_inventory();
        INVENTORY.with(|cache| assert!(cache.borrow().is_none()));
    }

    #[test]
    fn saved_path_wins_and_reidentifies_only_by_unique_serial_or_unique_target() {
        let saved = SavedSelection {
            schema_version: 1,
            device_key: "old-path".to_owned(),
            serial_number: Some("mouse-1".to_owned()),
            ..Default::default()
        };
        let candidates = vec![target("new-path", Some("mouse-1")), target("other", Some("mouse-2"))];
        assert!(matches!(
            resolve_selection(Some(&saved), &candidates),
            SelectionResolution::Reidentified(device) if device.device_key == "new-path"
        ));

        let ambiguous = vec![target("new-path", None), target("other", None)];
        assert!(matches!(
            resolve_selection(
                Some(&SavedSelection {
                    serial_number: None,
                    ..saved.clone()
                }),
                &ambiguous,
            ),
            SelectionResolution::Ambiguous
        ));

        assert!(matches!(
            resolve_selection(
                Some(&SavedSelection {
                    serial_number: None,
                    ..saved.clone()
                }),
                &[target("only", None)],
            ),
            SelectionResolution::Reidentified(device) if device.device_key == "only"
        ));
    }

    #[test]
    fn state_never_reports_connected_for_ambiguous_or_disconnected_targets() {
        let saved = SavedSelection {
            schema_version: 1,
            device_key: "saved".to_owned(),
            serial_number: Some("mouse-1".to_owned()),
            ..Default::default()
        };
        let ambiguous = state_from(
            Some(&SavedSelection {
                serial_number: None,
                ..saved.clone()
            }),
            vec![target("one", None), target("two", None)],
            true,
            None,
            &DeviceDiagnostics::default(),
        );
        assert!(!ambiguous.connected);
        assert!(!ambiguous.opened);
        assert!(ambiguous.ambiguous);
        assert!(ambiguous.selected.is_none());

        let disconnected = state_from(Some(&saved), Vec::new(), true, None, &DeviceDiagnostics::default());
        assert!(!disconnected.connected);
        assert!(!disconnected.opened);
        assert!(!disconnected.ambiguous);
    }

    fn identity_info(data_valid_len: u8, cid: Option<u8>, mid: Option<u8>) -> atk_device::DeviceInfo {
        atk_device::DeviceInfo {
            schema_version: 1,
            interface: atk_device::InterfaceMetadata::target(),
            transport_report_id: atk_device::TRANSPORT_REPORT_ID,
            queries: vec![atk_device::QueryInfo {
                command: "GetIdentity".to_owned(),
                command_id: 0x10,
                command_status_raw: 0,
                address_be_raw: 0,
                data_valid_len,
                report: atk_device::SanitizedReport {
                    wire_bytes_redacted: Vec::new(),
                    coordinate_system: "wire_report",
                    redacted_wire_byte_ranges: Vec::new(),
                },
                known_values: atk_device::KnownValues {
                    cid_raw: cid,
                    mid_raw: mid,
                    ..Default::default()
                },
            }],
        }
    }

    #[test]
    fn identity_proof_requires_a_valid_identity_query_with_both_raw_values() {
        assert_eq!(
            identity_from_device_info(&identity_info(2, Some(2), Some(32))),
            Some((2, 32))
        );
        assert_eq!(
            identity_from_device_info(&identity_info(1, Some(2), Some(32))),
            None
        );
        assert_eq!(
            identity_from_device_info(&identity_info(11, Some(2), Some(32))),
            None
        );
        assert_eq!(
            identity_from_device_info(&identity_info(2, Some(2), None)),
            None
        );
    }

    #[test]
    fn diagnostic_proof_is_cleared_by_failed_probe_disconnect_and_device_change() {
        let saved = SavedSelection {
            schema_version: 1,
            device_key: "device-one".to_owned(),
            serial_number: None,
            ..Default::default()
        };
        let selected = vec![target("device-one", None)];
        let mut diagnostics = DeviceDiagnostics::default();
        diagnostics.record_response("device-one", Some((2, 32)));

        let responded = state_from(Some(&saved), selected.clone(), true, None, &diagnostics);
        assert!(responded.receiver_enumerated);
        assert!(responded.identity_responded);
        assert_eq!(responded.diagnostic_status, DiagnosticStatus::Responded);
        assert!(responded.last_check_at_ms.is_some());

        diagnostics.record_failure(Some("device-one"), "tempo de resposta esgotado");
        let failed = state_from(Some(&saved), selected.clone(), true, None, &diagnostics);
        assert!(!failed.identity_responded);
        assert_eq!(failed.diagnostic_status, DiagnosticStatus::Failed);
        assert_eq!(failed.communication_error.as_deref(), Some("tempo de resposta esgotado"));

        diagnostics.record_response("device-one", Some((2, 32)));
        diagnostics.invalidate_for_disconnect();
        let disconnected = state_from(Some(&saved), Vec::new(), false, None, &diagnostics);
        assert!(!disconnected.receiver_enumerated);
        assert!(!disconnected.identity_responded);
        assert_eq!(disconnected.diagnostic_status, DiagnosticStatus::Disconnected);
        let reconnected = state_from(Some(&saved), selected, true, None, &diagnostics);
        assert!(!reconnected.identity_responded);

        diagnostics.record_response("device-one", Some((2, 32)));
        diagnostics.reset_for_device_change();
        let changed = state_from(
            Some(&SavedSelection { device_key: "device-two".to_owned(), ..saved }),
            vec![target("device-two", None)],
            true,
            None,
            &diagnostics,
        );
        assert!(!changed.identity_responded);
        assert_eq!(changed.diagnostic_status, DiagnosticStatus::NotChecked);
        assert_eq!(changed.last_check_at_ms, None);
    }

    #[test]
    fn stale_wrapper_key_is_rejected_before_the_io_path() {
        let mut io_path_called = false;
        let result = validate_expected_device_key(Some("previous-device"), "current-device")
            .and_then(|()| {
                io_path_called = true;
                Ok(())
            });

        assert!(result.is_err());
        assert!(!io_path_called);
    }

    #[test]
    fn invalid_saved_selection_is_reported_without_overwriting_the_file() {
        let path = temporary_path("corrupt.json");
        let original = b"{invalid-json";
        fs::write(&path, original).unwrap();

        let result = load_selection(&path);

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_clear_keeps_the_in_memory_selection_and_session_state() {
        let path = temporary_path("directory");
        fs::create_dir(&path).unwrap();
        let mut saved = Some(SavedSelection {
            schema_version: 1,
            device_key: "remembered-key".to_owned(),
            serial_number: None,
            ..Default::default()
        });
        let mut session_open = true;
        let mut clear_called = false;

        let result = clear_selection_file_then(&path, || {
            clear_called = true;
            saved = None;
            session_open = false;
        });

        assert!(result.is_err());
        assert!(!clear_called);
        assert!(session_open);
        assert_eq!(saved.as_ref().unwrap().device_key, "remembered-key");
        fs::remove_dir(path).unwrap();
    }

    #[test]
    fn expired_queued_report_is_rejected_before_the_write_path() {
        let (reply, result) = mpsc::sync_channel(1);
        let write_path_called = AtomicBool::new(false);
        let now = Instant::now();
        let request = request_before_deadline(
            ActorEnvelope {
                deadline: now - Duration::from_millis(1),
                request: ActorRequest::SendReport {
                    device_key: None,
                    report_id: 8,
                    data: vec![0x04; 16],
                    reply,
                },
            },
            now,
        );
        if let Some(ActorRequest::SendReport { .. }) = request {
            write_path_called.store(true, Ordering::Relaxed);
        }

        let message = result.recv().unwrap().unwrap_err();
        assert!(message.contains("expirou na fila"));
        assert!(!write_path_called.load(Ordering::Relaxed));
    }
}
