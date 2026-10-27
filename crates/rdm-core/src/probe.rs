use anyhow::{Context, Result, bail};
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, HeaderMap, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use url::Url;

use crate::filename;

/// What the server told us about the file before downloading it.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    pub total: Option<u64>,
    /// True only when the server honours `Range` *and* reports the size.
    pub ranges: bool,
    pub file_name: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Asks for the first byte only. A `206` with a `Content-Range` total means
/// we can download in segments; a `200` means one plain stream.
pub(crate) async fn probe(client: &Client, url: &Url, headers: &HeaderMap) -> Result<Probe> {
    let resp = client
        .get(url.clone())
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?;

    let status = resp.status();
    let h = resp.headers();
    let header = |name| h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let mut probe = Probe {
        total: None,
        ranges: false,
        file_name: header(CONTENT_DISPOSITION)
            .as_deref()
            .and_then(filename::from_content_disposition),
        etag: header(ETAG),
        last_modified: header(LAST_MODIFIED),
    };

    match status {
        StatusCode::PARTIAL_CONTENT => {
            probe.total = header(CONTENT_RANGE).as_deref().and_then(content_range_total);
            probe.ranges = probe.total.is_some();
        }
        // Empty files can't satisfy "bytes=0-0"; a plain GET handles them.
        StatusCode::RANGE_NOT_SATISFIABLE => {}
        s if s.is_success() => probe.total = resp.content_length(),
        s => bail!("server responded with {s}"),
    }
    Ok(probe)
}

/// `bytes 0-0/12345` -> 12345 (`*` means unknown).
fn content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(content_range_total("bytes 0-0/*"), None);
        assert_eq!(content_range_total("garbage"), None);
    }
}
