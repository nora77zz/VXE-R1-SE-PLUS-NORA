use crate::AppState;
use semver::Version;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

const STATUS_EVENT: &str = "app-update-status";
const MAX_NOTES_CHARS: usize = 16_000;
const MAX_ERROR_CHARS: usize = 4_000;
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AppUpdatePhase {
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Installing,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateStatus {
    pub current_version: String,
    pub state: AppUpdatePhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub downloaded_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl AppUpdateStatus {
    fn initial() -> Self {
        Self {
            current_version: env!("CARGO_PKG_VERSION").to_owned(),
            state: AppUpdatePhase::Idle,
            available_version: None,
            notes: None,
            downloaded_bytes: 0,
            total_bytes: None,
            error: None,
        }
    }

}

#[derive(Debug)]
struct PendingUpdate<T> {
    version: String,
    notes: Option<String>,
    update: T,
}

pub struct UpdateManager {
    status: Mutex<AppUpdateStatus>,
    pending: Mutex<Option<PendingUpdate<Update>>>,
    operation_active: AtomicBool,
    last_progress_emit: Mutex<Option<Instant>>,
}

impl UpdateManager {
    pub fn new() -> Self {
        Self {
            status: Mutex::new(AppUpdateStatus::initial()),
            pending: Mutex::new(None),
            operation_active: AtomicBool::new(false),
            last_progress_emit: Mutex::new(None),
        }
    }

    fn try_begin_operation(&self) -> Option<UpdateOperation<'_>> {
        self.operation_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| UpdateOperation {
                active: &self.operation_active,
            })
    }

    fn status(&self, current_version: &str) -> AppUpdateStatus {
        let mut status = lock(&self.status);
        status.current_version = current_version.to_owned();
        status.clone()
    }

    fn replace_status(&self, status: AppUpdateStatus) -> AppUpdateStatus {
        *lock(&self.status) = status.clone();
        status
    }

    fn mutate_status(&self, update: impl FnOnce(&mut AppUpdateStatus)) -> AppUpdateStatus {
        let mut status = lock(&self.status);
        update(&mut status);
        status.clone()
    }

    fn emit(&self, app: &AppHandle, status: AppUpdateStatus) {
        if let Err(error) = app.emit(STATUS_EVENT, status) {
            eprintln!("Não foi possível emitir o estado do atualizador: {error}");
        }
    }

    fn restore_pending(&self, pending: PendingUpdate<Update>) {
        *lock(&self.pending) = Some(pending);
    }

    fn fail(&self, app: &AppHandle, current_version: &str, message: impl AsRef<str>) -> AppUpdateStatus {
        let error = truncate_chars(message.as_ref(), MAX_ERROR_CHARS);
        let status = self.mutate_status(|status| {
            status.current_version = current_version.to_owned();
            status.state = AppUpdatePhase::Error;
            status.error = Some(error);
            status.downloaded_bytes = 0;
            status.total_bytes = None;
        });
        self.emit(app, status.clone());
        status
    }

    fn update_progress(
        &self,
        app: &AppHandle,
        current_version: &str,
        chunk_bytes: usize,
        total_bytes: Option<u64>,
        force_emit: bool,
    ) {
        let status = self.mutate_status(|status| {
            status.current_version = current_version.to_owned();
            status.state = AppUpdatePhase::Downloading;
            status.downloaded_bytes = status.downloaded_bytes.saturating_add(chunk_bytes as u64);
            if total_bytes.is_some() {
                status.total_bytes = total_bytes;
            }
            status.error = None;
        });
        let now = Instant::now();
        let should_emit = {
            let mut last_emit = lock(&self.last_progress_emit);
            let elapsed = last_emit.map(|last| now.saturating_duration_since(last));
            if force_emit || elapsed.is_none_or(|elapsed| elapsed >= PROGRESS_EVENT_INTERVAL) {
                *last_emit = Some(now);
                true
            } else {
                false
            }
        };
        if should_emit {
            self.emit(app, status);
        }
    }
}

impl Default for UpdateManager {
    fn default() -> Self {
        Self::new()
    }
}

struct UpdateOperation<'a> {
    active: &'a AtomicBool,
}

impl Drop for UpdateOperation<'_> {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

fn parse_semver(value: &str) -> Result<Version, String> {
    Version::parse(value.strip_prefix('v').unwrap_or(value))
        .map_err(|error| format!("versão SemVer inválida `{value}`: {error}"))
}

fn is_strictly_newer(current: &str, offered: &str) -> Result<bool, String> {
    Ok(parse_semver(offered)? > parse_semver(current)?)
}

