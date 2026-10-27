use std::sync::Mutex;
use std::time::{Duration, Instant};

use rdm_manager::Manager;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager as _, PhysicalPosition};

use crate::profile::{self, Profile};

const TRAY_ID: &str = "main";
/// macOS tints a black template icon to suit the menu bar; Windows shows
/// tray icons as drawn, so it gets the full-colour app icon.
#[cfg(target_os = "macos")]
const TRAY_ICON: &[u8] = include_bytes!("../icons/tray.png");
#[cfg(not(target_os = "macos"))]
const TRAY_ICON: &[u8] = include_bytes!("../icons/32x32.png");
/// Panel width in points; matches `panel` in tauri.conf.json.
const PANEL_WIDTH: f64 = 360.0;

/// The Speed submenu's items, kept to update their check marks.
struct ProfileItems {
    full: CheckMenuItem<tauri::Wry>,
    background: CheckMenuItem<tauri::Wry>,
    paused: CheckMenuItem<tauri::Wry>,
}

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let item = |id: &str, label: &str| MenuItem::with_id(app, id, label, true, None::<&str>);
    let check = |id: &str, label: &str| CheckMenuItem::with_id(app, id, label, true, false, None::<&str>);
    let items = ProfileItems {
        full: check("profile_full", "Full speed")?,
        background: check("profile_background", "Background")?,
        paused: check("profile_paused", "Paused")?,
    };
    let speed = Submenu::with_items(app, "Speed", true, &[&items.full, &items.background, &items.paused])?;
    let menu = Menu::with_items(app, &[
        &item("show", "Show Flash")?,
        &item("add", "Add URL…")?,
        &PredefinedMenuItem::separator(app)?,
        &speed,
        &item("resume_all", "Resume all")?,
        &item("pause_all", "Pause all")?,
        &PredefinedMenuItem::separator(app)?,
        &item("quit", "Quit Flash")?,
    ])?;
    app.manage(items);

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(Image::from_bytes(TRAY_ICON)?)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("Flash")
        .menu(&menu)
        // Left click opens the panel; the menu is on right click.
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. } =
                event
            {
                toggle_panel(tray.app_handle(), position);
            }
        })
        .on_menu_event(|app, event| {
            let manager = app.state::<Manager>();
            let profile = match event.id.as_ref() {
                "show" => return show_main(app),
                "add" => {
                    show_main(app);
                    let _ = app.emit("rdm-show-add", ());
                    return;
                }
                "resume_all" => {
                    let _ = manager.resume_all();
                    return profile::leave_paused(app);
                }
                "pause_all" => return drop(manager.pause_all()),
                "quit" => return app.exit(0),
                "profile_full" => Profile::Full,
                "profile_background" => Profile::Background,
                "profile_paused" => Profile::Paused,
                _ => return,
            };
            if let Err(e) = profile::set(app, profile) {
                eprintln!("rdm: speed profile: {e:#}");
            }
            // A check item toggles itself when clicked; put the marks right.
            profile::changed(app);
        })
        .build(app)?;
    profile::changed(app);
    Ok(())
}

/// Ticks the active profile in the Speed submenu.
pub fn show_profile(app: &AppHandle, profile: Profile) {
    let Some(items) = app.try_state::<ProfileItems>() else { return };
    let _ = items.full.set_checked(profile == Profile::Full);
    let _ = items.background.set_checked(profile == Profile::Background);
    let _ = items.paused.set_checked(profile == Profile::Paused);
    let limit = app.state::<Manager>().settings().background_limit;
    let _ = items.background.set_text(format!("Background ({}/s)", human_bytes(limit as f64)));
}

/// When the panel was last shown, so the focus change from opening it
/// doesn't close it straight away.
static PANEL_SHOWN: Mutex<Option<Instant>> = Mutex::new(None);

/// Shows the panel next to the menu bar or taskbar where it was clicked, or
/// hides it if it's showing. Placed from the cursor and the screen's usable
/// area rather than the icon's frame, which macOS doesn't always report
/// correctly.
fn toggle_panel(app: &AppHandle, cursor: PhysicalPosition<f64>) {
    let Some(panel) = app.get_webview_window("panel") else { return };
    if panel.is_visible().unwrap_or(false) {
        let _ = panel.hide();
        return;
    }
    let monitor = app.monitor_from_point(cursor.x, cursor.y).ok().flatten()
        .or_else(|| app.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else { return };
    let scale = monitor.scale_factor();
    let area = monitor.work_area();
    let (left, top) = (area.position.x as f64, area.position.y as f64);
    let (right, bottom) = (left + area.size.width as f64, top + area.size.height as f64);
    let width = PANEL_WIDTH * scale;
    let height = panel.outer_size().map(|s| s.height as f64).unwrap_or(480.0 * scale);
    let gap = 6.0 * scale;
    let x = (cursor.x - width / 2.0).clamp(left + gap, right - width - gap);
    // The menu bar is at the top; a Windows taskbar is usually at the bottom.
    let y = if cursor.y > (top + bottom) / 2.0 { bottom - height - gap } else { top + gap };
    let _ = panel.set_position(PhysicalPosition::new(x, y));
    *PANEL_SHOWN.lock().unwrap() = Some(Instant::now());
    let _ = panel.show();
    let _ = panel.set_focus();
}

/// Hides the panel when you click elsewhere, but not while it is still
/// opening (showing it can briefly move focus around).
pub fn panel_lost_focus(panel: &tauri::Window) {
    let opening = PANEL_SHOWN.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(500));
    if !opening {
        let _ = panel.hide();
    }
}

/// Shows the total download speed next to the menu bar icon, or in the tray
/// icon's tooltip on Windows, where icons can't have text.
pub fn set_speed(app: &AppHandle, bytes_per_sec: f64) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let speed = (bytes_per_sec >= 1.0).then(|| format!("{}/s", human_bytes(bytes_per_sec)));
        if cfg!(target_os = "macos") {
            let _ = tray.set_title(speed);
        } else {
            let _ = tray.set_tooltip(Some(speed.map_or("Flash".into(), |s| format!("Flash · {s}"))));
        }
    }
}

pub fn show_main(app: &AppHandle) {
    if let Some(panel) = app.get_webview_window("panel") {
        let _ = panel.hide();
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn human_bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut n, mut i) = (n, 0);
    while n >= 1024.0 && i < UNITS.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 || n >= 100.0 { format!("{n:.0} {}", UNITS[i]) } else { format!("{n:.1} {}", UNITS[i]) }
}
