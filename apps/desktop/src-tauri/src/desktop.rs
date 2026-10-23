use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tauri::{AppHandle, Manager, Window};

const SETTINGS_FILE: &str = "system-settings.json";
const MAX_EXPORT_BYTES: usize = 1024 * 1024;
const AUTOSTART_ARGUMENT: &str = "--autostart";

static SETTINGS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn settings_lock() -> MutexGuard<'static, ()> {
    SETTINGS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SystemSettings {
    pub open_at_login: bool,
    pub start_minimized: bool,
    pub close_minimize_tray: bool,
    pub low_charge_tips: bool,
}

impl Default for SystemSettings {
    fn default() -> Self {
        Self {
            open_at_login: false,
            start_minimized: false,
            close_minimize_tray: true,
            low_charge_tips: true,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub cancelled: bool,
    pub path: Option<String>,
}

#[tauri::command]
pub fn system_get_settings(app: AppHandle) -> Result<SystemSettings, String> {
    let _guard = settings_lock();
    read_settings(&settings_path(&app)?)
}

#[tauri::command]
pub fn system_set_settings(
    patch: Value,
    app: AppHandle,
) -> Result<SystemSettings, String> {
    let _guard = settings_lock();
    apply_settings_patch(&settings_path(&app)?, patch)
}

#[tauri::command]
pub fn system_migrate_legacy_settings(
    settings: Value,
    app: AppHandle,
) -> Result<SystemSettings, String> {
    let _guard = settings_lock();
    let path = settings_path(&app)?;
    migrate_settings_if_absent(&path, settings)
}

fn migrate_settings_if_absent(path: &Path, settings: Value) -> Result<SystemSettings, String> {
    match fs::metadata(path) {
        Ok(_) => return read_settings(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Não foi possível consultar as configurações locais: {error}")),
    }

    migrate_legacy_settings(path, settings)
}

#[tauri::command]
pub fn system_get_pointer_speed() -> Result<i32, String> {
    #[cfg(windows)]
    {
        windows_settings::get_pointer_speed()
    }
    #[cfg(not(windows))]
    {
        Err("A velocidade do ponteiro só pode ser lida no Windows.".to_owned())
    }
}

#[tauri::command]
pub fn system_set_pointer_speed(value: i32) -> Result<i32, String> {
    validate_pointer_speed(value)?;
    #[cfg(windows)]
    {
        windows_settings::set_pointer_speed(value)
    }
    #[cfg(not(windows))]
    {
        Err("A velocidade do ponteiro só pode ser alterada no Windows.".to_owned())
    }
}

#[tauri::command]
pub fn system_get_roller_speed() -> Result<i32, String> {
    #[cfg(windows)]
    {
        windows_settings::get_roller_speed()
    }
    #[cfg(not(windows))]
    {
        Err("A velocidade da roda só pode ser lida no Windows.".to_owned())
    }
}

#[tauri::command]
pub fn system_set_roller_speed(value: i32) -> Result<i32, String> {
    validate_roller_speed(value)?;
    #[cfg(windows)]
    {
        windows_settings::set_roller_speed(value)
    }
    #[cfg(not(windows))]
    {
        Err("A velocidade da roda só pode ser alterada no Windows.".to_owned())
    }
}

#[tauri::command]
pub fn configuration_export(
    configuration: Value,
    window: Window,
) -> Result<ExportResult, String> {
    let bytes = serialize_export_configuration(&configuration)?;

    #[cfg(windows)]
    {
        let default_name = export_default_filename(&configuration);
        match windows_settings::save_configuration(&window, &default_name)? {
            None => Ok(ExportResult {
                cancelled: true,
                path: None,
            }),
            Some(path) => {
                atomic_write(&path, &bytes)?;
                Ok(ExportResult {
                    cancelled: false,
                    path: Some(path.to_string_lossy().into_owned()),
                })
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (window, bytes);
        Err("A exportação por diálogo só está disponível no Windows.".to_owned())
    }
}

fn serialize_export_configuration(configuration: &Value) -> Result<Vec<u8>, String> {
    if !configuration.is_object() {
        return Err("A configuração exportada deve ser um objeto JSON.".to_owned());
    }
    let bytes = serde_json::to_vec(configuration)
        .map_err(|error| format!("Não foi possível preparar a configuração: {error}"))?;
    if bytes.len() > MAX_EXPORT_BYTES {
        return Err(format!(
            "A configuração excede o limite de {} bytes.",
            MAX_EXPORT_BYTES
        ));
    }
    Ok(bytes)
}

pub fn close_window(window: &Window, app: &AppHandle) -> Result<(), String> {
    match system_get_settings(app.clone()) {
        Ok(settings) if settings.close_minimize_tray => {
            window.hide().map_err(|error| error.to_string())
        }
        Ok(_) => {
            app.state::<crate::AppState>().shutdown();
            app.exit(0);
            Ok(())
        }
        Err(error) => Err(format!("Não foi possível ler a preferência de fechamento: {error}")),
    }
}

pub fn show_main_window(app: &AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or_else(|| "A janela principal não está disponível.".to_owned())?;
    if window.is_minimized().map_err(|error| error.to_string())? {
        window.unminimize().map_err(|error| error.to_string())?;
    }
    window.show().map_err(|error| error.to_string())?;
    window.set_focus().map_err(|error| error.to_string())
}

pub fn request_app_quit(app: &AppHandle) {
    app.state::<crate::AppState>().shutdown();
    app.exit(0);
}

pub fn should_start_hidden(app: &AppHandle) -> Result<bool, String> {
    let launched_at_login = std::env::args_os().any(|argument| argument == AUTOSTART_ARGUMENT);
    Ok(launched_at_login && system_get_settings(app.clone())?.start_minimized)
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|directory| directory.join(SETTINGS_FILE))
        .map_err(|error| format!("Não foi possível localizar as configurações locais: {error}"))
}

fn read_settings(path: &Path) -> Result<SystemSettings, String> {
    let object = match fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Arquivo local de configurações inválido: {error}"))?;
            value
                .as_object()
                .cloned()
                .ok_or_else(|| "O arquivo local de configurações deve conter um objeto JSON.".to_owned())?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(error) => return Err(format!("Não foi possível ler as configurações locais: {error}")),
    };
    settings_from_object(&object)
}

fn apply_settings_patch(path: &Path, patch: Value) -> Result<SystemSettings, String> {
    let patch = patch
        .as_object()
        .ok_or_else(|| "As configurações devem ser enviadas como um objeto JSON.".to_owned())?;
    for (key, value) in patch {
        if !is_setting_key(key) {
            return Err(format!("Configuração desconhecida: {key}"));
        }
        if !value.is_boolean() {
            return Err(format!("A configuração {key} deve ser booleana."));
        }
    }

    let mut object = read_settings_object(path)?;
    validate_stored_settings(&object)?;

    for (key, value) in patch {
        if key != "openAtLogin" {
            object.insert(key.clone(), value.clone());
        }
    }

    let open_at_login = patch.get("openAtLogin").and_then(Value::as_bool);
    commit_settings(path, object, open_at_login, !path.exists() || patch.keys().any(|key| key != "openAtLogin"))
}

fn migrate_legacy_settings(path: &Path, legacy: Value) -> Result<SystemSettings, String> {
    let legacy = legacy
        .as_object()
        .ok_or_else(|| "As configurações antigas devem ser um objeto JSON.".to_owned())?;
    let mut object = read_settings_object(path)?;
    validate_stored_settings(&object)?;
    let mut open_at_login = None;
    for (key, value) in legacy {
        if is_setting_key(key) && !value.is_boolean() {
            return Err(format!("A configuração antiga {key} deve ser booleana."));
        }
        if key == "openAtLogin" {
            open_at_login = value.as_bool();
        } else {
            // Keep unrelated legacy preferences as opaque JSON data. They are
            // never interpreted as commands or as desktop settings.
            object.insert(key.clone(), value.clone());
        }
    }
    validate_stored_settings(&object)?;
    commit_settings(path, object, open_at_login, true)
}

fn commit_settings(
    path: &Path,
    object: Map<String, Value>,
    open_at_login: Option<bool>,
    persist: bool,
) -> Result<SystemSettings, String> {
    #[cfg(windows)]
    let old_run_value = if open_at_login.is_some() {
        Some(windows_settings::read_autostart_value()?)
    } else {
        None
    };

    #[cfg(not(windows))]
    if open_at_login.is_some() {
        return Err("A inicialização automática só pode ser alterada no Windows.".to_owned());
    }

    #[cfg(windows)]
    if let Some(enabled) = open_at_login {
        if let Err(error) = windows_settings::write_autostart(enabled) {
            if let Some(old_value) = old_run_value.as_ref() {
                if let Err(rollback) = windows_settings::restore_autostart(old_value.as_deref()) {
                    return Err(format!("{error}; falha ao restaurar o registro de inicialização: {rollback}"));
                }
            }
            return Err(error);
        }
    }

    let settings = match settings_from_object(&object) {
        Ok(settings) => settings,
        Err(error) => {
            #[cfg(windows)]
            if let Some(Some(old_value)) = old_run_value.as_ref() {
                if let Err(rollback) = windows_settings::restore_autostart(Some(old_value)) {
                    return Err(format!("{error}; falha ao restaurar o registro de inicialização: {rollback}"));
                }
            } else if open_at_login.is_some() {
                if let Err(rollback) = windows_settings::restore_autostart(None) {
                    return Err(format!("{error}; falha ao restaurar o registro de inicialização: {rollback}"));
                }
            }
            return Err(error);
        }
    };

    if persist {
        if let Err(error) = persist_json_object(path, &object) {
            #[cfg(windows)]
            if let Some(old_value) = old_run_value.as_ref() {
                if let Err(rollback) = windows_settings::restore_autostart(old_value.as_deref()) {
                    return Err(format!("{error}; falha ao restaurar o registro de inicialização: {rollback}"));
                }
            }
            return Err(error);
        }
    }

    Ok(settings)
}

fn read_settings_object(path: &Path) -> Result<Map<String, Value>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Arquivo local de configurações inválido: {error}"))?;
            value
                .as_object()
                .cloned()
                .ok_or_else(|| "O arquivo local de configurações deve conter um objeto JSON.".to_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(error) => Err(format!("Não foi possível ler as configurações locais: {error}")),
    }
}

fn settings_from_object(object: &Map<String, Value>) -> Result<SystemSettings, String> {
    #[cfg(windows)]
    let open_at_login = windows_settings::autostart_is_enabled()?;
    #[cfg(not(windows))]
    let open_at_login = false;

    Ok(SystemSettings {
        open_at_login,
        start_minimized: read_bool_setting(object, "startMinimized", false)?,
        close_minimize_tray: read_bool_setting(object, "closeMinimizeTray", true)?,
        low_charge_tips: read_bool_setting(object, "lowChargeTips", true)?,
    })
}

fn validate_stored_settings(object: &Map<String, Value>) -> Result<(), String> {
    for key in [
        "openAtLogin",
        "startMinimized",
        "closeMinimizeTray",
        "lowChargeTips",
    ] {
        if let Some(value) = object.get(key) {
            if !value.is_boolean() {
                return Err(format!("A configuração local {key} não contém um valor booleano."));
            }
        }
    }
    Ok(())
}

fn read_bool_setting(object: &Map<String, Value>, key: &str, default: bool) -> Result<bool, String> {
    match object.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(format!("A configuração local {key} não contém um valor booleano.")),
    }
}

fn is_setting_key(key: &str) -> bool {
    matches!(
        key,
        "openAtLogin" | "startMinimized" | "closeMinimizeTray" | "lowChargeTips"
    )
}

fn validate_pointer_speed(value: i32) -> Result<(), String> {
    if (1..=20).contains(&value) {
        Ok(())
    } else {
        Err("A velocidade do ponteiro deve estar entre 1 e 20.".to_owned())
    }
}

fn validate_roller_speed(value: i32) -> Result<(), String> {
    if value == -1 || (1..=100).contains(&value) {
        Ok(())
    } else {
        Err("A velocidade da roda deve ser -1 (página inteira) ou de 1 a 100 linhas.".to_owned())
    }
}

fn roller_speed_from_windows(lines: u32) -> Result<i32, String> {
    if lines == u32::MAX {
        Ok(-1)
    } else {
        i32::try_from(lines)
            .map_err(|_| "O Windows retornou uma velocidade de roda fora do intervalo numérico.".to_owned())
    }
}

fn export_default_filename(configuration: &Value) -> String {
    let name = configuration
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("ATK configuration");
    let mut filename = String::new();
    for ch in name.chars().take(96) {
        if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            filename.push('_');
        } else {
            filename.push(ch);
        }
    }
    let filename = filename.trim().trim_end_matches('.').trim_end_matches(' ');
    let mut filename = if filename.is_empty() {
        "ATK configuration".to_owned()
    } else {
        filename.to_owned()
    };
    let stem = filename.split('.').next().unwrap_or_default();
    let reserved = ["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];
    if reserved.iter().any(|name| stem.eq_ignore_ascii_case(name)) {
        filename.insert(0, '_');
    }
    if !filename.to_ascii_lowercase().ends_with(".eq") {
        filename.push_str(".eq");
    }
    filename
}

fn persist_json_object(path: &Path, object: &Map<String, Value>) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(&Value::Object(object.clone()))
        .map_err(|error| format!("Não foi possível serializar as configurações: {error}"))?;
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "O caminho de destino não possui diretório pai.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Não foi possível criar o diretório de destino: {error}"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| "O caminho de destino não contém um nome de arquivo.".to_owned())?;
    let mut temp_name = file_name.to_os_string();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    temp_name.push(format!(".{}.{}.{}.tmp", std::process::id(), nonce, TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let temp_path = parent.join(temp_name);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| format!("Não foi possível criar o arquivo temporário: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("Não foi possível gravar o arquivo temporário: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Não foi possível confirmar a gravação: {error}"))?;
        drop(file);
        replace_file(&temp_path, path)
            .map_err(|error| format!("Não foi possível substituir o arquivo de destino: {error}"))
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            windows::core::PCWSTR(source.as_ptr()),
            windows::core::PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
mod windows_settings {
    use super::{validate_pointer_speed, validate_roller_speed};
    use std::path::PathBuf;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
        REG_SZ, REG_VALUE_TYPE,
    };
    use windows::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetSaveFileNameW, OPENFILENAMEW, OFN_EXPLORER, OFN_HIDEREADONLY,
        OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO,
        SystemParametersInfoW, SPI_GETMOUSESPEED, SPI_GETWHEELSCROLLLINES, SPI_SETMOUSESPEED,
        SPI_SETWHEELSCROLLLINES, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE,
    };

    const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");

    pub fn get_pointer_speed() -> Result<i32, String> {
        let mut speed = 0i32;
        unsafe {
            SystemParametersInfoW(
                SPI_GETMOUSESPEED,
                0,
                Some((&mut speed as *mut i32).cast()),
                Default::default(),
            )
        }
        .map_err(|error| format!("Não foi possível ler a velocidade do ponteiro: {error}"))?;
        validate_pointer_speed(speed)?;
        Ok(speed)
    }

    pub fn set_pointer_speed(speed: i32) -> Result<i32, String> {
        validate_pointer_speed(speed)?;
        unsafe {
            SystemParametersInfoW(
                SPI_SETMOUSESPEED,
                0,
                Some(speed as usize as *mut std::ffi::c_void),
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        }
        .map_err(|error| format!("O Windows recusou a alteração da velocidade do ponteiro: {error}"))?;
        let actual = get_pointer_speed()?;
        if actual != speed {
            return Err(format!("O Windows confirmou a velocidade {actual}, diferente do valor solicitado {speed}."));
        }
        Ok(actual)
    }

    pub fn get_roller_speed() -> Result<i32, String> {
        let mut lines = 0u32;
        unsafe {
            SystemParametersInfoW(
                SPI_GETWHEELSCROLLLINES,
                0,
                Some((&mut lines as *mut u32).cast()),
                Default::default(),
            )
        }
        .map_err(|error| format!("Não foi possível ler a velocidade da roda: {error}"))?;
        super::roller_speed_from_windows(lines)
    }

    pub fn set_roller_speed(lines: i32) -> Result<i32, String> {
        validate_roller_speed(lines)?;
        let requested = if lines == -1 { u32::MAX } else { lines as u32 };
        unsafe {
            SystemParametersInfoW(
                SPI_SETWHEELSCROLLLINES,
                requested,
                None,
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        }
        .map_err(|error| format!("O Windows recusou a alteração da velocidade da roda: {error}"))?;
        let actual = get_roller_speed()?;
        if actual != lines {
            return Err(format!("O Windows confirmou a velocidade da roda {actual}, diferente do valor solicitado {lines}."));
        }
        Ok(actual)
    }

    pub fn autostart_is_enabled() -> Result<bool, String> {
        let expected = expected_autostart_command()?;
        Ok(read_autostart_value()?.is_some_and(|value| value.eq_ignore_ascii_case(&expected)))
    }

    pub fn read_autostart_value() -> Result<Option<String>, String> {
        let mut key = HKEY::default();
        let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_QUERY_VALUE, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(format!("Não foi possível abrir a chave de inicialização do usuário (código {}).", status.0));
        }
        let key = RegistryKey::new(key);
        let result = (|| {
            let mut value_type = REG_VALUE_TYPE::default();
            let mut size = 0u32;
            let status = unsafe {
                RegQueryValueExW(
                    key.get(),
                    w!("ATKHubRust"),
                    None,
                    Some(&mut value_type),
                    None,
                    Some(&mut size),
                )
            };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(None);
            }
            if status != ERROR_SUCCESS {
                return Err(format!("Não foi possível ler o valor de inicialização do ATK HUB (código {}).", status.0));
            }
            if value_type != REG_SZ || size < 2 || size % 2 != 0 || size > 64 * 1024 {
                return Err("O valor de inicialização do ATK HUB tem formato inesperado.".to_owned());
            }
            let mut bytes = vec![0u8; size as usize];
            let status = unsafe {
                RegQueryValueExW(
                    key.get(),
                    w!("ATKHubRust"),
                    None,
                    Some(&mut value_type),
                    Some(bytes.as_mut_ptr()),
                    Some(&mut size),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(format!("Não foi possível concluir a leitura da inicialização do ATK HUB (código {}).", status.0));
            }
            let words = bytes[..size as usize]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|unit| *unit != 0)
                .collect::<Vec<_>>();
            String::from_utf16(&words)
                .map(Some)
                .map_err(|error| format!("O caminho de inicialização não contém texto UTF-16 válido: {error}"))
        })();
        finish_registry_operation(key, result)
    }

    pub fn write_autostart(enabled: bool) -> Result<(), String> {
        if enabled {
            write_autostart_value(Some(&expected_autostart_command()?))
        } else {
            write_autostart_value(None)
        }
    }

    pub fn restore_autostart(value: Option<&str>) -> Result<(), String> {
        write_autostart_value(value)
    }

    fn expected_autostart_command() -> Result<String, String> {
        use std::os::windows::ffi::OsStrExt;

        let executable = std::env::current_exe()
            .map_err(|error| format!("Não foi possível localizar o executável do ATK HUB: {error}"))?;
        let executable = String::from_utf16(&executable.as_os_str().encode_wide().collect::<Vec<_>>())
            .map_err(|error| format!("O caminho do ATK HUB não é UTF-16 válido: {error}"))?;
        Ok(format!("\"{executable}\" --autostart"))
    }

    fn write_autostart_value(value: Option<&str>) -> Result<(), String> {
        let mut key = HKEY::default();
        if value.is_none() {
            let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_SET_VALUE, &mut key) };
            if status == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            if status != ERROR_SUCCESS {
                return Err(format!("Não foi possível abrir a chave de inicialização para remoção (código {}).", status.0));
            }
            let key = RegistryKey::new(key);
            let status = unsafe { RegDeleteValueW(key.get(), w!("ATKHubRust")) };
            let result = if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("Não foi possível remover a inicialização do ATK HUB (código {}).", status.0))
            };
            return finish_registry_operation(key, result);
        }

        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!("Não foi possível abrir a chave de inicialização para gravação (código {}).", status.0));
        }
        let key = RegistryKey::new(key);
        let mut data: Vec<u8> = value
            .expect("checked above")
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        let status = unsafe {
            RegSetValueExW(key.get(), w!("ATKHubRust"), 0, REG_SZ, Some(&data))
        };
        data.fill(0);
        let result = if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("Não foi possível gravar a inicialização do ATK HUB (código {}).", status.0))
        };
        finish_registry_operation(key, result)
    }

    struct RegistryKey(Option<HKEY>);

    impl RegistryKey {
        fn new(key: HKEY) -> Self {
            Self(Some(key))
        }

        fn get(&self) -> HKEY {
            self.0.expect("registry key is still open")
        }

        fn close(mut self) -> Result<(), String> {
            let Some(key) = self.0.take() else {
                return Ok(());
            };
            let status = unsafe { RegCloseKey(key) };
            if status == ERROR_SUCCESS {
                Ok(())
            } else {
                Err(format!("Não foi possível fechar a chave de inicialização do usuário (código {}).", status.0))
            }
        }
    }

    fn finish_registry_operation<T>(key: RegistryKey, result: Result<T, String>) -> Result<T, String> {
        match (result, key.close()) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(close_error)) => Err(close_error),
            (Err(operation_error), Ok(())) => Err(operation_error),
            (Err(operation_error), Err(close_error)) => Err(format!("{operation_error}; {close_error}")),
        }
    }

    impl Drop for RegistryKey {
        fn drop(&mut self) {
            if let Some(key) = self.0.take() {
                let status = unsafe { RegCloseKey(key) };
                if status != ERROR_SUCCESS {
                    eprintln!("Não foi possível fechar uma chave de inicialização do usuário (código {}).", status.0);
                }
            }
        }
    }

    pub fn save_configuration(window: &tauri::Window, default_name: &str) -> Result<Option<PathBuf>, String> {
        use std::os::windows::ffi::OsStringExt;

        let hwnd = window
            .hwnd()
            .map_err(|error| format!("Não foi possível obter a janela do aplicativo: {error}"))?;
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0);
        let filter: Vec<u16> = "ATK configuration (*.eq)\0*.eq\0All files (*.*)\0*.*\0\0"
            .encode_utf16()
            .collect();
        let mut file_name = vec![0u16; 32_768];
        for (target, source) in file_name.iter_mut().zip(default_name.encode_utf16()) {
            *target = source;
        }
        let title: Vec<u16> = "Export ATK configuration".encode_utf16().chain(Some(0)).collect();
        let extension = [b'e' as u16, b'q' as u16, 0];
        let mut dialog = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: hwnd,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            nFilterIndex: 1,
            lpstrFile: windows::core::PWSTR(file_name.as_mut_ptr()),
            nMaxFile: file_name.len() as u32,
            lpstrTitle: PCWSTR(title.as_ptr()),
            lpstrDefExt: PCWSTR(extension.as_ptr()),
            Flags: OFN_EXPLORER | OFN_HIDEREADONLY | OFN_NOCHANGEDIR | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        let accepted = unsafe { GetSaveFileNameW(&mut dialog).as_bool() };
        if !accepted {
            let error = unsafe { CommDlgExtendedError() };
            if error.0 == 0 {
                return Ok(None);
            }
            return Err(format!("O diálogo de exportação falhou (código {}).", error.0));
        }
        let length = file_name.iter().position(|unit| *unit == 0).unwrap_or(file_name.len());
        if length == 0 {
            return Err("O diálogo retornou um caminho vazio.".to_owned());
        }
        let selected = std::ffi::OsString::from_wide(&file_name[..length]);
        let mut path = PathBuf::from(selected);
        let extension_changed = !path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("eq"));
        if extension_changed {
            path.set_extension("eq");
        }

        // The common dialog's overwrite prompt applies to the path the user
        // entered. If adding/replacing the extension changes that path, ask
        // about the actual `.eq` destination too before atomic replacement.
        if extension_changed {
            match std::fs::metadata(&path) {
                Ok(_) => {
                    let prompt: Vec<u16> = "O arquivo .eq de destino já existe. Deseja substituí-lo?"
                        .encode_utf16()
                        .chain(Some(0))
                        .collect();
                    let caption: Vec<u16> = "Confirmar substituição"
                        .encode_utf16()
                        .chain(Some(0))
                        .collect();
                    let answer = unsafe {
                        MessageBoxW(
                            hwnd,
                            PCWSTR(prompt.as_ptr()),
                            PCWSTR(caption.as_ptr()),
                            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                        )
                    };
                    if answer != IDYES {
                        return Ok(None);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "Não foi possível verificar o arquivo de destino: {error}"
                    ));
                }
            }
        }
        Ok(Some(path))
    }
}