fn take_expected_update<T>(
    pending: &mut Option<PendingUpdate<T>>,
    expected_version: &str,
) -> Result<PendingUpdate<T>, String> {
    let offered = pending
        .as_ref()
        .ok_or_else(|| "Não há uma atualização aguardando instalação.".to_owned())?;
    if offered.version != expected_version {
        return Err(format!(
            "A versão confirmada ({expected_version}) não corresponde à oferta atual ({}).",
            offered.version
        ));
    }
    Ok(pending.take().expect("the pending update was checked above"))
}

#[tauri::command]
pub fn app_update_status(app: AppHandle, updates: State<'_, UpdateManager>) -> AppUpdateStatus {
    updates.status(&app.package_info().version.to_string())
}

#[tauri::command]
pub async fn app_update_check(
    app: AppHandle,
    updates: State<'_, UpdateManager>,
) -> Result<AppUpdateStatus, String> {
    Ok(check_updates(app, updates).await)
}

async fn check_updates(app: AppHandle, updates: State<'_, UpdateManager>) -> AppUpdateStatus {
    let current_version = app.package_info().version.to_string();
    let Some(_operation) = updates.try_begin_operation() else {
        return updates.status(&current_version);
    };

    let checking = updates.mutate_status(|status| {
        status.current_version = current_version.clone();
        status.state = AppUpdatePhase::Checking;
        status.downloaded_bytes = 0;
        status.total_bytes = None;
        status.error = None;
    });
    updates.emit(&app, checking);

    let update = match app
        .updater_builder()
        .timeout(UPDATE_CHECK_TIMEOUT)
        .build()
    {
        Ok(updater) => match updater.check().await {
            Ok(update) => update,
            Err(error) => {
                return updates.fail(
                    &app,
                    &current_version,
                    format!("Não foi possível verificar atualizações: {error}"),
                )
            }
        },
        Err(error) => {
            return updates.fail(
                &app,
                &current_version,
                format!("Não foi possível iniciar o atualizador: {error}"),
            )
        }
    };

    match update {
        Some(mut update) => {
            update.timeout = Some(UPDATE_DOWNLOAD_TIMEOUT);
            let version = update.version.clone();
            match is_strictly_newer(&current_version, &version) {
                Ok(true) => {
                    let notes = update.body.as_deref().map(|body| truncate_chars(body, MAX_NOTES_CHARS));
                    let pending = PendingUpdate {
                        version: version.clone(),
                        notes: notes.clone(),
                        // The official Windows updater passes its restart flag to
                        // NSIS/MSI. Keep that behavior explicit for this app.
                        update: update.restart_after_install(true),
                    };
                    *lock(&updates.pending) = Some(pending);
                    let status = updates.replace_status(AppUpdateStatus {
                        current_version,
                        state: AppUpdatePhase::Available,
                        available_version: Some(version),
                        notes,
                        downloaded_bytes: 0,
                        total_bytes: None,
                        error: None,
                    });
                    updates.emit(&app, status.clone());
                    status
                }
                Ok(false) => updates.fail(
                    &app,
                    &current_version,
                    "O servidor anunciou uma versão que não é superior à instalada; nenhuma oferta foi substituída.",
                ),
                Err(error) => updates.fail(&app, &current_version, error),
            }
        }
        None => {
            *lock(&updates.pending) = None;
            let status = updates.replace_status(AppUpdateStatus {
                current_version,
                state: AppUpdatePhase::UpToDate,
                available_version: None,
                notes: None,
                downloaded_bytes: 0,
                total_bytes: None,
                error: None,
            });
            updates.emit(&app, status.clone());
            status
        }
    }
}

#[tauri::command]
pub async fn app_update_install(
    expected_version: String,
    app: AppHandle,
    updates: State<'_, UpdateManager>,
    app_state: State<'_, AppState>,
) -> Result<AppUpdateStatus, String> {
    Ok(install_update(expected_version, app, updates, app_state).await)
}

