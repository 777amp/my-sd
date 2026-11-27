// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Linux system tray integration (StatusNotifierItem / Ayatana AppIndicator)
//! replacing `app/SystemTrayService.main.ts`.

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, Runtime,
};
use tracing::info;

pub const TRAY_ID: &str = "signal-tray";

pub fn setup_tray<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, "tray_show", "Show Signal", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, "tray_hide", "Hide Signal", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "tray_quit", "Quit Signal", true, None::<&str>)?;

    let menu = Menu::with_items(app, &[&show_item, &hide_item, &quit_item])?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .tooltip("Signal")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "tray_show" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                }
            }
            "tray_hide" => {
                if let Some(win) = app.get_webview_window("main") {
                    let _ = win.hide();
                }
            }
            "tray_quit" => {
                info!("Quit requested from system tray");
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(win) = app.get_webview_window("main") {
                    let visible = win.is_visible().unwrap_or(false);
                    if visible {
                        let _ = win.hide();
                    } else {
                        let _ = win.show();
                        let _ = win.unminimize();
                        let _ = win.set_focus();
                    }
                }
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    builder.build(app)?;
    Ok(())
}

pub fn update_tray_unread_count<R: Runtime>(app: &AppHandle<R>, unread_count: u64) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let tooltip = if unread_count == 0 {
            "Signal".to_string()
        } else {
            format!("Signal ({unread_count} unread)")
        };
        let _ = tray.set_tooltip(Some(&tooltip));
    }
}