#[tauri::command]
pub fn app_quit(app: AppHandle) {
    request_app_quit(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "atk-desktop-settings-test-{}-{}",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn settings_patch_preserves_unrecognized_stored_fields_and_defaults() {
        let directory = temp_dir();
        let path = directory.join(SETTINGS_FILE);
        fs::write(&path, r#"{"lowChargeTips":false,"futureSetting":{"enabled":true}}"#).unwrap();
        let updated = apply_settings_patch(&path, serde_json::json!({"startMinimized":true})).unwrap();
        assert_eq!(updated.start_minimized, true);
        assert_eq!(updated.low_charge_tips, false);
        assert_eq!(updated.close_minimize_tray, true);
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["futureSetting"]["enabled"], true);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn settings_patch_rejects_unknown_keys_and_non_boolean_values_without_writing() {
        let directory = temp_dir();
        let path = directory.join(SETTINGS_FILE);
        assert!(apply_settings_patch(&path, serde_json::json!({"future":true})).is_err());
        assert!(apply_settings_patch(&path, serde_json::json!({"lowChargeTips":1})).is_err());
        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_migration_preserves_unrelated_values_as_opaque_json() {
        let directory = temp_dir();
        let path = directory.join(SETTINGS_FILE);
        let migrated = migrate_legacy_settings(
            &path,
            serde_json::json!({
                "lowChargeTips": false,
                "unrelatedPreference": {"scriptText": "keep as data"}
            }),
        )
        .unwrap();
        assert!(!migrated.low_charge_tips);
        let saved: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["unrelatedPreference"]["scriptText"], "keep as data");
        let second_attempt = migrate_settings_if_absent(
            &path,
            serde_json::json!({"lowChargeTips": true, "unrelatedPreference": "replace"}),
        )
        .unwrap();
        assert!(!second_attempt.low_charge_tips);
        let unchanged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(unchanged["unrelatedPreference"]["scriptText"], "keep as data");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn system_preference_ranges_match_windows_controls() {
        assert!(validate_pointer_speed(1).is_ok());
        assert!(validate_pointer_speed(20).is_ok());
        assert!(validate_pointer_speed(0).is_err());
        assert!(validate_roller_speed(-1).is_ok());
        assert!(validate_roller_speed(1).is_ok());
        assert!(validate_roller_speed(100).is_ok());
        assert!(validate_roller_speed(0).is_err());
        assert!(validate_roller_speed(101).is_err());
        assert_eq!(roller_speed_from_windows(0).unwrap(), 0);
        assert_eq!(roller_speed_from_windows(4).unwrap(), 4);
        assert_eq!(roller_speed_from_windows(u32::MAX).unwrap(), -1);
    }

    #[test]
    fn export_filename_is_safe_and_keeps_eq_extension() {
        assert_eq!(
            export_default_filename(&serde_json::json!({"name":"Mouse: profile/1"})),
            "Mouse_ profile_1.eq"
        );
        assert_eq!(export_default_filename(&serde_json::json!({"name":"CON"})), "_CON.eq");
        assert_eq!(export_default_filename(&serde_json::json!({})), "ATK configuration.eq");
    }

    #[test]
    fn export_serializes_configuration_without_wrapper_and_keeps_unknown_fields() {
        let configuration = serde_json::json!({
            "name": "profile",
            "deviceSpecificFutureField": {"enabled": true}
        });
        let bytes = serialize_export_configuration(&configuration).unwrap();
        let decoded: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, configuration);
        assert!(decoded.get("configuration").is_none());
    }

    #[test]
    fn export_rejects_non_objects_and_raw_json_over_one_mebibyte() {
        assert!(serialize_export_configuration(&serde_json::json!([1, 2, 3])).is_err());
        let too_large = serde_json::json!({"payload":"x".repeat(MAX_EXPORT_BYTES)});
        assert!(serialize_export_configuration(&too_large).is_err());
    }

    #[test]
    fn export_atomic_write_replaces_only_the_requested_file() {
        let directory = temp_dir();
        let output = directory.join("profile.eq");
        fs::write(&output, b"old").unwrap();
        atomic_write(&output, br#"{"name":"new"}"#).unwrap();
        assert_eq!(fs::read(&output).unwrap(), br#"{"name":"new"}"#);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
