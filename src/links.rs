use std::collections::HashMap;

use spider::url::Url;

/// A link found on a page.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// Absolute URL without the `#fragment`.
    pub url: String,
    /// Anchor text, whitespace-collapsed.
    pub text: String,
    /// The heading the link appears under, if any.
    pub heading: String,
}

impl Link {
    /// The words a link is scored on: anchor text, heading, and URL path words.
    pub fn scoring_text(&self) -> String {
        let path = Url::parse(&self.url)
            .map(|u| u.path().replace(['/', '-', '_', '.'], " "))
            .unwrap_or_default();
        format!("{} {} {}", self.text, self.heading, path)
    }
}

/// File types that are never pages worth reading as text.
const SKIP_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "css", "js", "mjs", "json", "xml",
    "rss", "atom", "zip", "gz", "tar", "rar", "7z", "exe", "dmg", "pkg", "msi", "mp3", "mp4",
    "mov", "avi", "webm", "wav", "woff", "woff2", "ttf", "otf", "eot",
];

/// Resolve raw `(href, text, heading)` triples against the page URL, keep only
/// http(s) pages, drop fragments, and merge duplicates (keeping the longest text).
pub fn resolve(base: Option<&Url>, raw: Vec<(String, String, String)>) -> Vec<Link> {
    let mut out: Vec<Link> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (href, text, heading) in raw {
        let Some(url) = normalize(base, &href) else {
            continue;
        };
        let text = collapse_ws(&text);
        match seen.get(&url) {
            Some(&i) => {
                if text.len() > out[i].text.len() {
                    out[i].text = text;
                }
            }
            None => {
                seen.insert(url.clone(), out.len());
                out.push(Link {
                    url,
                    text,
                    heading: collapse_ws(&heading),
                });
            }
        }
    }
    out
}

/// Absolute http(s) URL without fragment, or `None` for anything that isn't a
/// readable page (mailto:, javascript:, images, archives, …).
pub fn normalize(base: Option<&Url>, href: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return None;
    }
    let mut url = match base {
        Some(base) => base.join(href).ok()?,
        None => Url::parse(href).ok()?,
    };
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    let ext = url
        .path()
        .rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase());
    if ext.is_some_and(|e| SKIP_EXTENSIONS.contains(&e.as_str())) {
        return None;
    }
    Some(url.to_string())
}

/// Host without a leading `www.`, for same-site checks.
pub fn site(url: &str) -> Option<String> {
    let host = Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// True if `url` is on `site` or one of its subdomains.
pub fn same_site(site_host: &str, url: &str) -> bool {
    self::site(url).is_some_and(|h| h == site_host || h.ends_with(&format!(".{site_host}")))
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_filters_and_dedupes() {
        let base = Url::parse("https://example.com/blog/post").unwrap();
        let raw = vec![
            ("/about#team".into(), "About".into(), "".into()),
            ("other".into(), " Other  post ".into(), "Related".into()),
            ("mailto:x@y.z".into(), "Mail".into(), "".into()),
            ("logo.PNG".into(), "".into(), "".into()),
            ("#top".into(), "Top".into(), "".into()),
            (
                "https://example.com/about".into(),
                "About our team".into(),
                "".into(),
            ),
        ];
        let links = resolve(Some(&base), raw);
        assert_eq!(
            links,
            vec![
                Link {
                    url: "https://example.com/about".into(),
                    text: "About our team".into(),
                    heading: "".into(),
                },
                Link {
                    url: "https://example.com/blog/other".into(),
                    text: "Other post".into(),
                    heading: "Related".into(),
                },
            ]
        );
    }

    #[test]
    fn same_site_ignores_www_and_allows_subdomains() {
        let s = site("https://www.example.com/").unwrap();
        assert!(same_site(&s, "https://example.com/a"));
        assert!(same_site(&s, "https://docs.example.com/a"));
        assert!(!same_site(&s, "https://notexample.com/a"));
    }

    #[test]
    fn scoring_text_includes_path_words() {
        let l = Link {
            url: "https://x.com/jobs/rust-engineer".into(),
            text: "Apply".into(),
            heading: "Engineering".into(),
        };
        assert_eq!(l.scoring_text(), "Apply Engineering  jobs rust engineer");
    }
}
