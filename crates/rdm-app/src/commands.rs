//! Commands the UI calls through `invoke`.

use std::path::PathBuf;
use std::sync::Arc;

use rdm_manager::{AddRequest, DownloadView, Duplicate, Manager, SegmentView, Settings, Stats};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;

use crate::{bridge, os};
use crate::profile::{self, Profile};

type Res<T> = Result<T, String>;

fn err(e: impl std::fmt::Display) -> String {
    // anyhow's alternate format includes the cause chain.
    format!("{e:#}")
}

#[tauri::command(rename_all = "snake_case")]
pub fn list_downloads(m: State<Manager>) -> Res<Vec<DownloadView>> {
    m.list().map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn get_stats(m: State<Manager>) -> Res<Stats> {
    m.stats().map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn add_downloads(m: State<Manager>, req: AddRequest) -> Res<Vec<i64>> {
    m.add(req).map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn pause(m: State<Manager>, id: i64) -> Res<()> {
    m.pause(id).map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn resume(app: AppHandle, m: State<Manager>, id: i64) -> Res<()> {
    m.resume(id).map_err(err)?;
    profile::leave_paused(&app);
    Ok(())
}


#[tauri::command(rename_all = "snake_case")]
pub fn redownload(m: State<Manager>, id: i64) -> Res<Vec<i64>> {
    m.redownload(id).map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn remove(m: State<Manager>, id: i64, delete_file: bool) -> Res<()> {
    m.remove(id, delete_file).map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn pause_all(m: State<Manager>) -> Res<()> {
    m.pause_all().map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn resume_all(app: AppHandle, m: State<Manager>) -> Res<()> {
    m.resume_all().map_err(err)?;
    profile::leave_paused(&app);
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub fn clear_completed(m: State<Manager>) -> Res<()> {
    m.clear_completed().map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn clear_all(m: State<Manager>) -> Res<()> {
    m.clear_all().map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn set_speed_limit(m: State<Manager>, id: i64, limit: u64) -> Res<()> {
    m.set_speed_limit(id, limit).map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn segments(m: State<Manager>, id: i64) -> Vec<SegmentView> {
    m.segments(id)
}

#[tauri::command(rename_all = "snake_case")]
pub fn get_settings(m: State<Manager>) -> Settings {
    m.settings()
}

#[tauri::command(rename_all = "snake_case")]
pub fn save_settings(app: AppHandle, m: State<Manager>, settings: Settings) -> Res<Settings> {
    let saved = m.update_settings(settings).map_err(err)?;
    let _ = app.emit("rdm-settings-changed", ());
    profile::changed(&app);
    Ok(saved)
}

#[tauri::command(rename_all = "snake_case")]
pub fn get_profile(app: AppHandle) -> Profile {
    profile::current(&app)
}

#[tauri::command(rename_all = "snake_case")]
pub fn set_profile(app: AppHandle, profile: Profile) -> Res<()> {
    profile::set(&app, profile).map_err(err)?;
    let _ = app.emit("rdm-settings-changed", ());
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub fn find_duplicates(m: State<Manager>, url: String, file_name: Option<String>, size: Option<u64>) -> Res<Vec<Duplicate>> {
    m.duplicates(&url, file_name.as_deref(), size).map_err(err)
}

/// Continues a paused or failed download from a new link to the same file.
#[tauri::command(rename_all = "snake_case")]
pub async fn relink(m: State<'_, Manager>, id: i64, url: String) -> Res<()> {
    m.relink(id, url.trim(), vec![]).await.map_err(err)
}

/// While set, the next download the browser sends refreshes download `id`'s
/// link instead of starting a new download. `None` stops waiting.
#[tauri::command(rename_all = "snake_case")]
pub fn await_link(capture: State<Arc<bridge::LinkCapture>>, id: Option<i64>) {
    capture.set(id);
}

#[tauri::command(rename_all = "snake_case")]
pub fn show_main_window(app: AppHandle) {
    crate::tray::show_main(&app);
}

fn download_path(m: &Manager, id: i64) -> Res<PathBuf> {
    let d = m.get(id).map_err(err)?.ok_or("download not found")?;
    let path = d.download.path.ok_or("the file hasn't been created yet")?;
    if !path.exists() {
        return Err(format!("{} no longer exists", path.display()));
    }
    Ok(path)
}

#[tauri::command(rename_all = "snake_case")]
pub fn open_download(m: State<Manager>, id: i64) -> Res<()> {
    os::open_path(&download_path(&m, id)?).map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn reveal_download(m: State<Manager>, id: i64) -> Res<()> {
    let d = m.get(id).map_err(err)?.ok_or("download not found")?;
    let path = d.download.path.ok_or("the file hasn't been created yet")?;
    // Unfinished downloads only have their .part file so far.
    let target = [path.clone(), PathBuf::from(format!("{}.part", path.display()))]
        .into_iter()
        .find(|p| p.exists());
    match target {
        Some(p) => os::reveal_path(&p),
        None => os::reveal_path(&d.download.dir),
    }
    .map_err(err)
}

#[tauri::command(rename_all = "snake_case")]
pub fn clipboard_text(app: AppHandle) -> Res<String> {
    // An empty or non-text clipboard isn't an error worth showing.
    Ok(app.clipboard().read_text().unwrap_or_default())
}

#[tauri::command(rename_all = "snake_case")]
pub fn copy_text(app: AppHandle, text: String) -> Res<()> {
    crate::clipboard::note_own_copy(&text);
    app.clipboard().write_text(text).map_err(err)
}

/// Native folder picker. `None` when cancelled.
#[tauri::command(rename_all = "snake_case")]
pub async fn choose_folder(app: AppHandle, start: Option<String>) -> Res<Option<String>> {
    pick_folder(app, start).await
}

/// Shows the system folder picker, opening at `start` if it exists.
pub async fn pick_folder(app: AppHandle, start: Option<String>) -> Res<Option<String>> {
    tokio::task::spawn_blocking(move || {
        let mut dialog = app.dialog().file().set_title("Save downloads to");
        if let Some(dir) = start.filter(|s| std::path::Path::new(s).is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        match dialog.blocking_pick_folder() {
            Some(path) => path.into_path().map(|p| Some(p.display().to_string())).map_err(err),
            None => Ok(None),
        }
    })
    .await
    .map_err(err)?
}

#[tauri::command(rename_all = "snake_case")]
pub fn get_autostart(app: AppHandle) -> Res<bool> {
    app.autolaunch().is_enabled().map_err(err)
}

/// Starts Flash hidden in the menu bar or tray at login.
#[tauri::command(rename_all = "snake_case")]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Res<()> {
    let launcher = app.autolaunch();
    if enabled { launcher.enable() } else { launcher.disable() }.map_err(err)
}

#[derive(Serialize)]
pub struct BridgeInfo {
    port: u16,
    error: Option<String>,
}

#[tauri::command(rename_all = "snake_case")]
pub fn bridge_status(status: State<Arc<bridge::Status>>) -> BridgeInfo {
    BridgeInfo { port: status.port(), error: status.error() }
}

#[derive(Serialize)]
pub struct ToolsInfo {
    ytdlp: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
}

/// Which optional helpers are installed (video sites, MP4 merging).
#[tauri::command(rename_all = "snake_case")]
pub fn tools_status() -> ToolsInfo {
    ToolsInfo { ytdlp: rdm_core_tool("yt-dlp"), ffmpeg: rdm_core_tool("ffmpeg") }
}

fn rdm_core_tool(name: &str) -> Option<PathBuf> {
    rdm_manager::find_tool(name)
}
