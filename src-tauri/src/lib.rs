// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

pub mod attachments;
pub mod config;
pub mod ipc;
pub mod logging;
pub mod sidecar;
pub mod signal_client;
pub mod sql;
pub mod tray;

use attachments::{AttachmentStore, ATTACHMENTS_DIR, AVATARS_DIR, BADGES_DIR, DRAFTS_DIR, STICKERS_DIR};
use config::{resolve_user_data_dir, ConfigStore};
use logging::LogSink;
use sidecar::{resolve_sidecar_script, SidecarManager};
use signal_client::SignalClient;
use sql::SqlEngine;
use std::sync::Arc;
use tauri::{http::Response, Emitter, Manager};
use tracing::{error, info, warn};

pub struct AppState {
    pub config: Arc<ConfigStore>,
    pub sql: Arc<SqlEngine>,
    pub attachments: Arc<AttachmentStore>,
    pub sidecar: Arc<SidecarManager>,
    pub signal: Arc<SignalClient>,
    pub logs: Arc<LogSink>,
}

/// Applies Raspberry Pi 4B / Linux WebKitGTK tuning before GTK/WebKit initializes.
pub fn configure_linux_rpi4_env() {
    #[cfg(target_os = "linux")]
    {
        // Escape hatch matching README.md: SIGNAL_RPI_SW_RENDER=1 forces software rendering
        if std::env::var("SIGNAL_RPI_SW_RENDER").as_deref() == Ok("1") {
            std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        } else if std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_err() {
            // VideoCore VI + Wayland DMA-BUF in WebKitGTK 4.1 can stall on some RPi4 kernels
            // if EGL DMA-BUF is unavailable; allow override via env.
            if std::env::var("SIGNAL_RPI_ENABLE_DMABUF").as_deref() != Ok("1") {
                std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            }
        }
    }
}

/// Handles `attachment://<subdir>/<relative_path>?keys=<base64>` custom URI requests
/// directly in Rust without bouncing through Electron/Node.js.
fn handle_attachment_protocol(
    store: &AttachmentStore,
    uri: &tauri::http::Uri,
) -> Response<Vec<u8>> {
    let full_url = uri.to_string();
    let parsed = match url::Url::parse(&full_url) {
        Ok(u) => u,
        Err(err) => {
            return Response::builder()
                .status(400)
                .body(format!("Invalid attachment URI: {err}").into_bytes())
                .unwrap();
        }
    };

    let host = parsed.host_str().unwrap_or("");
    let subdir = match host {
        "attachments" | "attachments.noindex" => ATTACHMENTS_DIR,
        "avatars" | "avatars.noindex" => AVATARS_DIR,
        "stickers" | "stickers.noindex" => STICKERS_DIR,
        "drafts" | "drafts.noindex" => DRAFTS_DIR,
        "badges" | "badges.noindex" => BADGES_DIR,
        _ => ATTACHMENTS_DIR,
    };

    let rel_path = parsed.path().trim_start_matches('/');
    let keys_b64 = parsed
        .query_pairs()
        .find(|(k, _)| k == "keys")
        .map(|(_, v)| v.into_owned());

    match store.read_attachment(subdir, rel_path, keys_b64.as_deref()) {
        Ok((bytes, mime)) => Response::builder()
            .status(200)
            .header("Content-Type", mime)
            .header("Cache-Control", "private, max-age=31536000")
            .body(bytes)
            .unwrap(),
        Err(err) => {
            warn!("attachment:// failed for {full_url}: {err:#}");
            Response::builder()
                .status(404)
                .body(format!("Attachment not found: {err}").into_bytes())
                .unwrap()
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    configure_linux_rpi4_env();
    logging::init_tracing();

    let user_data_dir = resolve_user_data_dir();
    info!("Signal Desktop (Rust/Tauri v2) userData: {}", user_data_dir.display());

    let attachments_for_protocol = Arc::new(AttachmentStore::new(user_data_dir.clone()));

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_shell::init())
        .register_uri_scheme_protocol("attachment", {
            let att = Arc::clone(&attachments_for_protocol);
            move |_ctx, request| handle_attachment_protocol(&att, request.uri())
        })
        .setup(move |app| {
            let resource_dir = app.path().resource_dir().ok();
            let config = Arc::new(
                ConfigStore::load(user_data_dir.clone(), resource_dir.as_deref())
                    .expect("Failed to initialize ConfigStore"),
            );
            let logs = Arc::new(LogSink::new(&user_data_dir));

            // Resolve SQLCipher key (supporting existing Electron safeStorage keys)
            let sql_key = tauri::async_runtime::block_on(config.get_or_create_sql_key())
                .expect("Failed to resolve SQLCipher key");

            let sql = Arc::new(
                SqlEngine::open(&user_data_dir, &sql_key)
                    .expect("Failed to open SQLCipher database"),
            );

            let sidecar_script = resolve_sidecar_script(resource_dir.as_deref());
            let sidecar = Arc::new(SidecarManager::new(
                sidecar_script,
                user_data_dir.clone(),
            ));

            let handle_sidecar = app.handle().clone();
            sidecar.set_event_callback(Arc::new(move |event_name, payload| {
                if let Err(err) = handle_sidecar.emit(event_name, payload) {
                    error!("Failed to emit sidecar event '{event_name}': {err}");
                }
            }));

            // Spawn pure-Rust Signal protocol engine (presage)
            let handle_signal = app.handle().clone();
            let signal = Arc::new(SignalClient::spawn(
                user_data_dir.clone(),
                sql_key,
                Arc::new(move |event_name, payload| {
                    if let Err(err) = handle_signal.emit(event_name, payload) {
                        error!("Failed to emit Signal event '{event_name}': {err}");
                    }
                }),
            ));

            let state = AppState {
                config,
                sql,
                attachments: Arc::clone(&attachments_for_protocol),
                sidecar,
                signal,
                logs,
            };

            app.manage(state);

            if let Err(err) = tray::setup_tray(app.handle()) {
                warn!("System tray unavailable: {err}");
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ipc::get_config,
            ipc::get_user_data_path,
            ipc::os_get_class_name,
            ipc::settings_get,
            ipc::settings_set,
            ipc::sql_read,
            ipc::sql_write,
            ipc::sql_remove_db,
            ipc::erase_sql_key,
            ipc::pause_sql_writes,
            ipc::resume_sql_writes,
            ipc::erase_attachments,
            ipc::erase_stickers,
            ipc::erase_temp,
            ipc::erase_drafts,
            ipc::erase_downloads,
            ipc::cleanup_downloads,
            ipc::cleanup_orphaned_attachments,
            ipc::clipboard_write_text,
            ipc::clipboard_clear,
            ipc::clipboard_clear_if_needed,
            ipc::show_window,
            ipc::draw_attention,
            ipc::set_badge_count,
            ipc::update_tray_icon,
            ipc::get_zoom_factor,
            ipc::set_zoom_factor,
            ipc::show_save_dialog,
            ipc::show_item_in_folder,
            ipc::get_auto_launch,
            ipc::set_auto_launch,
            ipc::restart_app,
            ipc::shutdown_app,
            ipc::log_from_renderer,
            ipc::delete_all_logs,
            ipc::sidecar_invoke,
            ipc::sidecar_status,
            ipc::signal_status,
            ipc::signal_start_linking,
            ipc::signal_send_message,
            ipc::signal_list_threads,
        ]);

    builder
        .run(tauri::generate_context!())
        .expect("Error while running Signal Desktop (Tauri v2)");
}
