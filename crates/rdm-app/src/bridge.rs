//! A small HTTP server on 127.0.0.1 that the browser extension sends
//! downloads to. Every request must carry the pairing token, and requests
//! from web pages (rather than extensions) are refused.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use rdm_manager::{AddRequest, Kind, Manager};
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};

use crate::tray;

#[derive(Default)]
pub struct Status {
    port: AtomicU16,
    error: Mutex<Option<String>>,
}

impl Status {
    pub fn port(&self) -> u16 {
        self.port.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    fn fail(&self, msg: String) {
        eprintln!("rdm: browser bridge: {msg}");
        *self.error.lock().unwrap() = Some(msg);
    }
}

/// A download waiting for a fresh link from the browser (IDM's "refresh
/// download address"): the next download the extension sends goes to it.
#[derive(Default)]
pub struct LinkCapture(Mutex<Option<i64>>);

impl LinkCapture {
    pub fn set(&self, id: Option<i64>) {
        *self.0.lock().unwrap() = id;
    }

    fn waiting(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    fn take(&self) -> Option<i64> {
        self.0.lock().unwrap().take()
    }
}

#[derive(Clone)]
struct Ctx {
    app: AppHandle,
    manager: Manager,
    capture: Arc<LinkCapture>,
    port: u16,
}

pub fn start(app: AppHandle, manager: Manager, status: Arc<Status>, capture: Arc<LinkCapture>) {
    let port = manager.settings().bridge_port;
    status.port.store(port, Ordering::Relaxed);
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => return status.fail(format!("could not listen on port {port}: {e}")),
        };
        let router = Router::new()
            .route("/v1/ping", get(ping))
            .route("/v1/downloads", post(add))
            .route("/v1/choose-folder", post(choose_folder))
            .route("/v1/duplicates", post(duplicates))
            .with_state(Ctx { app, manager, capture, port });
        if let Err(e) = axum::serve(listener, router).await {
            status.fail(e.to_string());
        }
    });
}

type Reply = Result<Json<Value>, (StatusCode, String)>;

fn check(ctx: &Ctx, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let deny = |code, msg: &str| Err((code, msg.to_owned()));
    let get = |name| headers.get(name).and_then(|v: &header::HeaderValue| v.to_str().ok());

    // Guards against DNS rebinding: a web page can't make its own host
    // resolve here and still send this Host header.
    let host_ok = get(header::HOST)
        .is_some_and(|h| h == format!("127.0.0.1:{}", ctx.port) || h == format!("localhost:{}", ctx.port));
    if !host_ok {
        return deny(StatusCode::FORBIDDEN, "bad host");
    }
    // Pages send their own origin; extensions send theirs.
    if let Some(origin) = get(header::ORIGIN)
        && !(origin.starts_with("chrome-extension://") || origin.starts_with("moz-extension://"))
    {
        return deny(StatusCode::FORBIDDEN, "requests from web pages are not allowed");
    }
    let token = ctx.manager.settings().bridge_token;
    let given = get(header::HeaderName::from_static("x-rdm-token")).unwrap_or("");
    if !constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return deny(StatusCode::UNAUTHORIZED, "wrong pairing token");
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn ping(State(ctx): State<Ctx>, headers: HeaderMap) -> Reply {
    check(&ctx, &headers)?;
    let settings = ctx.manager.settings();
    Ok(Json(json!({
        "app": "rdm",
        "version": env!("CARGO_PKG_VERSION"),
        "download_dir": settings.download_dir,
        "use_categories": settings.use_categories,
        "home": rdm_manager::home_dir(),
        "video_quality": settings.video_quality,
        "has_ytdlp": rdm_manager::find_tool("yt-dlp").is_some(),
        // Waiting for a fresh link: send the next download straight away.
        "awaiting_link": ctx.capture.waiting(),
    })))
}

#[derive(Deserialize)]
struct ChooseFolder {
    start: Option<String>,
}

/// Opens the native folder picker for the extension's dialog.
async fn choose_folder(State(ctx): State<Ctx>, headers: HeaderMap, Json(body): Json<ChooseFolder>) -> Reply {
    check(&ctx, &headers)?;
    let path = crate::commands::pick_folder(ctx.app.clone(), body.start)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "path": path })))
}

#[derive(Deserialize)]
struct DuplicateQuery {
    url: String,
    file_name: Option<String>,
    size: Option<u64>,
}

/// What the user already has that a download would repeat.
async fn duplicates(State(ctx): State<Ctx>, headers: HeaderMap, Json(q): Json<DuplicateQuery>) -> Reply {
    check(&ctx, &headers)?;
    let found = ctx
        .manager
        .duplicates(&q.url, q.file_name.as_deref(), q.size)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "duplicates": found })))
}

#[derive(Deserialize)]
struct Incoming {
    url: String,
    file_name: Option<String>,
    referrer: Option<String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    kind: Option<Kind>,
    /// Folder chosen in the extension's dialog; `None` = Flash's default.
    dir: Option<String>,
    /// `false` adds it paused.
    start: Option<bool>,
    /// Video downloads: "best", "1080", "audio", ...
    quality: Option<String>,
    /// The user already confirmed in the browser, so skip Flash's dialog.
    #[serde(default)]
    confirmed: bool,
}

/// `~/Movies` -> `/Users/me/Movies` (or `~\\Videos` on Windows).
fn expand_home(dir: &str) -> PathBuf {
    match (dir.strip_prefix("~/").or_else(|| dir.strip_prefix("~\\")), rdm_manager::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(dir),
    }
}

async fn add(State(ctx): State<Ctx>, headers: HeaderMap, Json(body): Json<Incoming>) -> Reply {
    check(&ctx, &headers)?;
    let mut forwarded = Vec::new();
    for (name, value) in [("Cookie", body.cookies), ("Referer", body.referrer), ("User-Agent", body.user_agent)] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            forwarded.push((name.to_owned(), v));
        }
    }

    if let Some(id) = ctx.capture.take() {
        let result = ctx.manager.relink(id, &body.url, forwarded).await;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        tray::show_main(&ctx.app);
        let _ = ctx.app.emit("rdm-relinked", json!({ "id": id, "ok": error.is_none(), "error": error }));
        return match error {
            None => Ok(Json(json!({ "status": "relinked", "ids": [id] }))),
            Some(e) => Err((StatusCode::CONFLICT, e)),
        };
    }

    if !body.confirmed && ctx.manager.settings().confirm_browser_downloads {
        tray::show_main(&ctx.app);
        let _ = ctx.app.emit(
            "rdm-incoming",
            json!({ "urls": [body.url], "file_name": body.file_name, "headers": forwarded, "kind": body.kind, "quality": body.quality, "source": "browser" }),
        );
        return Ok(Json(json!({ "status": "confirm" })));
    }

    let ids = ctx
        .manager
        .add(AddRequest {
            urls: vec![body.url],
            dir: body.dir.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(expand_home),
            file_name: body.file_name,
            headers: forwarded,
            kind: body.kind,
            quality: body.quality,
            start: body.start.unwrap_or(true),
            ..Default::default()
        })
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    Ok(Json(json!({ "status": "added", "ids": ids })))
}
