//! Commands the UI calls through `invoke`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use rdm_manager::{AddRequest, DownloadView, Duplicate, Manager, SegmentView, Settings, Stats};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::bridge;
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
    let path = download_path(&m, id)?;
    Command::new("open").arg(path).status().map_err(err)?;
    Ok(())
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
        Some(p) => Command::new("open").arg("-R").arg(p).status(),
        None => Command::new("open").arg(d.download.dir).status(),
    }
    .map_err(err)?;
    Ok(())
}

#[tauri::command(rename_all = "snake_case")]
pub fn clipboard_text() -> Res<String> {
    let out = Command::new("pbpaste").output().map_err(err)?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[tauri::command(rename_all = "snake_case")]
pub fn copy_text(text: String) -> Res<()> {
    crate::clipboard::note_own_copy(&text);
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn().map_err(err)?;
    child.stdin.take().ok_or("no stdin")?.write_all(text.as_bytes()).map_err(err)?;
    child.wait().map_err(err)?;
    Ok(())
}

/// Native folder picker. `None` when cancelled.
#[tauri::command(rename_all = "snake_case")]
pub async fn choose_folder(start: Option<String>) -> Res<Option<String>> {
    pick_folder(start).await
}

/// Shows the macOS folder picker, opening at `start` if it exists.
/// The path goes in as an argument, never spliced into the script.
pub async fn pick_folder(start: Option<String>) -> Res<Option<String>> {
    tokio::task::spawn_blocking(move || {
        let start = start.filter(|s| std::path::Path::new(s).is_dir());
        let mut cmd = Command::new("osascript");
        match &start {
            Some(dir) => cmd.args([
                "-e",
                "on run argv",
                "-e",
                r#"POSIX path of (choose folder with prompt "Save downloads to" default location (POSIX file (item 1 of argv)))"#,
                "-e",
                "end run",
                dir,
            ]),
            None => cmd.args(["-e", r#"POSIX path of (choose folder with prompt "Save downloads to")"#]),
        };
        let out = cmd.output().map_err(err)?;
        let path = String::from_utf8_lossy(&out.stdout).trim().trim_end_matches('/').to_owned();
        Ok((out.status.success() && !path.is_empty()).then_some(path))
    })
    .await
    .map_err(err)?
}

const LAUNCH_AGENT: &str = "com.siddhant.rdm";

fn launch_agent_path() -> Res<PathBuf> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LAUNCH_AGENT}.plist")))
}

#[tauri::command(rename_all = "snake_case")]
pub fn get_autostart() -> Res<bool> {
    Ok(launch_agent_path()?.exists())
}

/// Starts RDM hidden in the menu bar at login, via a LaunchAgent.
#[tauri::command(rename_all = "snake_case")]
pub fn set_autostart(enabled: bool) -> Res<()> {
    let path = launch_agent_path()?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(err(e)),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe().map_err(err)?;
    let exe = exe.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LAUNCH_AGENT}</string>
    <key>ProgramArguments</key><array><string>{exe}</string><string>--hidden</string></array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
    );
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(err)?;
    }
    std::fs::write(&path, plist).map_err(err)
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
