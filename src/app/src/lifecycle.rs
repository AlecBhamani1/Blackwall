//! Keep the macOS main window alive when closed, and restore it from the Dock.

use tauri::{AppHandle, Manager, RunEvent, Window, WindowEvent};

pub fn on_window_event(window: &Window, event: &WindowEvent) {
    if window.label() == "main" {
        if let WindowEvent::CloseRequested { api, .. } = event {
            // Preserve the webview and running work. Explicit Quit still exits the app.
            api.prevent_close();
            if let Err(error) = window.hide() {
                eprintln!("Blackwall could not hide its main window: {error}");
            }
        }
    }
}

pub fn on_run_event(app: &AppHandle, event: RunEvent) {
    if let RunEvent::Reopen { .. } = event {
        if let Some(window) = app.get_webview_window("main") {
            if let Err(error) = window.unminimize() {
                eprintln!("Blackwall could not unminimize its main window: {error}");
            }
            if let Err(error) = window.show() {
                eprintln!("Blackwall could not show its main window: {error}");
            }
            if let Err(error) = window.set_focus() {
                eprintln!("Blackwall could not focus its main window: {error}");
            }
        }
    }
}
