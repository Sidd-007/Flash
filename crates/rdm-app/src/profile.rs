//! Speed profiles: one-click switches between full speed, a gentle
//! background limit, and pausing everything.

use std::sync::Mutex;

use rdm_manager::{Manager, Status};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    Full,
    Background,
    Paused,
    /// Some other global limit, set in Settings.
    Custom,
}

/// Downloads that "Paused" stopped, so switching back resumes only those.
#[derive(Default)]
pub struct PausedByProfile(Mutex<Option<Vec<i64>>>);

pub fn current(app: &AppHandle) -> Profile {
    if app.state::<PausedByProfile>().0.lock().unwrap().is_some() {
        return Profile::Paused;
    }
    let s = app.state::<Manager>().settings();
    match s.speed_limit {
        0 => Profile::Full,
        l if l == s.background_limit => Profile::Background,
        _ => Profile::Custom,
    }
}

pub fn set(app: &AppHandle, profile: Profile) -> anyhow::Result<()> {
    let manager = app.state::<Manager>();
    let paused = app.state::<PausedByProfile>();
    if profile == Profile::Paused {
        let ids: Vec<i64> = manager.list()?.iter()
            .filter(|d| matches!(d.download.status, Status::Running | Status::Queued))
            .map(|d| d.download.id)
            .collect();
        for &id in &ids {
            manager.pause(id)?;
        }
        paused.0.lock().unwrap().get_or_insert_default().extend(ids);
    } else {
        let mut settings = manager.settings();
        settings.speed_limit = match profile {
            Profile::Background => settings.background_limit,
            _ => 0,
        };
        manager.update_settings(settings)?;
        if let Some(ids) = paused.0.lock().unwrap().take() {
            for id in ids {
                // Skips anything removed or resumed by hand since.
                if manager.get(id)?.is_some_and(|d| d.download.status == Status::Paused) {
                    manager.resume(id)?;
                }
            }
        }
    }
    changed(app);
    Ok(())
}

/// Tells the menu bar and the windows that the profile may have changed.
pub fn changed(app: &AppHandle) {
    let profile = current(app);
    crate::tray::show_profile(app, profile);
    let _ = app.emit("rdm-profile", profile);
}

/// Starting downloads by hand ends the "Paused" profile.
pub fn leave_paused(app: &AppHandle) {
    if app.state::<PausedByProfile>().0.lock().unwrap().take().is_some() {
        changed(app);
    }
}
