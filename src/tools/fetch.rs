use futures::{StreamExt, stream};
use rmcp::schemars;
use serde::Deserialize;
use spider::reqwest::Client;

use crate::{
    browser::Chrome,
    chunk::{Chunk, MD_LINK, chunk},
    extract::extract,
    fetcher,
    models::ModelSet,
    pack::{PageOut, pack, pack_passages},
    pdf,
    rank::{self, Models, RankConfig},
    tools::{query_text, render},
};

/// Default token budget for one `fetch` call.
pub const DEFAULT_MAX_TOKENS: usize = 2000;

/// Pages fetched at the same time within one call.
const CONCURRENCY: usize = 16;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct FetchParams {
    /// Pages or PDFs to read (URLs, or absolute paths of local PDF files). Pass several in one call rather than calling once per URL.
    pub urls: Vec<String>,
    /// What you are looking for. When set, only the most relevant passages are returned.
    pub query: Option<String>,
    /// Exact terms or synonyms that should match, to sharpen the query.
    pub keywords: Option<Vec<String>>,
    /// Maximum tokens to return across all pages (default 2000).
    pub max_tokens: Option<usize>,
    #[cfg(feature = "rerank")]
    /// Rescore passages with a slower, more precise model (~0.3 s more, fewer tokens). Off by default.
    pub precise: Option<bool>,
}

pub async fn run(
    client: &Client,
    chrome: &Chrome,
    models: &ModelSet,
    params: FetchParams,
) -> String {
    let pages: Vec<PageOut> = stream::iter(params.urls.clone())
        .map(|url| async move { read(client, chrome, &url).await })
        .buffered(CONCURRENCY)
        .collect()
        .await;
    answer(pages, params, models).await
}

/// Rank and pack already-read pages into the tool's reply (`fetch` and `render`).
pub async fn answer(pages: Vec<PageOut>, params: FetchParams, models: &ModelSet) -> String {
    #[cfg(feature = "rerank")]
    let precise = params.precise.unwrap_or(false);
    #[cfg(not(feature = "rerank"))]
    let precise = false;
    if params.query.is_some() || params.keywords.is_some() {
        // Wait for the models so the first query isn't ranked without them.
        models.wait(precise).await;
    }
    // Ranking is CPU-bound; keep it off the async worker threads.
    let models = models.clone();
    tokio::task::spawn_blocking(move || {
        respond(
            &pages,
            &params,
            models.ready(precise),
            RankConfig::default(),
        )
    })
    .await
    .unwrap_or_else(|e| format!("error: ranking failed: {e}"))
}

/// Build the tool output from already-fetched pages: whole pages without a
/// query, ranked passages with one.
pub fn respond(
    pages: &[PageOut],
    params: &FetchParams,
    models: Models,
    config: RankConfig,
) -> String {
    let max_tokens = params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let query = query_text(params.query.as_deref(), params.keywords.as_deref());
    if query.is_empty() {
        return pack(pages, max_tokens);
    }

    let chunks: Vec<Chunk> = pages
        .iter()
        .enumerate()
        .filter(|(_, p)| p.ok)
        .flat_map(|(i, p)| chunk(i, &p.body))
        .collect();
    let ranked = rank::rank(&query, &chunks, models, config);
    pack_passages(
        pages,
        &chunks,
        &ranked.scores,
        ranked.min_relative,
        max_tokens,
    )
}

/// Download a page, and load it in Chrome instead when the download has no
/// content or only links: its scripts build the content after loading.
async fn read(client: &Client, chrome: &Chrome, url: &str) -> PageOut {
    let page = fetch_one(client, url).await;
    let needs_browser = if page.ok {
        is_thin(&page.body)
    } else {
        page.body == NO_CONTENT
    };
    if !needs_browser || fetcher::local_path(url).is_some() {
        return page;
    }
    let rendered = render::render_one(chrome, url).await;
    // Twice the text, so a small JSON or PDF isn't swapped for Chrome's view of it.
    let better = rendered.ok
        && (!page.ok || text_outside_links(&rendered.body) > 2 * text_outside_links(&page.body));
    if better {
        rendered
    } else if page.ok {
        page
    } else {
        PageOut::error(
            rendered.url,
            format!("{NO_CONTENT}, and in Chrome: {}", rendered.body),
        )
    }
}

/// Little text, nearly all of it links. Amazon's job search: 2,161 letters
/// and digits, 337 outside links. Hacker News (365 outside, 7,095 in all) is not thin.
fn is_thin(markdown: &str) -> bool {
    alnum(markdown) < 3000 && text_outside_links(markdown) < 500
}

fn text_outside_links(markdown: &str) -> usize {
    alnum(&MD_LINK.replace_all(markdown, ""))
}

fn alnum(s: &str) -> usize {
    s.chars().filter(|c| c.is_alphanumeric()).count()
}

pub async fn fetch_one(client: &Client, url: &str) -> PageOut {
    if let Some(path) = fetcher::local_path(url) {
        return read_local(path).await;
    }
    let fetched = fetcher::fetch(client, url).await;
    if fetched.too_large {
        let mb = fetcher::MAX_BYTES >> 20;
        return PageOut::error(fetched.url, format!("larger than {mb} MB, not downloaded"));
    }
    // AWS WAF (IMDb) answers a bot check with 202 and a challenge script.
    if fetched.status == 202 {
        return PageOut::error(fetched.url, BLOCKED);
    }
    if !(200..300).contains(&fetched.status) {
        return PageOut::error(fetched.url, format!("HTTP {}", fetched.status));
    }
    // Extraction is CPU-bound; keep it off the async worker threads.
    let url = fetched.url.clone();
    let extracted = tokio::task::spawn_blocking(move || extract(&fetched.url, &fetched.body)).await;
    page_out(url, extracted.ok())
}

