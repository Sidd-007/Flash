//! RDM desktop app: a Tauri shell around `rdm-manager`.

mod bridge;
mod clipboard;
mod commands;
mod profile;
mod tray;

use std::sync::Arc;

use rdm_manager::{Event, Manager};
use tauri::{Emitter, Manager as _, RunEvent, WindowEvent};
use tauri_plugin_notification::NotificationExt;
use tokio::sync::broadcast::error::RecvError;

fn main() {
    let start_hidden = std::env::args().any(|a| a == "--hidden");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(move |app| {
            let db_path = app.path().app_data_dir()?.join("rdm.sqlite3");
            // The manager spawns its queue tasks, so open it inside the runtime.
            let manager = tauri::async_runtime::block_on(async { Manager::open(&db_path) })?;
            app.manage(manager.clone());

            let bridge = Arc::new(bridge::Status::default());
            app.manage(bridge.clone());
            let capture = Arc::new(bridge::LinkCapture::default());
            app.manage(capture.clone());
            app.manage(profile::PausedByProfile::default());
            bridge::start(app.handle().clone(), manager.clone(), bridge, capture);

            tray::create(app.handle())?;
            clipboard::start(app.handle().clone(), manager.clone());
            tauri::async_runtime::spawn(forward_events(app.handle().clone(), manager));

            if !start_hidden {
                tray::show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // Closing the window keeps downloads running in the menu bar.
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            // The menu bar panel closes when you click elsewhere.
            WindowEvent::Focused(false) if window.label() == "panel" => tray::panel_lost_focus(window),
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_downloads,
            commands::get_stats,
            commands::add_downloads,
            commands::pause,
            commands::resume,
            commands::redownload,
            commands::remove,
            commands::pause_all,
            commands::resume_all,
            commands::clear_completed,
            commands::set_speed_limit,
            commands::segments,
            commands::get_settings,
            commands::save_settings,
            commands::open_download,
            commands::reveal_download,
            commands::clipboard_text,
            commands::copy_text,
            commands::choose_folder,
            commands::get_autostart,
            commands::set_autostart,
            commands::bridge_status,
            commands::tools_status,
            commands::get_profile,
            commands::set_profile,
            commands::find_duplicates,
            commands::relink,
            commands::await_link,
            commands::show_main_window,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build RDM");

    app.run(|app, event| {
        // Clicking the Dock icon brings the window back.
        if let RunEvent::Reopen { .. } = event {
            tray::show_main(app);
        }
    });
}

/// Relays manager events to the UI, the menu bar and notifications.
async fn forward_events(app: tauri::AppHandle, manager: Manager) {
    let mut rx = manager.subscribe();
    // `caffeinate` holds off idle sleep while downloads run; it exits with us.
    let mut awake: Option<std::process::Child> = None;
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return,
        };
        match &event {
            Event::Progress { total_speed, items } => {
                tray::set_speed(&app, *total_speed);
                let want = !items.is_empty() && manager.settings().keep_awake;
                if want && awake.is_none() {
                    awake = std::process::Command::new("/usr/bin/caffeinate")
                        .args(["-i", "-w", &std::process::id().to_string()])
                        .spawn()
                        .ok();
                } else if !want && let Some(mut child) = awake.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            Event::Finished { name, ok, error, .. } if manager.settings().notify => {
                let (title, body) = if *ok {
                    ("Download complete", name.clone())
                } else {
                    ("Download failed", format!("{name}: {}", error.as_deref().unwrap_or("unknown error")))
                };
                let _ = app.notification().builder().title(title).body(body).show();
            }
            _ => {}
        }
        let _ = app.emit("rdm-event", &event);
    }
}
