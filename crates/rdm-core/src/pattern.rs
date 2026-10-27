use anyhow::{Result, bail};

/// Most URLs one pattern may expand to.
pub const MAX_EXPANSION: usize = 10_000;

/// Expands batch patterns like `img[001-120].jpg` or `part-[a-f].zip`.
/// Several brackets multiply out. A URL without brackets returns itself.
/// Zero-padding follows the start value's width.
pub fn expand(url: &str) -> Result<Vec<String>> {
    let Some(open) = url.find('[') else {
        return Ok(vec![url.to_owned()]);
    };
    let Some(close) = url[open..].find(']').map(|i| open + i) else {
        return Ok(vec![url.to_owned()]);
    };
    let Some(values) = range_values(&url[open + 1..close]) else {
        // Not a range (e.g. an IPv6 host); leave this bracket alone.
        let rest = expand(&url[close + 1..])?;
        return Ok(rest.into_iter().map(|r| format!("{}{r}", &url[..=close])).collect());
    };

    let rest = expand(&url[close + 1..])?;
    if values.len().saturating_mul(rest.len()) > MAX_EXPANSION {
        bail!("pattern expands to more than {MAX_EXPANSION} URLs");
    }
    let prefix = &url[..open];
    Ok(values
        .iter()
        .flat_map(|v| rest.iter().map(move |r| format!("{prefix}{v}{r}")))
        .collect())
}

fn range_values(spec: &str) -> Option<Vec<String>> {
    let (a, b) = spec.split_once('-')?;
    if let (Ok(start), Ok(end)) = (a.parse::<u64>(), b.parse::<u64>()) {
        if start > end || end - start >= MAX_EXPANSION as u64 {
            return None;
        }
        let width = a.len();
        return Some((start..=end).map(|n| format!("{n:0width$}")).collect());
    }
    let (mut ac, mut bc) = (a.chars(), b.chars());
    match (ac.next(), ac.next(), bc.next(), bc.next()) {
        (Some(s), None, Some(e), None) if s.is_ascii_alphabetic() && e.is_ascii_alphabetic() && s <= e => {
            Some((s..=e).map(String::from).collect())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_url_is_unchanged() {
        assert_eq!(expand("https://a.com/x.zip").unwrap(), vec!["https://a.com/x.zip"]);
    }

    #[test]
    fn numeric_with_padding() {
        let v = expand("https://a.com/img[008-011].jpg").unwrap();
        assert_eq!(v, vec![
            "https://a.com/img008.jpg",
            "https://a.com/img009.jpg",
            "https://a.com/img010.jpg",
            "https://a.com/img011.jpg"
        ]);
    }

    #[test]
    fn letters_and_multiple_brackets() {
        let v = expand("https://a.com/[a-b]/[1-2].bin").unwrap();
        assert_eq!(v, vec![
            "https://a.com/a/1.bin",
            "https://a.com/a/2.bin",
            "https://a.com/b/1.bin",
            "https://a.com/b/2.bin"
        ]);
    }

    #[test]
    fn ipv6_and_non_ranges_survive() {
        assert_eq!(expand("http://[::1]:8080/f[1-2]").unwrap(), vec![
            "http://[::1]:8080/f1",
            "http://[::1]:8080/f2"
        ]);
        assert_eq!(expand("https://a.com/[x]").unwrap(), vec!["https://a.com/[x]"]);
    }

    #[test]
    fn refuses_huge_expansions() {
        assert!(expand("https://a.com/[0-9999]/[0-9]").is_err());
    }
}
