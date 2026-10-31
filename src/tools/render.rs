use futures::{StreamExt, stream};
use rmcp::schemars;
use serde::Deserialize;

use crate::{
    browser::Chrome,
    extract::extract,
    fetcher,
    models::ModelSet,
    pack::PageOut,
    tools::fetch::{self, FetchParams, page_out},
};

/// Tabs open at the same time within one call.
const TABS: usize = 4;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RenderParams {
    /// Pages to load in the browser. Pass several in one call rather than calling once per URL.
    pub urls: Vec<String>,
    /// What you are looking for. When set, only the most relevant passages are returned.
    pub query: Option<String>,
    /// Exact terms or synonyms that should match, to sharpen the query.
    pub keywords: Option<Vec<String>>,
    /// Maximum tokens to return across all pages (default 2000).
    pub max_tokens: Option<usize>,
}

pub async fn run(chrome: &Chrome, models: &ModelSet, params: RenderParams) -> String {
    let pages: Vec<PageOut> = stream::iter(params.urls.clone())
        .map(|url| async move { render_one(chrome, &url).await })
        .buffered(TABS)
        .collect()
        .await;
    // `..default()` fills `precise`, which exists with the `rerank` feature.
    #[allow(clippy::needless_update)]
    let params = FetchParams {
        urls: params.urls,
        query: params.query,
        keywords: params.keywords,
        max_tokens: params.max_tokens,
        ..FetchParams::default()
    };
    fetch::answer(pages, params, models).await
}

/// Render one page in Chrome and extract it like a fetched one.
async fn render_one(chrome: &Chrome, url: &str) -> PageOut {
    // Local files never reach Chrome: `fetch` reads local PDFs and nothing else.
    if fetcher::local_path(url).is_some() {
        return PageOut::error(url.to_string(), "render reads web pages; use `fetch` for local PDFs");
    }
    let url = fetcher::normalize(url);
    let rendered = match chrome.render(&url).await {
        Ok(r) => r,
        Err(e) => return PageOut::error(url, e),
    };
    let page_url = rendered.url.clone();
    let extracted =
        tokio::task::spawn_blocking(move || extract(&rendered.url, rendered.html.as_bytes()))
            .await
            .ok();
    match extracted {
        // A bot-check page is reported, not returned as the page's content.
        Some(e) if is_bot_check(e.title.as_deref().unwrap_or_default(), &e.markdown) => {
            PageOut::error(
                page_url,
                "blocked by a bot check (e.g. Cloudflare); web-shooter doesn't get past these",
            )
        }
        // Still empty once the scripts have run: usually a bot check or a login wall.
        Some(e) if e.markdown.is_empty() && e.error.is_none() => PageOut::error(
            page_url,
            "no readable content after rendering (a bot check or login page?)",
        ),
        e => page_out(page_url, e),
    }
}

/// A bot-check interstitial (Cloudflare and similar) instead of the page.
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
            ]
            .iter()
            .any(|p| text.contains(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_files_are_refused() {
        let chrome = Chrome::default();
        for url in ["file:///etc/passwd", "/Users/me/.ssh/config", "~/notes.txt"] {
            let page = render_one(&chrome, url).await;
            assert!(!page.ok && page.body.contains("use `fetch`"), "{url}: {}", page.body);
        }
        assert!(chrome.render("chrome://settings").await.is_err());
    }

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
        assert!(!is_bot_check(
            "Software Engineer",
            &"Build secure systems. ".repeat(200)
        ));
    }
}
