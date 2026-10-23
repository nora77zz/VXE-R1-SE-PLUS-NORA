#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod desktop;
mod device_bridge;
mod app_updates;
mod window_layout;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State, Window, WindowEvent};
use tauri::menu::MenuItem;
use tauri::tray::TrayIcon;

#[cfg(windows)]
use ghub_lua::{InputBackend, ScriptEvent};
#[cfg(windows)]
use lua_process::{LuaProcessHandle, LuaProcessRuntime, ProcessExit, ProcessReport};
#[cfg(windows)]
use windows_input::{
    log_clear_generation, log_sequence_now, log_session_token, parse_stop_shortcut,
    GlobalStopShortcutManager, LogEntry, RawInputEvent, RawInputListener, WindowsInputBackend,
};

const PROFILE_STORE_FILE: &str = "profiles.json";
const MAX_ARCHIVED_LOGS: usize = 1_000;
const STOP_SHORTCUT_DEFAULT: &str = "Ctrl+Alt+F11";
const STOP_SHORTCUT_FILE: &str = "lua-stop-shortcut.json";

pub(crate) struct AppState {
    lua_status: Arc<Mutex<LuaStatus>>,
    script: Mutex<Option<RunningScript>>,
    archived_logs: Mutex<Vec<ArchivedLogEntry>>,
    log_generation: AtomicU64,
    shortcut_status: Mutex<StopShortcutStatus>,
    #[cfg(windows)]
    shortcut_manager: Mutex<Option<GlobalStopShortcutManager>>,
    #[cfg(windows)]
    shortcut_config_path: Mutex<Option<PathBuf>>,
    tray_icon: Mutex<Option<TrayIcon<tauri::Wry>>>,
    tray_status_item: Mutex<Option<MenuItem<tauri::Wry>>>,
    tray_monitor_stop: Arc<AtomicBool>,
    tray_monitor: Mutex<Option<JoinHandle<()>>>,
    pub(crate) lua_update_gate: tauri::async_runtime::Mutex<()>,
}

impl AppState {
    fn new() -> Self {
        Self {
            lua_status: Arc::new(Mutex::new(LuaStatus::stopped(None))),
            script: Mutex::new(None),
            archived_logs: Mutex::new(Vec::new()),
            log_generation: AtomicU64::new(0),
            shortcut_status: Mutex::new(StopShortcutStatus {
                shortcut: STOP_SHORTCUT_DEFAULT.to_owned(),
                registered: false,
                error: {
                    #[cfg(windows)]
                    {
                        None
                    }
                    #[cfg(not(windows))]
                    {
                        Some("Atalho global disponível apenas no Windows.".to_owned())
                    }
                },
            }),
            #[cfg(windows)]
            shortcut_manager: Mutex::new(None),
            #[cfg(windows)]
            shortcut_config_path: Mutex::new(None),
            tray_icon: Mutex::new(None),
            tray_status_item: Mutex::new(None),
            tray_monitor_stop: Arc::new(AtomicBool::new(false)),
            tray_monitor: Mutex::new(None),
            lua_update_gate: tauri::async_runtime::Mutex::new(()),
        }
    }

    fn shutdown(&self) {
        self.tray_monitor_stop.store(true, Ordering::Release);
        // Dropping the handle detaches the monitor. It may currently be waiting
        // for Tauri's synchronous tray-item dispatch on the main thread, so joining
        // here from the exit callback could deadlock the event loop.
        drop(lock(&self.tray_monitor).take());
        #[cfg(windows)]
        if let Some(mut manager) = lock(&self.shortcut_manager).take() {
            if let Err(error) = manager.stop() {
                eprintln!("Falha ao liberar o atalho global de parada: {error}");
            }
        }
        let mut script = lock(&self.script);
        if let Some(mut running) = script.take() {
            match running.stop() {
                Ok((report, logs)) => {
                    self.archive(logs.into_iter().map(Into::into).collect());
                    *lock(&self.lua_status) = status_after_stop(report);
                }
                Err(error) => {
                    self.archive(backend_log_entries(&running.backend));
                    *lock(&self.lua_status) = failed_status(error);
                }
            }
        } else {
            *lock(&self.lua_status) = LuaStatus::stopped(None);
        }
    }

    fn archive(&self, entries: Vec<ArchivedLogEntry>) {
        let mut archived = lock(&self.archived_logs);
        for entry in entries {
            if archived.len() == MAX_ARCHIVED_LOGS {
                archived.remove(0);
            }
            archived.push(entry);
        }
    }

