//! Watches the clipboard and offers to download copied links, like IDM.

use std::sync::Mutex;
use std::time::Duration;

use rdm_manager::Manager;
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager as _};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::{os, tray};

/// Text Flash itself put on the clipboard (Copy URL), which must not echo back.
static OWN_COPY: Mutex<Option<String>> = Mutex::new(None);

pub fn note_own_copy(text: &str) {
    *OWN_COPY.lock().unwrap() = Some(text.trim().to_owned());
}

pub fn start(app: AppHandle, manager: Manager) {
    std::thread::spawn(move || {
        // Only the change counter is polled; the text is read (which macOS
        // may ask permission for) only after something new was copied.
        let mut seen = os::clipboard_change_count();
        let mut last_offered = String::new();
        loop {
            std::thread::sleep(Duration::from_millis(700));
            let count = os::clipboard_change_count();
            if count == seen {
                continue;
            }
            seen = count;
            if !manager.settings().clipboard_watch {
                continue;
            }
            // Copying inside Flash itself shouldn't pop Flash's dialog.
            let flash_focused = app.get_webview_window("main").and_then(|w| w.is_focused().ok()).unwrap_or(false);
            if flash_focused {
                continue;
            }
            let Ok(text) = app.clipboard().read_text() else { continue };
            let url = text.trim().to_owned();
            if url.is_empty() || url.contains(char::is_whitespace) || url == last_offered {
                continue;
            }
            if OWN_COPY.lock().unwrap().as_deref() == Some(url.as_str()) || !manager.is_download_link(&url) {
                continue;
            }
            last_offered = url.clone();
            tray::show_main(&app);
            let _ = app.emit("rdm-incoming", json!({ "urls": [url], "source": "clipboard" }));
        }
    });
}
