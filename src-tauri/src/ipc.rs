// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Tauri v2 command handlers implementing Signal Desktop's core IPC surface.
//!
//! Replaces Electron `ipcMain.handle` / `ipcMain.on` handlers from `app/main.main.ts`,
//! `app/sql_channel.main.ts`, `app/attachment_channel.main.ts`, `ts/main/*`, and `ts/services/*`.

use crate::attachments::{
    ATTACHMENTS_DIR, DOWNLOADS_DIR, DRAFTS_DIR, STICKERS_DIR, TEMP_DIR,
};
use crate::tray::update_tray_unread_count;
use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State, UserAttentionType};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tracing::{info, warn};

#[derive(Debug, Serialize)]
pub struct SqlRpcEnvelope {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SqlRpcEnvelope {
    fn from_result(res: anyhow::Result<Value>) -> Self {
        match res {
            Ok(v) => Self {
                ok: true,
                value: Some(v),
                error: None,
            },
            Err(e) => Self {
                ok: false,
                value: None,
                error: Some(format!("{e:#}")),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Config & Environment
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn get_config(app: AppHandle, state: State<'_, AppState>) -> Value {
    let version = app.package_info().version.to_string();
    state.config.build_renderer_config(&version)
}

#[tauri::command]
pub fn get_user_data_path(state: State<'_, AppState>) -> String {
    state
        .config
        .user_data_dir()
        .to_string_lossy()
        .into_owned()
}

#[tauri::command]
pub fn os_get_class_name() -> String {
    "os-linux".to_string()
}

#[tauri::command]
pub fn settings_get(state: State<'_, AppState>, key: String) -> Option<Value> {
    state.config.get_user(&key)
}

#[tauri::command]
pub fn settings_set(
    state: State<'_, AppState>,
    key: String,
    value: Value,
) -> Result<(), String> {
    state
        .config
        .set_user(&key, value)
        .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// SQLCipher IPC (`sql-channel:read`, `sql-channel:write`, etc.)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sql_read(
    state: State<'_, AppState>,
    call_name: String,
    args: Vec<Value>,
) -> Result<SqlRpcEnvelope, String> {
    let sql = state.sql.clone();
    let res = tokio::task::spawn_blocking(move || sql.dispatch_read(&call_name, &args))
        .await
        .map_err(|e| format!("SQL read task panicked: {e}"))?;
    Ok(SqlRpcEnvelope::from_result(res))
}

#[tauri::command]
pub async fn sql_write(
    state: State<'_, AppState>,
    call_name: String,
    args: Vec<Value>,
) -> Result<SqlRpcEnvelope, String> {
    let sql = state.sql.clone();
    let res = tokio::task::spawn_blocking(move || sql.dispatch_write(&call_name, &args))
        .await
        .map_err(|e| format!("SQL write task panicked: {e}"))?;
    Ok(SqlRpcEnvelope::from_result(res))
}

#[tauri::command]
pub async fn sql_remove_db(state: State<'_, AppState>) -> Result<(), String> {
    let sql = state.sql.clone();
    tokio::task::spawn_blocking(move || sql.remove_db())
        .await
        .map_err(|e| format!("removeDB panicked: {e}"))?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn erase_sql_key(state: State<'_, AppState>) -> Result<(), String> {
    state.config.remove_all_ephemeral();
    state
        .config
        .remove_all_user_config()
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub fn pause_sql_writes(state: State<'_, AppState>) {
    state.sql.pause_write_access();
}

#[tauri::command]
pub fn resume_sql_writes(state: State<'_, AppState>) {
    state.sql.resume_write_access();
}

// ---------------------------------------------------------------------------
// Attachments & Storage Cleanup (`attachment_channel.main.ts`)
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn erase_attachments(state: State<'_, AppState>) -> Result<usize, String> {
    let att = state.attachments.clone();
    tokio::task::spawn_blocking(move || att.erase_subdir(ATTACHMENTS_DIR))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn erase_stickers(state: State<'_, AppState>) -> Result<usize, String> {
    let att = state.attachments.clone();
    tokio::task::spawn_blocking(move || att.erase_subdir(STICKERS_DIR))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn erase_temp(state: State<'_, AppState>) -> Result<usize, String> {
    let att = state.attachments.clone();
    tokio::task::spawn_blocking(move || att.erase_subdir(TEMP_DIR))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn erase_drafts(state: State<'_, AppState>) -> Result<usize, String> {
    let att = state.attachments.clone();
    tokio::task::spawn_blocking(move || att.erase_subdir(DRAFTS_DIR))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn erase_downloads(state: State<'_, AppState>) -> Result<usize, String> {
    let att = state.attachments.clone();
    tokio::task::spawn_blocking(move || att.erase_subdir(DOWNLOADS_DIR))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn cleanup_downloads(state: State<'_, AppState>) -> Result<usize, String> {
    let att = state.attachments.clone();
    tokio::task::spawn_blocking(move || att.cleanup_stale_downloads())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn cleanup_orphaned_attachments(
    state: State<'_, AppState>,
    known_paths: Vec<String>,
) -> Result<usize, String> {
    let att = state.attachments.clone();
    let set: HashSet<String> = known_paths.into_iter().collect();
    tokio::task::spawn_blocking(move || att.cleanup_orphaned_attachments(&set))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

// ---------------------------------------------------------------------------
// Clipboard (`ts/main/clipboardMain.main.ts`)
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn clipboard_write_text(app: AppHandle, text: String) -> Result<(), String> {
    app.clipboard()
        .write_text(text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clipboard_clear(app: AppHandle) -> Result<(), String> {
    app.clipboard().clear().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clipboard_clear_if_needed(
    app: AppHandle,
    expected_text: String,
) -> Result<bool, String> {
    let current = app.clipboard().read_text().unwrap_or_default();
    if current == expected_text {
        app.clipboard().clear().map_err(|e| e.to_string())?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ---------------------------------------------------------------------------
// Window, Tray, Zoom, Dialogs, Autostart, Lifecycle
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn show_window(app: AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[tauri::command]
pub fn draw_attention(app: AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.request_user_attention(Some(UserAttentionType::Informational));
    }
}

#[tauri::command]
pub fn set_badge_count(app: AppHandle, badge_count: u64) {
    update_tray_unread_count(&app, badge_count);
}

#[tauri::command]
pub fn update_tray_icon(app: AppHandle, unread_count: u64) {
    update_tray_unread_count(&app, unread_count);
}

#[tauri::command]
pub fn get_zoom_factor(state: State<'_, AppState>) -> f64 {
    state
        .config
        .get_user("zoomFactor")
        .and_then(|v| v.as_f64())
        .unwrap_or(1.0)
}

#[tauri::command]
pub fn set_zoom_factor(
    app: AppHandle,
    state: State<'_, AppState>,
    zoom_factor: f64,
) -> Result<(), String> {
    let clamped = zoom_factor.clamp(0.5, 3.0);
    state
        .config
        .set_user("zoomFactor", json!(clamped))
        .map_err(|e| format!("{e:#}"))?;
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.set_zoom(clamped);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct SaveDialogOptions {
    pub default_path: Option<String>,
}

#[tauri::command]
pub async fn show_save_dialog(
    app: AppHandle,
    options: SaveDialogOptions,
) -> Result<Option<String>, String> {
    let mut builder = app.dialog().file();
    if let Some(name) = options.default_path {
        builder = builder.set_file_name(name);
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    builder.save_file(move |file_path| {
        let _ = tx.send(file_path.map(|p| p.to_string()));
    });

    rx.await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn show_item_in_folder(path: String) -> Result<(), String> {
    let target = PathBuf::from(&path);
    let dir = if target.is_file() {
        target.parent().unwrap_or(&target).to_path_buf()
    } else {
        target
    };
    std::process::Command::new("xdg-open")
        .arg(dir)
        .spawn()
        .map_err(|e| format!("Failed to open folder with xdg-open: {e}"))?;
    Ok(())
}

/// Linux XDG autostart (`~/.config/autostart/signal-desktop.desktop`).
fn autostart_desktop_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart")
        .join("signal-desktop.desktop")
}

#[tauri::command]
pub fn get_auto_launch() -> bool {
    autostart_desktop_path().exists()
}

#[tauri::command]
pub fn set_auto_launch(enabled: bool) -> Result<(), String> {
    let path = autostart_desktop_path();
    if enabled {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let exe = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/usr/bin/signal-desktop".to_string());
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=Signal\nExec={exe} --start-in-tray\nX-GNOME-Autostart-enabled=true\n"
        );
        fs::write(&path, content).map_err(|e| e.to_string())?;
    } else if path.exists() {
        fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub fn restart_app(app: AppHandle) {
    info!("Restart requested by renderer");
    app.restart();
}

#[tauri::command]
pub async fn shutdown_app(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    info!("Shutdown requested by renderer");
    state.sidecar.shutdown().await;
    state.sql.close();
    app.exit(0);
    Ok(())
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn log_from_renderer(
    state: State<'_, AppState>,
    level: String,
    scope: String,
    message: String,
) {
    state.logs.write_line(&level, &scope, &message);
}

#[tauri::command]
pub fn delete_all_logs(state: State<'_, AppState>) -> Result<(), String> {
    state.logs.delete_all_logs().map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Headless Node.js Signal Protocol Sidecar Bridge
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn sidecar_invoke(
    state: State<'_, AppState>,
    method: String,
    params: Value,
) -> Result<Value, String> {
    state
        .sidecar
        .invoke(&method, params)
        .await
        .map_err(|e| {
            warn!("sidecar_invoke({method}) failed: {e:#}");
            format!("{e:#}")
        })
}

#[tauri::command]
pub async fn sidecar_status(state: State<'_, AppState>) -> Result<Value, String> {
    Ok(json!({
        "running": state.sidecar.is_running().await,
    }))
}

// ---------------------------------------------------------------------------
// Pure-Rust Signal Protocol (presage) — QR Linking & E2EE Messaging
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn signal_status(state: State<'_, AppState>) -> crate::signal_client::SignalStatus {
    state.signal.status()
}

#[tauri::command]
pub async fn signal_start_linking(
    state: State<'_, AppState>,
    device_name: Option<String>,
) -> Result<(), String> {
    let name = device_name
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Signal-Pi4".to_string());
    state
        .signal
        .start_linking(name)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn signal_send_message(
    state: State<'_, AppState>,
    recipient_type: String,
    recipient_id: String,
    body: String,
) -> Result<Value, String> {
    state
        .signal
        .send_message(recipient_type, recipient_id, body)
        .await
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn signal_list_threads(state: State<'_, AppState>) -> Result<Value, String> {
    state
        .signal
        .list_contacts_and_groups()
        .await
        .map_err(|e| format!("{e:#}"))
}
