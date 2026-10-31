//! Real-Debrid: turns a file host's link (Rapidgator and the like) into a
//! fast direct link, using the user's own account.

use std::collections::HashSet;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const API: &str = "https://api.real-debrid.com/rest/1.0";
const API_TIMEOUT: Duration = Duration::from_secs(60);

/// Known hosts until the live list has loaded.
const STARTING_HOSTS: &[&str] = &[
    "rapidgator.net", "rapidgator.asia", "rg.to", "nitroflare.com", "nitro.download", "1fichier.com",
    "turbobit.net", "uploadgig.com", "katfile.com", "ddownload.com", "ddl.to", "filefactory.com",
    "k2s.cc", "keep2share.cc", "mega.nz",
];

/// On Real-Debrid's list but not sent to it: its own download servers (their
/// links are already direct), and general sites whose links work as they are
/// and may be private to the user. Video sites go through yt-dlp.
const LEFT_ALONE: &[&str] = &[
    "real-debrid.com", "docs.google.com", "drive.google.com", "dropbox.com", "icloud.com", "scribd.com",
    "vk.com", "youtube.com", "vimeo.com", "dailymotion.com", "soundcloud.com", "rutube.ru", "redtube.com",
    "youporn.com", "di.fm", "sky.fm", "radiotunes.com", "jazzradio.com", "classicalradio.com",
];

/// A direct link for a file host's link.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Unlocked {
    pub link: String,
    pub file_name: Option<String>,
    /// How many connections the link allows, if it says.
    pub chunks: Option<usize>,
}

/// The account behind a token, for showing in Settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DebridAccount {
    pub username: String,
    /// "premium" or "free".
    #[serde(rename = "type")]
    pub kind: String,
    /// When premium ends.
    pub expiration: Option<String>,
}

/// Real-Debrid refused a request.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub code: Option<i64>,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Real-Debrid: {}", self.advice())
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// The host or this link isn't one Real-Debrid handles, so the link
    /// may as well be tried directly.
    pub fn not_for_debrid(&self) -> bool {
        matches!(self.code, Some(2 | 16))
    }

    /// Worth trying again later: a busy or unavailable host, or limits that reset.
    pub fn is_temporary(&self) -> bool {
        matches!(self.code, Some(-1 | 5 | 6 | 17 | 18 | 19 | 21 | 23 | 25 | 34 | 36))
    }

    fn advice(&self) -> String {
        match self.code {
            Some(8) => "the API token was rejected. Check it in Settings → Real-Debrid".to_owned(),
            Some(14) => "this account is locked".to_owned(),
            Some(20) => "this host needs a premium account".to_owned(),
            Some(24) => "the file is no longer available on the host".to_owned(),
            _ => self.message.clone(),
        }
    }
}

pub(crate) struct Debrid {
    base: RwLock<String>,
    hosts: RwLock<HashSet<String>>,
}

impl Debrid {
    pub fn new() -> Self {
        Self {
            base: RwLock::new(API.to_owned()),
            hosts: RwLock::new(STARTING_HOSTS.iter().map(|h| (*h).to_owned()).collect()),
        }
    }

    pub fn set_base(&self, base: &str) {
        *self.base.write().unwrap() = base.trim_end_matches('/').to_owned();
    }

    /// Whether a link is on a file host that should go through Real-Debrid.
    pub fn covers(&self, url: &str) -> bool {
        let Some(host) = url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)) else {
            return false;
        };
        let is_on = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
        !LEFT_ALONE.iter().any(|d| is_on(d)) && self.hosts.read().unwrap().iter().any(|d| is_on(d))
    }

    /// Loads the live list of hosts. Needs no token.
    pub async fn refresh_hosts(&self, client: &reqwest::Client) -> Result<()> {
        let url = format!("{}/hosts/domains", self.base.read().unwrap());
        let resp = tokio::time::timeout(API_TIMEOUT, client.get(url).send()).await.context("Real-Debrid didn't answer")??;
        let domains: Vec<String> = serde_json::from_str(&resp.error_for_status()?.text().await?)?;
        if !domains.is_empty() {
            *self.hosts.write().unwrap() = domains.into_iter().map(|d| d.to_ascii_lowercase()).collect();
        }
        Ok(())
    }

    /// Gets a direct link for a file host's link.
    pub async fn unlock(&self, client: &reqwest::Client, token: &str, link: &str) -> Result<Unlocked> {
        let url = format!("{}/unrestrict/link", self.base.read().unwrap());
        let body = url::form_urlencoded::Serializer::new(String::new()).append_pair("link", link).finish();
        let request = client
            .post(url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        let value = api_call(request).await?;
        // Links with several qualities come back as a list; files are single.
        let item = value.as_array().and_then(|a| a.first()).unwrap_or(&value);
        let direct = item["download"].as_str().filter(|d| !d.is_empty());
        Ok(Unlocked {
            link: direct.context("Real-Debrid returned no download link")?.to_owned(),
            file_name: item["filename"].as_str().filter(|n| !n.is_empty()).map(str::to_owned),
            chunks: item["chunks"].as_u64().filter(|&c| c > 0).map(|c| c as usize),
        })
    }

    /// The account a token belongs to.
    pub async fn account(&self, client: &reqwest::Client, token: &str) -> Result<DebridAccount> {
        let url = format!("{}/user", self.base.read().unwrap());
        Ok(serde_json::from_value(api_call(client.get(url).bearer_auth(token)).await?)?)
    }
}

/// Sends a request and reads the answer, turning Real-Debrid's own errors
/// into [`ApiError`]. Network failures stay as they are, to be retried.
async fn api_call(request: reqwest::RequestBuilder) -> Result<Value> {
    let resp = tokio::time::timeout(API_TIMEOUT, request.send()).await.context("Real-Debrid didn't answer")??;
    let status = resp.status();
    let text = resp.text().await?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    let code = body["error_code"].as_i64();
    let message = body["error"].as_str().map_or_else(|| format!("request failed ({status})"), str::to_owned);
    // A gateway error has no code of its own, but is temporary all the same.
    let code = code.or(status.is_server_error().then_some(25)).or((status.as_u16() == 429).then_some(34));
    Err(ApiError { code, message }.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_file_hosts_go_through_real_debrid() {
        let d = Debrid::new();
        assert!(d.covers("https://rapidgator.net/file/abc/movie.mp4.html"));
        assert!(d.covers("https://www.rapidgator.net/file/abc"));
        assert!(d.covers("https://rg.to/file/abc"));
        assert!(!d.covers("https://cdn.example.com/rapidgator.net/x.zip"));
        assert!(!d.covers("https://notrapidgator.net/file"));
        assert!(!d.covers("not a url"));

        // Even if the live list names them.
        d.hosts.write().unwrap().extend(["real-debrid.com".to_owned(), "drive.google.com".to_owned()]);
        assert!(!d.covers("https://mum2-4.download.real-debrid.com/d/ABC/file.mp4"), "its own links are direct already");
        assert!(!d.covers("https://drive.google.com/file/d/1"));
    }

    #[test]
    fn errors_say_what_to_do_about_them() {
        let e = |code| ApiError { code: Some(code), message: "x".into() };
        assert!(e(16).not_for_debrid());
        assert!(e(19).is_temporary() && e(21).is_temporary() && e(23).is_temporary());
        assert!(!e(8).is_temporary() && !e(24).is_temporary() && !e(8).not_for_debrid());
        assert!(e(8).to_string().contains("Settings"));
        assert!(e(24).to_string().contains("no longer available"));
    }
}