async fn install_update(
    expected_version: String,
    app: AppHandle,
    updates: State<'_, UpdateManager>,
    app_state: State<'_, AppState>,
) -> AppUpdateStatus {
    let current_version = app.package_info().version.to_string();
    let Some(_operation) = updates.try_begin_operation() else {
        return updates.status(&current_version);
    };

    let pending = match take_pending_update(&updates.pending, &expected_version) {
        Ok(pending) => pending,
        Err(error) => return updates.fail(&app, &current_version, error),
    };

    let available_version = pending.version.clone();
    let notes = pending.notes.clone();
    let downloading = updates.replace_status(AppUpdateStatus {
        current_version: current_version.clone(),
        state: AppUpdatePhase::Downloading,
        available_version: Some(available_version),
        notes,
        downloaded_bytes: 0,
        total_bytes: None,
        error: None,
    });
    updates.emit(&app, downloading);

    let progress_app = app.clone();
    let progress_current_version = current_version.clone();
    let update_manager = updates.inner();
    let bytes = match pending
        .update
        .download(
            move |chunk_bytes, total_bytes| {
                update_manager.update_progress(
                    &progress_app,
                    &progress_current_version,
                    chunk_bytes,
                    total_bytes,
                    false,
                );
            },
            || {},
        )
        .await
    {
        Ok(bytes) => bytes,
        Err(error) => {
            updates.restore_pending(pending);
            return updates.fail(
                &app,
                &current_version,
                format!("Download ou verificação da assinatura falhou: {error}"),
            );
        }
    };

    if let Err(error) = pending.update.verify(&bytes) {
        updates.restore_pending(pending);
        return updates.fail(
            &app,
            &current_version,
            format!("A assinatura do pacote não pôde ser confirmada: {error}"),
        );
    }

    // The download callback is throttled; always send the final verified byte
    // count before stopping Lua and starting the installer.
    updates.update_progress(&app, &current_version, 0, None, true);

    // Serialize Lua starts with the entire stop/install critical section. The
    // existing worker can run throughout the download and is stopped only after
    // the updater has downloaded and verified the signed package.
    let _lua_gate = app_state.lua_update_gate.lock().await;
    if let Err(error) = app_state.stop_lua() {
        updates.restore_pending(pending);
        return updates.fail(
            &app,
            &current_version,
            format!("O worker Lua não parou com segurança; a instalação foi cancelada: {error}"),
        );
    }

    let installing = updates.mutate_status(|status| {
        status.current_version = current_version.clone();
        status.state = AppUpdatePhase::Installing;
        status.available_version = Some(pending.version.clone());
        status.notes = pending.notes.clone();
        status.error = None;
    });
    updates.emit(&app, installing);

    if let Err(error) = pending.update.install(bytes) {
        updates.restore_pending(pending);
        return updates.fail(
            &app,
            &current_version,
            format!("A instalação falhou; tente novamente: {error}"),
        );
    }

    // On Windows, the official updater launches the installer with its restart
    // flag and exits the app. macOS/Linux need this explicit relaunch.
    app.restart()
}

fn take_pending_update(
    pending: &Mutex<Option<PendingUpdate<Update>>>,
    expected_version: &str,
) -> Result<PendingUpdate<Update>, String> {
    take_expected_update(&mut lock(pending), expected_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_status_serializes_frontend_contract_with_optional_metadata() {
        let value = serde_json::to_value(AppUpdateStatus {
            current_version: "1.0.0".to_owned(),
            state: AppUpdatePhase::Downloading,
            available_version: Some("1.2.0".to_owned()),
            notes: Some("Correções".to_owned()),
            downloaded_bytes: 128,
            total_bytes: Some(512),
            error: None,
        })
        .expect("status serializes");

        assert_eq!(
            value,
            json!({
                "currentVersion": "1.0.0",
                "state": "downloading",
                "availableVersion": "1.2.0",
                "notes": "Correções",
                "downloadedBytes": 128,
                "totalBytes": 512,
            })
        );
    }

    #[test]
    fn semver_guard_accepts_prerelease_progression_and_rejects_equal_or_downgrade() {
        assert!(is_strictly_newer("1.2.0", "v1.2.1").expect("valid versions"));
        assert!(is_strictly_newer("1.2.0-beta.1", "1.2.0").expect("valid versions"));
        assert!(!is_strictly_newer("1.2.0", "1.2.0").expect("valid versions"));
        assert!(!is_strictly_newer("1.2.0", "1.1.9").expect("valid versions"));
    }

    #[test]
    fn expected_version_mismatch_preserves_pending_offer_for_retry() {
        let mut pending = Some(PendingUpdate {
            version: "1.2.0".to_owned(),
            notes: None,
            update: "signed-update-handle",
        });

        let error = take_expected_update(&mut pending, "1.3.0").expect_err("version must match");

        assert!(error.contains("não corresponde"));
        assert_eq!(pending.as_ref().map(|offer| offer.version.as_str()), Some("1.2.0"));
        assert_eq!(pending.as_ref().map(|offer| offer.update), Some("signed-update-handle"));
    }

    #[test]
    fn update_operations_reject_overlap_and_release_the_guard_for_retry() {
        let manager = UpdateManager::new();
        let first = manager.try_begin_operation().expect("first operation begins");
        assert!(manager.try_begin_operation().is_none());

        drop(first);

        assert!(manager.try_begin_operation().is_some());
    }
}
