use percent_encoding::percent_decode_str;
use url::Url;

/// Longest name we produce, leaving room for the `.rdm.json` suffix within
/// the 255-byte limit of macOS and Windows.
const MAX_NAME_BYTES: usize = 200;

/// Extracts a filename from a `Content-Disposition` header value, preferring
/// the RFC 5987 `filename*=` form over plain `filename=`.
pub(crate) fn from_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';') {
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        let val = val.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                // charset'language'percent-encoded-name
                if let Some(encoded) = val.splitn(3, '\'').nth(2) {
                    let decoded = percent_decode_str(encoded).decode_utf8_lossy();
                    if !decoded.is_empty() {
                        return Some(decoded.into_owned());
                    }
                }
            }
            "filename" if !val.is_empty() => plain = Some(val.to_owned()),
            _ => {}
        }
    }
    plain
}

/// The last non-empty path segment of the URL, percent-decoded.
pub(crate) fn from_url(url: &Url) -> Option<String> {
    let last = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    Some(percent_decode_str(last).decode_utf8_lossy().into_owned())
}

/// Makes a server-supplied name safe to use as a single path component, on
/// macOS and Windows alike.
pub(crate) fn sanitize(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Leading dots would make the file hidden (or be ".."); Windows drops
    // trailing dots and spaces.
    let trimmed = replaced.trim().trim_start_matches('.').trim_end_matches(['.', ' ']).trim();

    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        return "download".into();
    }
    // Windows reserves device names, with or without an extension.
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved { format!("_{out}") } else { out }
}

/// `report.pdf` -> `report (2).pdf`
pub(crate) fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_forms() {
        assert_eq!(
            from_content_disposition(r#"attachment; filename="a b.zip""#).as_deref(),
            Some("a b.zip")
        );
        assert_eq!(
            from_content_disposition("attachment; filename=plain.iso").as_deref(),
            Some("plain.iso")
        );
        assert_eq!(
            from_content_disposition(
                r#"attachment; filename="fallback.txt"; filename*=UTF-8''na%C3%AFve%20file.txt"#
            )
            .as_deref(),
            Some("naïve file.txt")
        );
        assert_eq!(from_content_disposition("inline"), None);
    }

    #[test]
    fn url_names() {
        let url = Url::parse("https://x.com/dl/My%20File.dmg?token=1").unwrap();
        assert_eq!(from_url(&url).as_deref(), Some("My File.dmg"));
        let url = Url::parse("https://x.com/").unwrap();
        assert_eq!(from_url(&url), None);
    }

    #[test]
    fn sanitizing() {
        assert_eq!(sanitize("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize("a/b:c"), "a_b_c");
        assert_eq!(sanitize("  "), "download");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize(r#"What? "Live" <4K>|*.mp4"#), "What_ _Live_ _4K___.mp4");
        assert_eq!(sanitize("notes. . "), "notes");
        assert_eq!(sanitize("con.txt"), "_con.txt");
        assert_eq!(sanitize("COM1"), "_COM1");
        assert_eq!(sanitize("console.log"), "console.log");
        assert!(sanitize(&"é".repeat(300)).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn numbering() {
        assert_eq!(numbered("report.pdf", 2), "report (2).pdf");
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered(".env", 1), ".env (1)");
    }
}