/// Read a local PDF file. Other local files are refused: `fetch` reads the
/// web, and a PDF on disk is the one case where a local copy is the source.
async fn read_local(path: std::path::PathBuf) -> PageOut {
    let shown = path.display().to_string();
    let read = tokio::task::spawn_blocking(move || -> Result<_, String> {
        let size = std::fs::metadata(&path)
            .map_err(|e| format!("cannot read file: {e}"))?
            .len();
        if size > pdf::MAX_BYTES as u64 {
            return Err(format!("file is larger than {} MB", pdf::MAX_BYTES >> 20));
        }
        let body = std::fs::read(&path).map_err(|e| format!("cannot read file: {e}"))?;
        if !pdf::is_pdf(&body) {
            return Err("not a PDF; only local PDF files can be read".to_string());
        }
        Ok(extract(&path.display().to_string(), &body))
    })
    .await;
    match read {
        Ok(Ok(extracted)) => page_out(shown, Some(extracted)),
        Ok(Err(e)) => PageOut::error(shown, e),
        Err(_) => page_out(shown, None),
    }
}

/// Wrap an extraction result as a page, turning empty or failed ones into errors.
pub fn page_out(url: String, extracted: Option<crate::extract::Extracted>) -> PageOut {
    match extracted {
        // A bot check or login wall is reported, not returned as the page's content.
        Some(e) if is_bot_check(e.title.as_deref().unwrap_or_default(), &e.markdown) => {
            PageOut::error(url, BLOCKED)
        }
        Some(_) if is_login_page(&url) => PageOut::error(url, "redirected to a login page"),
        Some(e) if !e.markdown.is_empty() => PageOut {
            url,
            title: e.title,
            body: e.markdown,
            ok: true,
            links: e.links,
            canonical: e.canonical,
        },
        Some(e) if e.error.is_some() => PageOut::error(url, e.error.unwrap_or_default()),
        Some(_) => PageOut::error(url, NO_CONTENT),
        None => PageOut::error(url, "extraction failed"),
    }
}

const NO_CONTENT: &str = "no readable content";

pub const BLOCKED: &str = "blocked by a bot check; web-shooter doesn't get past these";

/// A bot-check interstitial (Cloudflare, Reddit, Google and similar) instead of the page.
fn is_bot_check(title: &str, markdown: &str) -> bool {
    let title = title.to_lowercase();
    let text = markdown.to_lowercase();
    title.starts_with("just a moment")
        || title.starts_with("attention required")
        || (markdown.len() < 2000
            && [
                "verify you are human",
                "performing security verification",
                "checking your browser",
                "prove your humanity",
                "unusual traffic from your computer network",
            ]
            .iter()
            .any(|p| text.contains(p)))
}

/// The page ended up on a sign-in form (after redirects), e.g. old.reddit's `/login/?dest=…`.
fn is_login_page(url: &str) -> bool {
    let path = spider::url::Url::parse(url)
        .map(|u| u.path().to_lowercase())
        .unwrap_or_default();
    ["/login", "/signin", "/sign-in", "/accounts/login"]
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_check_pages_are_recognised() {
        assert!(is_bot_check(
            "Just a moment...",
            "This website uses a security service."
        ));
        assert!(is_bot_check(
            "jobs",
            "Performing security verification\nRay ID: 1"
        ));
        assert!(is_bot_check(
            "Reddit - Prove your humanity",
            "# Prove your humanity\nComplete the challenge below."
        ));
        assert!(is_bot_check(
            "",
            "Our systems have detected unusual traffic from your computer network."
        ));
        assert!(!is_bot_check(
            "Software Engineer",
            &"Build secure systems. ".repeat(200)
        ));
    }

    #[test]
    fn thin_pages_are_mostly_links() {
        let link = "* [Software Engineer II](https://example.com/jobs/1)\n";
        assert!(is_thin(&link.repeat(40)));
        assert!(!is_thin(&"A real sentence about the job and its team. ".repeat(20)));
        // A long list of links (Hacker News) is the page's content, not a shell.
        assert!(!is_thin(&link.repeat(200)));
    }

    #[test]
    fn login_redirects_are_recognised() {
        assert!(is_login_page(
            "https://old.reddit.com/login/?reason=lor2&dest=https%3A%2F%2Fold.reddit.com%2Fr%2Fx"
        ));
        assert!(is_login_page("https://example.com/accounts/login"));
        assert!(!is_login_page("https://example.com/blog/login-tips"));
        assert!(!is_login_page("https://example.com/"));
    }

    #[test]
    fn pages_without_words_are_empty() {
        let read = |html: &str| {
            let url = "https://example.com/";
            page_out(
                url.into(),
                Some(crate::extract::extract(url, html.as_bytes())),
            )
        };
        for html in [
            "<html><head><title>Reddit</title></head><body>Reddit<script>var a=1</script></body></html>",
            "<html><head><title>Google Search</title></head><body><div>13, 8, 13</div><div>0, 1</div></body></html>",
        ] {
            let page = read(html);
            assert!(
                !page.ok && page.body.contains("no readable content"),
                "{}",
                page.body
            );
        }
        assert!(
            read("<html><head><title>Hi</title></head><body><p>Short page.</p></body></html>").ok
        );
    }
}