    pub(crate) fn stop_lua(&self) -> Result<LuaStatus, String> {
        let mut current = lock(&self.script);
        if let Some(mut script) = current.take() {
            *lock(&self.lua_status) = LuaStatus {
                running: true,
                state: LuaState::Stopping,
                message: None,
            };
            match script.stop() {
                Ok((report, logs)) => {
                    self.archive(logs.into_iter().map(Into::into).collect());
                    let status = status_after_stop(report);
                    *lock(&self.lua_status) = status.clone();
                    Ok(status)
                }
                Err(error) => {
                    self.archive(backend_log_entries(&script.backend));
                    *lock(&self.lua_status) = failed_status(error.clone());
                    Err(error)
                }
            }
        } else {
            let status = lock(&self.lua_status).clone();
            if matches!(status.state, LuaState::Failed) {
                Ok(status)
            } else {
                let stopped = LuaStatus::stopped(status.message);
                *lock(&self.lua_status) = stopped.clone();
                Ok(stopped)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArchivedLogEntry {
    id: String,
    sequence: u64,
    message: String,
}

#[cfg(windows)]
impl From<LogEntry> for ArchivedLogEntry {
    fn from(entry: LogEntry) -> Self {
        Self {
            id: entry.id,
            sequence: entry.sequence,
            message: entry.message,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StopShortcutStatus {
    shortcut: String,
    registered: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct LuaLogEntryView {
    id: String,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LuaLogsSince {
    cursor: String,
    reset: bool,
    entries: Vec<LuaLogEntryView>,
}

fn current_log_session() -> String {
    #[cfg(windows)]
    {
        log_session_token().to_owned()
    }
    #[cfg(not(windows))]
    {
        "unsupported".to_owned()
    }
}

fn current_log_sequence() -> u64 {
    #[cfg(windows)]
    {
        log_sequence_now()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn current_log_clear_generation() -> u64 {
    #[cfg(windows)]
    {
        log_clear_generation()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

fn parse_log_cursor(
    cursor: Option<&str>,
    session: &str,
    generation: u64,
    backend_clear_generation: u64,
    latest_sequence: u64,
) -> (u64, bool) {
    let Some(cursor) = cursor else {
        return (0, false);
    };
    let mut parts = cursor.split(':');
    let (
        Some(cursor_session),
        Some(cursor_generation),
        Some(cursor_clear_generation),
        Some(cursor_sequence),
        None,
    ) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return (0, true);
    };
    let Ok(cursor_generation) = cursor_generation.parse::<u64>() else {
        return (0, true);
    };
    let Ok(cursor_clear_generation) = cursor_clear_generation.parse::<u64>() else {
        return (0, true);
    };
    let Ok(cursor_sequence) = cursor_sequence.parse::<u64>() else {
        return (0, true);
    };
    if cursor_session != session
        || cursor_generation != generation
        || cursor_clear_generation != backend_clear_generation
        || cursor_sequence > latest_sequence
    {
        (0, true)
    } else {
        (cursor_sequence, false)
    }
}

fn log_sequence_gap(after_sequence: u64, latest_sequence: u64, entries: &[ArchivedLogEntry]) -> bool {
    if after_sequence >= latest_sequence {
        return false;
    }
    let mut expected = after_sequence.saturating_add(1);
    for entry in entries {
        if entry.sequence < expected {
            continue;
        }
        if entry.sequence != expected {
            return true;
        }
        expected = expected.saturating_add(1);
    }
    expected <= latest_sequence
}

fn next_log_cursor_sequence(
    after_sequence: u64,
    latest_sequence: u64,
    entries: &[ArchivedLogEntry],
) -> u64 {
    entries
        .last()
        .map(|entry| entry.sequence)
        .unwrap_or(after_sequence)
        .max(after_sequence)
        .max(latest_sequence)
}

#[cfg(windows)]
fn backend_log_entries(backend: &WindowsInputBackend) -> Vec<ArchivedLogEntry> {
    backend
        .log_entries_snapshot()
        .into_iter()
        .map(Into::into)
        .collect()
}

#[cfg(windows)]
#[derive(Deserialize)]
struct StopShortcutConfig {
    shortcut: String,
}

#[cfg(windows)]
fn load_stop_shortcut(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<StopShortcutConfig>(&bytes)
            .map(|config| Some(config.shortcut))
            .map_err(|error| format!("arquivo de atalho global inválido: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("não foi possível ler o atalho global salvo: {error}")),
    }
}

#[cfg(windows)]
fn save_stop_shortcut(path: &Path, shortcut: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "o arquivo de atalho não tem diretório pai".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let config = serde_json::json!({ "shortcut": shortcut });
    let bytes = serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LuaStatus {
    running: bool,
    state: LuaState,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl LuaStatus {
    fn stopped(message: Option<String>) -> Self {
        Self {
            running: false,
            state: LuaState::Stopped,
            message,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum LuaState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[cfg(windows)]
struct RunningScript {
    process: Arc<Mutex<Option<LuaProcessHandle>>>,
    completion: Arc<Mutex<Option<PumpCompletion>>>,
    event_pump_stop: Arc<AtomicBool>,
    event_pump: Option<JoinHandle<Result<(), String>>>,
    backend: Arc<WindowsInputBackend>,
}

#[cfg(windows)]
#[derive(Clone)]
struct PumpCompletion {
    report: Option<ProcessReport>,
    error: Option<String>,
}

#[cfg(windows)]
impl RunningScript {
    fn try_collect_report(&mut self, status: &Arc<Mutex<LuaStatus>>) -> bool {
        collect_process_report(&self.process, &self.completion, status)
    }

    fn has_completed(&self) -> bool {
        lock(&self.completion).is_some()
    }

    fn stop(&mut self) -> Result<(Option<ProcessReport>, Vec<LogEntry>), String> {
        self.event_pump_stop.store(true, Ordering::Release);
        let pump_error = self.event_pump.take().and_then(|pump| match pump.join() {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("a thread de encaminhamento de eventos entrou em panic".to_owned()),
        });

        let process_result = stop_process(&self.process);
        let completion = lock(&self.completion).take();
        let report = process_result
            .as_ref()
            .ok()
            .and_then(|report| report.clone())
            .or_else(|| completion.as_ref().and_then(|value| value.report.clone()));
        let logs = self.backend.log_entries_snapshot();
        let mut errors = Vec::new();
        if let Some(error) = pump_error {
            errors.push(error);
        }
        if let Err(error) = process_result {
            errors.push(error);
        }
        if let Some(error) = completion.and_then(|value| value.error) {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok((report, logs))
        } else {
            errors.dedup();
            Err(errors.join("; "))
        }
    }
}

#[cfg(windows)]
impl Drop for RunningScript {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    id: String,
    name: String,
    #[serde(default)]
    active: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileStore {
    #[serde(default)]
    profiles: Vec<Profile>,
    #[serde(default)]
    active_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileList {
    profiles: Vec<Profile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsAvailability {
    available: bool,
    settings: Option<Value>,
    unsupported_fields: Vec<String>,
    message: String,
}

#[tauri::command]
async fn device_info(
    device_key: Option<String>,
    bridge: State<'_, device_bridge::DeviceMemoryBridge>,
) -> Result<atk_device::DeviceInfo, String> {
    bridge.read_target_info_for(device_key).await
}

#[tauri::command]
fn device_get_settings() -> SettingsAvailability {
    SettingsAvailability {
        available: false,
        settings: None,
        unsupported_fields: vec!["mouse-settings".to_owned()],
        message: "A leitura destas configurações HID ainda não foi validada; este comando legado não retorna valores reais do mouse.".to_owned(),
    }
}

#[tauri::command]
fn device_update(field: String, value: serde_json::Value) -> Result<serde_json::Value, String> {
    let _ = (field, value);
    Err("Este comando legado não é utilizado pela interface ativa; altere a configuração pelo controlador HID da tela do mouse.".to_owned())
}

#[tauri::command]
fn lua_status(state: State<'_, AppState>) -> LuaStatus {
    lock(&state.lua_status).clone()
}

#[tauri::command]
fn lua_logs(state: State<'_, AppState>) -> Vec<String> {
    let mut entries: Vec<String> = lock(&state.archived_logs)
        .iter()
        .map(|entry| entry.message.clone())
        .collect();
    #[cfg(windows)]
    if let Some(script) = lock(&state.script).as_ref() {
        entries.extend(script.backend.log_snapshot());
    }
    entries
}

#[tauri::command]
fn lua_logs_since(cursor: Option<String>, state: State<'_, AppState>) -> LuaLogsSince {
    let session = current_log_session();
    let generation = state.log_generation.load(Ordering::Acquire);
    let backend_clear_generation = current_log_clear_generation();
    let latest_sequence = current_log_sequence();
    let (mut after_sequence, mut reset) = parse_log_cursor(
        cursor.as_deref(),
        &session,
        generation,
        backend_clear_generation,
        latest_sequence,
    );
    let cursor_was_reset = reset;

    let script = lock(&state.script);
    let archived = lock(&state.archived_logs);

    #[cfg(windows)]
    let (_active_oldest, _active_newest, active_entries) = script.as_ref().map_or(
        (None, None, Vec::new()),
        |running| running.backend.log_delta_since(after_sequence),
    );
    #[cfg(not(windows))]
    let (active_oldest, active_newest, active_entries): (
        Option<u64>,
        Option<u64>,
        Vec<ArchivedLogEntry>,
    ) = {
        let _ = &script;
        (None, None, Vec::new())
    };

    let archive_start = archived.partition_point(|entry| entry.sequence <= after_sequence);
    let mut entries: Vec<ArchivedLogEntry> = archived[archive_start..].to_vec();
    #[cfg(windows)]
    entries.extend(active_entries.into_iter().map(Into::into));

    entries.sort_by_key(|entry| entry.sequence);
    entries.dedup_by(|left, right| left.id == right.id);
    if !reset && cursor.is_some() && log_sequence_gap(after_sequence, latest_sequence, &entries) {
        reset = true;
    }
    if reset {
        after_sequence = 0;
        if !cursor_was_reset {
            let archive_start = archived.partition_point(|entry| entry.sequence <= after_sequence);
            entries = archived[archive_start..].to_vec();
            #[cfg(windows)]
            if let Some(running) = script.as_ref() {
                entries.extend(running.backend.log_entries_since(0).into_iter().map(Into::into));
            }
            entries.sort_by_key(|entry| entry.sequence);
            entries.dedup_by(|left, right| left.id == right.id);
        }
    }
    let next_sequence = next_log_cursor_sequence(after_sequence, latest_sequence, &entries);
    let response_cursor = format!(
        "{session}:{generation}:{backend_clear_generation}:{next_sequence}"
    );
    LuaLogsSince {
        cursor: response_cursor,
        reset,
        entries: entries
            .into_iter()
            .map(|entry| LuaLogEntryView {
                id: entry.id,
                message: entry.message,
            })
            .collect(),
    }
}

#[tauri::command]
fn lua_clear_log(state: State<'_, AppState>) {
    clear_log_state(&state);
}

fn clear_log_state(state: &AppState) {
    let script = lock(&state.script);
    lock(&state.archived_logs).clear();
    #[cfg(windows)]
    if let Some(script) = script.as_ref() {
        let backend: Arc<dyn InputBackend> = script.backend.clone();
        let _ = backend.clear_log();
    }
    state.log_generation.fetch_add(1, Ordering::AcqRel);
}

#[tauri::command]
fn lua_stop_shortcut_get(state: State<'_, AppState>) -> StopShortcutStatus {
    lock(&state.shortcut_status).clone()
}

#[tauri::command]
fn lua_stop_shortcut_set(
    shortcut: String,
    state: State<'_, AppState>,
) -> Result<StopShortcutStatus, String> {
    #[cfg(not(windows))]
    {
        let _ = (shortcut, state);
        return Err("O atalho global de parada só está disponível no Windows.".to_owned());
    }

    #[cfg(windows)]
    {
        let parsed = parse_stop_shortcut(&shortcut)?;
        let path = lock(&state.shortcut_config_path)
            .clone()
            .ok_or_else(|| "o caminho local para salvar o atalho não está disponível".to_owned())?;
        let manager = lock(&state.shortcut_manager);
        let manager = manager
            .as_ref()
            .ok_or_else(|| "a thread de atalhos globais não está ativa".to_owned())?;
        let previous = lock(&state.shortcut_status).clone();
        manager.set(parsed.clone())?;

        if let Err(save_error) = save_stop_shortcut(&path, &parsed.shortcut) {
            let rollback = parse_stop_shortcut(&previous.shortcut)
                .and_then(|old| manager.set(old));
            return match rollback {
                Ok(()) => Err(format!(
                    "não foi possível salvar a configuração do atalho; o atalho anterior foi restaurado: {save_error}"
                )),
                Err(rollback_error) => {
                    let status = StopShortcutStatus {
                        shortcut: parsed.shortcut,
                        registered: true,
                        error: Some(format!("configuração não salva: {save_error}; falha ao restaurar o atalho anterior: {rollback_error}")),
                    };
                    *lock(&state.shortcut_status) = status.clone();
                    Err(status.error.clone().unwrap_or_default())
                }
            };
        }

        let status = StopShortcutStatus {
            shortcut: parsed.shortcut,
            registered: true,
            error: None,
        };
        *lock(&state.shortcut_status) = status.clone();
        Ok(status)
    }
}

#[tauri::command]
async fn lua_start(source: String, app: AppHandle, state: State<'_, AppState>) -> Result<LuaStatus, String> {
    #[cfg(not(windows))]
    {
        let _ = (source, app, state);
        return Err("A execução de Lua com entrada do Windows só está disponível nesta plataforma.".to_owned());
    }

    #[cfg(windows)]
    {
        let _update_gate = state.lua_update_gate.lock().await;
        let mut current = lock(&state.script);
        if let Some(running) = current.as_mut() {
            running.try_collect_report(&state.lua_status);
            if !running.has_completed() {
                return Err("Já existe um script Lua em execução.".to_owned());
            }

            let mut previous = current
                .take()
                .expect("the completed script is still stored in app state");
            match previous.stop() {
                Ok((report, logs)) => {
                    state.archive(logs.into_iter().map(Into::into).collect());
                    *lock(&state.lua_status) = status_after_stop(report);
                }
                Err(error) => {
                    state.archive(backend_log_entries(&previous.backend));
                    *lock(&state.lua_status) = failed_status(error.clone());
                    return Err(error);
                }
            }
        }
        *lock(&state.lua_status) = LuaStatus {
            running: false,
            state: LuaState::Starting,
            message: None,
        };

        let result = start_lua_script(&app, &source, Arc::clone(&state.lua_status));
        match result {
            Ok(script) => {
                *current = Some(script);
                let mut status = lock(&state.lua_status);
                if matches!(status.state, LuaState::Starting) {
                    *status = LuaStatus {
                        running: true,
                        state: LuaState::Running,
                        message: None,
                    };
                }
                Ok(status.clone())
            }
            Err((error, logs)) => {
                state.archive(logs.into_iter().map(Into::into).collect());
                *lock(&state.lua_status) = LuaStatus {
                    running: false,
                    state: LuaState::Failed,
                    message: Some(error.clone()),
                };
                Err(error)
            }
        }
    }
}

#[cfg(windows)]
fn start_lua_script(
    app: &AppHandle,
    source: &str,
    status: Arc<Mutex<LuaStatus>>,
) -> Result<RunningScript, (String, Vec<LogEntry>)> {
    let worker = find_worker_executable(app).map_err(|error| (error, Vec::new()))?;
    let backend = Arc::new(WindowsInputBackend::new());
    let backend_trait: Arc<dyn InputBackend> = backend.clone();
    let runtime = LuaProcessRuntime::new(backend_trait);
    let mut handle = runtime
        .start(worker, source.to_owned())
        .map_err(|error| (error.to_string(), backend.log_entries_snapshot()))?;

    if let Err(error) = handle.dispatch(ScriptEvent::new("PROFILE_ACTIVATED", None, None::<String>)) {
        let cleanup = handle.stop();
        let message = format!(
            "Não foi possível entregar PROFILE_ACTIVATED: {}{}",
            error,
            cleanup_report_suffix(cleanup)
        );
        return Err((message, backend.log_entries_snapshot()));
    }

    let listener = match RawInputListener::start_with_keyboard_capture(
        256,
        backend.keyboard_capture_flag(),
    ) {
        Ok(listener) => listener,
        Err(error) => {
            let cleanup = handle.stop();
            let message = format!(
                "Não foi possível iniciar a captura Raw Input: {}{}",
                error,
                cleanup_report_suffix(cleanup)
            );
            return Err((message, backend.log_entries_snapshot()));
        }
    };

    let process = Arc::new(Mutex::new(Some(handle)));
    let completion = Arc::new(Mutex::new(None));
    let pump_process = Arc::clone(&process);
    let pump_completion = Arc::clone(&completion);
    let pump_status = Arc::clone(&status);
    let event_pump_stop = Arc::new(AtomicBool::new(false));
    let pump_stop = Arc::clone(&event_pump_stop);
    let event_pump = match thread::Builder::new()
        .name("atk-mouse-event-forwarder".to_owned())
        .spawn(move || {
            event_pump_main(
                listener,
                pump_process,
                pump_completion,
                pump_status,
                pump_stop,
            )
        }) {
        Ok(event_pump) => event_pump,
        Err(error) => {
            let cleanup = stop_process(&process);
            let message = format!(
                "Não foi possível iniciar o encaminhamento de eventos: {}{}",
                error,
                cleanup_report_suffix(cleanup.map_err(|error| error.to_owned()).and_then(|report| {
                    report.ok_or_else(|| "o processo Lua já havia sido encerrado".to_owned())
                }))
            );
            return Err((message, backend.log_entries_snapshot()));
        }
    };

    Ok(RunningScript {
        process,
        completion,
        event_pump_stop,
        event_pump: Some(event_pump),
        backend,
    })
}

#[cfg(windows)]
fn event_pump_main(
    mut listener: RawInputListener,
    process: Arc<Mutex<Option<LuaProcessHandle>>>,
    completion: Arc<Mutex<Option<PumpCompletion>>>,
    status: Arc<Mutex<LuaStatus>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut pump_error = None;
    while !stop.load(Ordering::Acquire) {
        if collect_process_report(&process, &completion, &status) {
            pump_error = lock(&completion)
                .as_ref()
                .and_then(|value| value.error.clone());
            break;
        }

        match listener.try_recv_raw() {
            Ok(raw_event) => {
                let events = raw_input_to_script_events(raw_event);
                let dispatch = {
                    let process = lock(&process);
                    match process.as_ref() {
                        Some(handle) => events.into_iter().try_for_each(|event| {
                            handle.dispatch(event).map_err(|error| error.to_string())
                        }),
                        None => Err("o processo Lua já foi encerrado".to_owned()),
                    }
                };
                if let Err(error) = dispatch {
                    pump_error = Some(fail_event_pump(
                        &process,
                        &completion,
                        &status,
                        format!("Falha ao encaminhar evento de entrada: {error}"),
                    ));
                    break;
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                if !stop.load(Ordering::Acquire) {
                    pump_error = Some(fail_event_pump(
                        &process,
                        &completion,
                        &status,
                        "A captura Raw Input foi encerrada inesperadamente.".to_owned(),
                    ));
                }
                break;
            }
        }
    }

    if let Err(error) = listener.stop() {
        let listener_error = format!("Falha ao encerrar a captura Raw Input: {error}");
        if stop.load(Ordering::Acquire) {
            pump_error = Some(listener_error);
        } else {
            let report = lock(&completion)
                .as_ref()
                .and_then(|value| value.report.clone());
            let (report, cause) = if report.is_some() {
                (report, listener_error)
            } else {
                match stop_process(&process) {
                    Ok(report) => (report, listener_error),
                    Err(cleanup_error) => (
                        None,
                        format!("{listener_error}; falha na limpeza do processo Lua: {cleanup_error}"),
                    ),
                }
            };
            let error = completion_error_message(cause, report.as_ref());
            publish_completion(
                &completion,
                &status,
                report,
                Some(error.clone()),
            );
            pump_error = Some(error);
        }
    }

    match pump_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[cfg(windows)]
fn raw_input_to_script_events(event: RawInputEvent) -> Vec<ScriptEvent> {
    match event {
        RawInputEvent::MouseButton(transition) => vec![ScriptEvent::new(
            transition.event_name(),
            Some(i64::from(transition.button)),
            Some(transition.family()),
        )],
        RawInputEvent::Wheel {
            vertical,
            horizontal,
        } => {
            let mut events = Vec::with_capacity(2);
            if vertical != 0 {
                events.push(ScriptEvent::new("MOUSE_WHEEL", Some(i64::from(vertical)), Some("wheel")));
            }
            if horizontal != 0 {
                events.push(ScriptEvent::new("MOUSE_HWHEEL", Some(i64::from(horizontal)), Some("wheel")));
            }
            events
        }
        RawInputEvent::Key { key, pressed } => vec![
            ScriptEvent::new(
                if pressed { "KEY_PRESSED" } else { "KEY_RELEASED" },
                Some(i64::from(key.scan_code)),
                Some("keyboard"),
            )
            .with_key(key),
        ],
    }
}

#[cfg(windows)]
fn collect_process_report(
    process: &Arc<Mutex<Option<LuaProcessHandle>>>,
    completion: &Arc<Mutex<Option<PumpCompletion>>>,
    status: &Arc<Mutex<LuaStatus>>,
) -> bool {
    let result = {
        let mut slot = lock(process);
        let Some(handle) = slot.as_mut() else {
            return lock(completion).is_some();
        };
        match handle.try_report() {
            Ok(None) => return false,
            Ok(Some(report)) => {
                slot.take();
                Ok(report)
            }
            Err(error) => {
                slot.take();
                Err(error.to_string())
            }
        }
    };
    match result {
        Ok(report) => publish_completion(completion, status, Some(report), None),
        Err(error) => publish_completion(completion, status, None, Some(error)),
    }
    true
}

#[cfg(windows)]
fn fail_event_pump(
    process: &Arc<Mutex<Option<LuaProcessHandle>>>,
    completion: &Arc<Mutex<Option<PumpCompletion>>>,
    status: &Arc<Mutex<LuaStatus>>,
    cause: String,
) -> String {
    let (report, cause) = match stop_process(process) {
        Ok(report) => (report, cause),
        Err(error) => (None, format!("{cause}; falha na limpeza do processo Lua: {error}")),
    };
    let error = completion_error_message(cause, report.as_ref());
    publish_completion(completion, status, report, Some(error.clone()));
    error
}

#[cfg(windows)]
fn stop_process(
    process: &Arc<Mutex<Option<LuaProcessHandle>>>,
) -> Result<Option<ProcessReport>, String> {
    let mut process = lock(process);
    match process.take() {
        Some(mut handle) => handle.stop().map(Some).map_err(|error| error.to_string()),
        None => Ok(None),
    }
}

#[cfg(windows)]
fn publish_completion(
    completion: &Arc<Mutex<Option<PumpCompletion>>>,
    status: &Arc<Mutex<LuaStatus>>,
    report: Option<ProcessReport>,
    error: Option<String>,
) {
    let next_status = if let Some(error) = &error {
        failed_status(completion_error_message(error.clone(), report.as_ref()))
    } else {
        status_after_stop(report.clone())
    };
    *lock(completion) = Some(PumpCompletion { report, error });
    *lock(status) = next_status;
}

#[cfg(windows)]
fn completion_error_message(cause: String, report: Option<&ProcessReport>) -> String {
    let mut details = Vec::new();
    if !cause.is_empty() {
        details.push(cause);
    }
    if let Some(report) = report {
        match &report.exit {
            ProcessExit::Failed(message)
            | ProcessExit::StartupFailed(message)
            | ProcessExit::ProtocolError(message) => details.push(message.clone()),
            ProcessExit::UnexpectedEof => details.push("o worker encerrou o protocolo inesperadamente".to_owned()),
            ProcessExit::KilledAfterGrace => details.push("o worker precisou ser encerrado após o prazo de limpeza".to_owned()),
            ProcessExit::Cancelled => {}
        }
        if !report.cleanup_errors.is_empty() {
            details.push(format!("erros de limpeza: {}", report.cleanup_errors.join("; ")));
        }
    }
    details.dedup();
    details.join("; ")
}

#[cfg(windows)]
fn cleanup_report_suffix<E: std::fmt::Display>(cleanup: Result<ProcessReport, E>) -> String {
    match cleanup {
        Ok(report) => {
            let details = completion_error_message(String::new(), Some(&report));
            if details.is_empty() {
                String::new()
            } else {
                format!("; relatório de limpeza: {details}")
            }
        }
        Err(error) => format!("; falha na limpeza do processo Lua: {error}"),
    }
}

#[cfg(windows)]
fn find_worker_executable(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(parent) = exe_path.parent() {
            let same_dir = parent.join("lua-worker.exe");
            if same_dir.is_file() {
                return fs::canonicalize(same_dir).map_err(|error| error.to_string());
            }
        }
    }

    if let Ok(resource_dir) = app.path().resource_dir() {
        let bundled = resource_dir.join("lua-worker.exe");
        if bundled.is_file() {
            return fs::canonicalize(&bundled).map_err(|error| error.to_string());
        }
    }

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for profile in ["release", "debug"] {
        let candidate = workspace
            .join("apps/lua-worker/target")
            .join(profile)
            .join("lua-worker.exe");
        if candidate.is_file() {
            return fs::canonicalize(candidate).map_err(|error| error.to_string());
        }
    }

    Err("Executável do worker Lua não encontrado. Compile `apps/lua-worker` antes de iniciar scripts.".to_owned())
}

#[tauri::command]
fn lua_stop(state: State<'_, AppState>) -> Result<LuaStatus, String> {
    state.stop_lua()
}

#[cfg(windows)]
fn status_after_stop(report: Option<ProcessReport>) -> LuaStatus {
    let Some(report) = report else {
        return LuaStatus::stopped(None);
    };
    let failure = match &report.exit {
        ProcessExit::Failed(message)
        | ProcessExit::StartupFailed(message)
        | ProcessExit::ProtocolError(message) => Some(message.clone()),
        ProcessExit::UnexpectedEof => Some("O processo Lua terminou sem encerrar o protocolo.".to_owned()),
        ProcessExit::KilledAfterGrace => Some("O processo Lua precisou ser encerrado após o prazo de limpeza.".to_owned()),
        ProcessExit::Cancelled => None,
    };
    if let Some(message) = failure {
        let message = if report.cleanup_errors.is_empty() {
            message
        } else {
            format!("{message}; erros de limpeza: {}", report.cleanup_errors.join("; "))
        };
        LuaStatus {
            running: false,
            state: LuaState::Failed,
            message: Some(message),
        }
    } else if report.cleanup_errors.is_empty() {
        LuaStatus::stopped(None)
    } else {
        LuaStatus::stopped(Some(report.cleanup_errors.join("; ")))
    }
}

#[cfg(not(windows))]
fn status_after_stop(_report: Option<()>) -> LuaStatus {
    LuaStatus::stopped(None)
}

fn failed_status(message: String) -> LuaStatus {
    LuaStatus {
        running: false,
        state: LuaState::Failed,
        message: Some(message),
    }
}

#[tauri::command]
fn profile_list(app: AppHandle) -> Result<ProfileList, String> {
    let store = read_profile_store(&app)?;
    Ok(ProfileList {
        profiles: with_active_flags(store),
    })
}

#[tauri::command]
fn profile_get(id: String, app: AppHandle) -> Result<Option<Profile>, String> {
    validate_profile_id(&id)?;
    let store = read_profile_store(&app)?;
    Ok(store
        .profiles
        .into_iter()
        .find(|profile| profile.id == id)
        .map(|mut profile| {
            profile.active = store.active_id.as_deref() == Some(profile.id.as_str());
            profile
        }))
}

#[tauri::command]
fn profile_set(id: String, app: AppHandle) -> Result<Profile, String> {
    validate_profile_id(&id)?;
    let path = profile_store_path(&app)?;
    let mut store = read_profile_store(&app)?;
    if !store.profiles.iter().any(|profile| profile.id == id) {
        return Err("O perfil solicitado não existe neste computador.".to_owned());
    }
    store.active_id = Some(id.clone());
    persist_profile_store(&path, &store)?;
    store
        .profiles
        .into_iter()
        .find(|profile| profile.id == id)
        .map(|mut profile| {
            profile.active = true;
            profile
        })
        .ok_or_else(|| "O perfil desapareceu durante a seleção.".to_owned())
}

fn validate_profile_id(id: &str) -> Result<(), String> {
    let valid = (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err("O identificador de perfil deve conter de 1 a 64 letras, números, `-` ou `_`.".to_owned())
    }
}

fn with_active_flags(mut store: ProfileStore) -> Vec<Profile> {
    for profile in &mut store.profiles {
        profile.active = store.active_id.as_deref() == Some(profile.id.as_str());
    }
    store.profiles
}

fn profile_store_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|directory| directory.join(PROFILE_STORE_FILE))
        .map_err(|error| error.to_string())
}

fn read_profile_store(app: &AppHandle) -> Result<ProfileStore, String> {
    let path = profile_store_path(app)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(ProfileStore::default()),
        Err(error) => return Err(error.to_string()),
    };
    serde_json::from_slice(&bytes).map_err(|error| format!("Arquivo local de perfis inválido: {error}"))
}

fn persist_profile_store(path: &Path, store: &ProfileStore) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Diretório local de dados inválido.".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(store).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())
}

#[tauri::command]
fn window_minimize(window: Window) -> Result<(), String> {
    window.minimize().map_err(|error| error.to_string())
}

#[tauri::command]
fn window_toggle_maximize(window: Window) -> Result<(), String> {
    if window.is_maximized().map_err(|error| error.to_string())? {
        window.unmaximize().map_err(|error| error.to_string())
    } else {
        window.maximize().map_err(|error| error.to_string())
    }
}

#[tauri::command]
fn window_close(window: Window) -> Result<(), String> {
    window.close().map_err(|error| error.to_string())
}

fn request_async_lua_stop(app: &AppHandle, origin: &'static str) {
    let app = app.clone();
    if let Err(error) = thread::Builder::new()
        .name(format!("atk-lua-stop-{origin}"))
        .spawn(move || {
            if let Err(error) = app.state::<AppState>().stop_lua() {
                eprintln!("Não foi possível parar o script Lua pelo {origin}: {error}");
            }
        })
    {
        eprintln!("Não foi possível iniciar a parada de Lua pelo {origin}: {error}");
    }
}

#[cfg(windows)]
fn install_stop_shortcut(app: &tauri::App) {
    let state = app.state::<AppState>();
    let path = app
        .path()
        .app_local_data_dir()
        .ok()
        .map(|directory| directory.join(STOP_SHORTCUT_FILE));
    let mut load_error = None;
    let saved = match path.as_deref().map(load_stop_shortcut) {
        Some(Ok(saved)) => saved,
        Some(Err(error)) => {
            load_error = Some(error);
            None
        }
        None => {
            load_error = Some("não foi possível localizar o diretório local de dados".to_owned());
            None
        }
    };
    let candidate = saved.as_deref().unwrap_or(STOP_SHORTCUT_DEFAULT);
    let parsed = match parse_stop_shortcut(candidate) {
        Ok(shortcut) => shortcut,
        Err(error) => {
            load_error = Some(format!("atalho salvo inválido; padrão aplicado: {error}"));
            parse_stop_shortcut(STOP_SHORTCUT_DEFAULT)
                .expect("the built-in stop shortcut is valid")
        }
    };
    let stop_app = app.handle().clone();
    match GlobalStopShortcutManager::start(parsed.clone(), move || {
        request_async_lua_stop(&stop_app, "atalho global");
    }) {
        Ok((manager, registration)) => {
            let registration_error = registration.err();
            let status = StopShortcutStatus {
                shortcut: parsed.shortcut,
                registered: registration_error.is_none(),
                error: match (registration_error, load_error) {
                    (Some(registration), Some(load)) => Some(format!("{registration}; {load}")),
                    (Some(error), None) | (None, Some(error)) => Some(error),
                    (None, None) => None,
                },
            };
            *lock(&state.shortcut_manager) = Some(manager);
            *lock(&state.shortcut_status) = status;
            *lock(&state.shortcut_config_path) = path;
        }
        Err(error) => {
            *lock(&state.shortcut_status) = StopShortcutStatus {
                shortcut: parsed.shortcut,
                registered: false,
                error: Some(load_error.map_or(error.clone(), |load| format!("{error}; {load}"))),
            };
            *lock(&state.shortcut_config_path) = path;
        }
    }
}

fn tray_lua_status(status: &LuaStatus) -> (String, String) {
    let state_text = match &status.state {
        LuaState::Stopped => "parado",
        LuaState::Starting => "iniciando",
        LuaState::Running => "executando",
        LuaState::Stopping => "parando",
        LuaState::Failed => "falhou",
    };
    let mut label = format!("Lua: {state_text}");
    if let Some(message) = status.message.as_deref() {
        if !message.is_empty() {
            label.push_str(" — ");
            label.push_str(message);
        }
    }
    let label = truncate_tray_text(&label, 160);
    let tooltip = truncate_tray_text(&format!("ATK HUB — {label}"), 120);
    (label, tooltip)
}

fn truncate_tray_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut value: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    value.push('…');
    value
}

fn start_tray_status_monitor(app: AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let stop = Arc::clone(&state.tray_monitor_stop);
    let monitor_app = app.clone();
    let monitor = thread::Builder::new()
        .name("atk-tray-lua-status".to_owned())
        .spawn(move || {
            let mut previous = None::<(String, String)>;
            while !stop.load(Ordering::Acquire) {
                let state = monitor_app.state::<AppState>();
                let status = lock(&state.lua_status).clone();
                let display = tray_lua_status(&status);
                if previous.as_ref() != Some(&display) {
                    if let Some(icon) = lock(&state.tray_icon).clone() {
                        if let Err(error) = icon.set_tooltip(Some(display.1.as_str())) {
                            eprintln!("Não foi possível atualizar o status da bandeja: {error}");
                        }
                    }
                    if let Some(item) = lock(&state.tray_status_item).clone() {
                        if let Err(error) = item.set_text(&display.0) {
                            eprintln!("Não foi possível atualizar o status de Lua no menu: {error}");
                        }
                    }
                    previous = Some(display);
                }
                thread::sleep(Duration::from_millis(300));
            }
        })
        .map_err(|error| format!("não foi possível acompanhar o status da bandeja: {error}"))?;
    *lock(&state.tray_monitor) = Some(monitor);
    Ok(())
}

fn install_tray(app: &tauri::App) -> Result<(), String> {
    use tauri::menu::{Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let open = MenuItem::with_id(app, "open", "Abrir ATK HUB", true, Option::<&str>::None)
        .map_err(|error| error.to_string())?;
    let lua_status = MenuItem::with_id(app, "lua-status", "Lua: parado", false, Option::<&str>::None)
        .map_err(|error| error.to_string())?;
    let stop_lua = MenuItem::with_id(app, "lua-stop", "Parar script", true, Option::<&str>::None)
        .map_err(|error| error.to_string())?;
    let update_check = MenuItem::with_id(
        app,
        "update-check",
        "Verificar atualizações",
        true,
        Option::<&str>::None,
    )
    .map_err(|error| error.to_string())?;
    let quit = MenuItem::with_id(app, "quit", "Sair", true, Option::<&str>::None)
        .map_err(|error| error.to_string())?;
    let menu = Menu::with_items(app, &[&open, &lua_status, &stop_lua, &update_check, &quit])
        .map_err(|error| error.to_string())?;
    let icon = app
        .default_window_icon()
        .ok_or_else(|| "O ícone do ATK HUB não foi carregado; o ícone da bandeja não pode ser criado.".to_owned())?;
    let tray = TrayIconBuilder::with_id("atk-hub-tray")
        .menu(&menu)
        .icon(icon.clone())
        .tooltip("ATK HUB")
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Err(error) = desktop::show_main_window(app) {
                    eprintln!("Não foi possível abrir a janela do ATK HUB: {error}");
                }
            }
            "lua-stop" => request_async_lua_stop(app, "menu da bandeja"),
            "update-check" => {
                if let Err(error) = desktop::show_main_window(app) {
                    eprintln!("Não foi possível abrir a janela para verificar atualizações: {error}");
                }
                if let Err(error) = app.emit("app-update-open", ()) {
                    eprintln!("Não foi possível abrir a seção de atualizações: {error}");
                }
            }
            "quit" => desktop::request_app_quit(app),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                if let Err(error) = desktop::show_main_window(&tray.app_handle()) {
                    eprintln!("Não foi possível abrir a janela do ATK HUB: {error}");
                }
            }
        });
    let tray = tray.build(app).map_err(|error| error.to_string())?;
    let state = app.state::<AppState>();
    *lock(&state.tray_icon) = Some(tray);
    *lock(&state.tray_status_item) = Some(lua_status);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Err(error) = desktop::show_main_window(app) {
                eprintln!("Não foi possível focar a janela da instância principal: {error}");
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(AppState::new())
        .manage(app_updates::UpdateManager::new())
        .invoke_handler(tauri::generate_handler![
            app_updates::app_update_status,
            app_updates::app_update_check,
            app_updates::app_update_install,
            device_info,
            device_bridge::device_memory_state,
            device_bridge::device_memory_remember,
            device_bridge::device_memory_clear,
            device_bridge::device_memory_open,
            device_bridge::device_memory_close,
            device_bridge::device_memory_send_report,
            device_get_settings,
            device_update,
            lua_start,
            lua_stop,
            lua_status,
            lua_logs,
            lua_logs_since,
            lua_clear_log,
            lua_stop_shortcut_get,
            lua_stop_shortcut_set,
            profile_list,
            profile_get,
            profile_set,
            desktop::system_get_settings,
            desktop::system_set_settings,
            desktop::system_migrate_legacy_settings,
            desktop::system_get_pointer_speed,
            desktop::system_set_pointer_speed,
            desktop::system_get_roller_speed,
            desktop::system_set_roller_speed,
            desktop::configuration_export,
            desktop::app_quit,
            window_minimize,
            window_toggle_maximize,
            window_close
        ])
        .setup(|app| {
            install_tray(app).map_err(std::io::Error::other)?;
            #[cfg(windows)]
            install_stop_shortcut(app);
            let memory_bridge = device_bridge::DeviceMemoryBridge::new(app.handle().clone())
                .map_err(std::io::Error::other)?;
            app.manage(memory_bridge);
            let window = app
                .get_webview_window("main")
                .ok_or_else(|| std::io::Error::other("janela principal não encontrada"))?;
            window_layout::clamp_main_window(&window).map_err(std::io::Error::other)?;
            match desktop::should_start_hidden(&app.handle()) {
                Ok(false) => window.show()?,
                Ok(true) => {}
                Err(error) => {
                    eprintln!("Não foi possível ler a preferência de início minimizado: {error}");
                    window.show()?;
                }
            }
            if let Err(error) = start_tray_status_monitor(app.handle().clone()) {
                eprintln!("Não foi possível iniciar o monitor de status da bandeja: {error}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(error) = desktop::close_window(window, &window.app_handle()) {
                    eprintln!("Não foi possível aplicar a preferência de fechamento: {error}");
                    desktop::request_app_quit(&window.app_handle());
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to build ATK HUB desktop application")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                app.state::<AppState>().shutdown();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archived(sequence: u64) -> ArchivedLogEntry {
        ArchivedLogEntry {
            id: format!("test:{sequence}"),
            sequence,
            message: format!("line {sequence}"),
        }
    }

    #[test]
    fn log_cursor_gap_detection_catches_archive_active_rotation_holes() {
        let mut retained = (1..=1_000).map(archived).collect::<Vec<_>>();
        retained.extend((4_001..=5_000).map(archived));
        assert!(log_sequence_gap(1_005, 5_000, &retained));

        let continuous = (1_001..=1_020).map(archived).collect::<Vec<_>>();
        assert!(!log_sequence_gap(1_000, 1_020, &continuous));
    }

    #[test]
    fn log_cursor_reset_advances_past_a_clear_with_old_archived_entries() {
        let state = AppState::new();
        state.archive((1..=8).map(archived).collect());
        let clear_generation_before = current_log_clear_generation();
        #[cfg(windows)]
        let backend = {
            let backend = Arc::new(WindowsInputBackend::new());
            *lock(&state.script) = Some(RunningScript {
                process: Arc::new(Mutex::new(None)),
                completion: Arc::new(Mutex::new(None)),
                event_pump_stop: Arc::new(AtomicBool::new(false)),
                event_pump: None,
                backend: Arc::clone(&backend),
            });
            backend
        };
        let old_cursor = format!(
            "session:0:{}:5",
            clear_generation_before
        );
        clear_log_state(&state);
        #[cfg(windows)]
        assert!(log_clear_generation() > clear_generation_before);

        let (sequence, reset) = parse_log_cursor(
            Some(&old_cursor),
            "session",
            state.log_generation.load(Ordering::Acquire),
            current_log_clear_generation(),
            5,
        );
        assert_eq!((sequence, reset), (0, true));
        assert!(lock(&state.archived_logs).is_empty());
        #[cfg(windows)]
        assert!(backend.log_entries_snapshot().is_empty());

        // Even with a stale archived tail still present (for example, a Lua
        // script clearing only its own backend log), the snapshot cursor consumes
        // the observed process sequence and remains stable on the next poll.
        let stale_archive = (1..=3).map(archived).collect::<Vec<_>>();
        let next_sequence = next_log_cursor_sequence(sequence, 5, &stale_archive);
        assert_eq!(next_sequence, 5);
        let (next_after, next_reset) = parse_log_cursor(
            Some(&format!("session:{}:{}:{next_sequence}", state.log_generation.load(Ordering::Acquire), current_log_clear_generation())),
            "session",
            state.log_generation.load(Ordering::Acquire),
            current_log_clear_generation(),
            5,
        );
        assert_eq!(next_after, 5);
        assert!(!next_reset);
        assert!(!log_sequence_gap(next_after, 5, &[]));
        assert_eq!(next_log_cursor_sequence(next_after, 5, &[]), 5);
    }

    #[cfg(windows)]
    #[test]
    fn save_stop_shortcut_replaces_existing_windows_file_and_preserves_neighbors() {
        struct ScratchDirectory(PathBuf);

        impl Drop for ScratchDirectory {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "atk-shortcut-persistence-{}-{nonce:x}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create isolated persistence directory");
        let _cleanup = ScratchDirectory(directory.clone());
        let config = directory.join(STOP_SHORTCUT_FILE);
        let unrelated = directory.join("keep-this-file.txt");
        fs::write(&unrelated, b"unrelated contents").expect("create neighboring file");

        for shortcut in ["Ctrl+Alt+F11", "Ctrl+Shift+F10", "Alt+F9"] {
            save_stop_shortcut(&config, shortcut).expect("save or replace shortcut file");
            assert_eq!(
                load_stop_shortcut(&config).expect("read saved shortcut").as_deref(),
                Some(shortcut)
            );
        }

        assert_eq!(fs::read(&unrelated).expect("read neighboring file"), b"unrelated contents");
        assert!(!config.with_extension("json.tmp").exists());
    }

    #[cfg(windows)]
    #[test]
    fn app_stop_archives_final_and_duplicate_lua_logs_with_stable_ids() {
        let worker = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../apps/lua-worker/target/release/lua-worker.exe");
        assert!(worker.is_file(), "expected the existing Lua worker at {}", worker.display());

        let backend = Arc::new(WindowsInputBackend::new());
        let backend_trait: Arc<dyn InputBackend> = backend.clone();
        let runtime = LuaProcessRuntime::new(backend_trait);
        let process = runtime
            .start(
                worker,
                "OutputLogMessage('same')\nOutputLogMessage('same')\natk.on_stop(function() OutputLogMessage('final') end)",
            )
            .expect("the existing Lua worker should start");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while backend.log_entries_snapshot().len() < 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(backend.log_entries_snapshot().len() >= 2);

        let running = RunningScript {
            process: Arc::new(Mutex::new(Some(process))),
            completion: Arc::new(Mutex::new(None)),
            event_pump_stop: Arc::new(AtomicBool::new(false)),
            event_pump: None,
            backend: Arc::clone(&backend),
        };
        let state = AppState::new();
        *lock(&state.script) = Some(running);
        state.stop_lua().expect("the app stop path should complete");

        let first_archive = lock(&state.archived_logs).clone();
        let messages = first_archive
            .iter()
            .map(|entry| entry.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(messages, ["same", "same", "final"]);
        let mut ids = first_archive.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), first_archive.len());
        assert!(first_archive.windows(2).all(|pair| pair[0].sequence < pair[1].sequence));

        state.stop_lua().expect("a second stop should be idempotent");
        assert_eq!(*lock(&state.archived_logs), first_archive);

        let prior_cursor = format!(
            "{}:{}:{}:{}",
            log_session_token(),
            state.log_generation.load(Ordering::Acquire),
            log_clear_generation(),
            first_archive.last().unwrap().sequence
        );
        clear_log_state(&state);
        let (sequence, reset) = parse_log_cursor(
            Some(&prior_cursor),
            log_session_token(),
            state.log_generation.load(Ordering::Acquire),
            log_clear_generation(),
            log_sequence_now(),
        );
        assert_eq!((sequence, reset), (0, true));
        assert!(lock(&state.archived_logs).is_empty());
    }
}
