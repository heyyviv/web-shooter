use futures::{StreamExt, stream};
use rmcp::schemars;
use serde::Deserialize;
use spider::reqwest::Client;

use crate::{
    extract::extract,
    fetcher,
    pack::{PageOut, pack},
};

/// Default token budget for one `fetch` call.
pub const DEFAULT_MAX_TOKENS: usize = 2000;

/// Pages fetched at the same time within one call.
const CONCURRENCY: usize = 16;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FetchParams {
    /// Pages to read. Pass several URLs in one call rather than calling once per URL.
    pub urls: Vec<String>,
    /// What you are looking for. When set, only the most relevant passages are returned.
    #[allow(dead_code, reason = "used for ranking from M2")]
    pub query: Option<String>,
    /// Exact terms or synonyms that should match, to sharpen the query.
    #[allow(dead_code, reason = "used for ranking from M2")]
    pub keywords: Option<Vec<String>>,
    /// Maximum tokens to return across all pages (default 2000).
    pub max_tokens: Option<usize>,
}

pub async fn run(client: &Client, params: FetchParams) -> String {
    let max_tokens = params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let pages: Vec<PageOut> = stream::iter(params.urls)
        .map(|url| fetch_one(client, url))
        .buffered(CONCURRENCY)
        .collect()
        .await;

    pack(&pages, max_tokens)
}

async fn fetch_one(client: &Client, url: String) -> PageOut {
    let fetched = fetcher::fetch(client, &url).await;
    if !(200..300).contains(&fetched.status) {
        return PageOut {
            url: fetched.url,
            title: None,
            body: format!("HTTP {}", fetched.status),
            ok: false,
        };
    }
    // Extraction is CPU-bound; keep it off the async worker threads.
    let url = fetched.url.clone();
    let extracted = tokio::task::spawn_blocking(move || extract(&fetched.url, &fetched.body)).await;
    match extracted {
        Ok(e) if !e.markdown.is_empty() => PageOut {
            url,
            title: e.title,
            body: e.markdown,
            ok: true,
        },
        Ok(_) => PageOut {
            url,
            title: None,
            body: "no readable content (the page may need JavaScript)".into(),
            ok: false,
        },
        Err(_) => PageOut {
            url,
            title: None,
            body: "extraction failed".into(),
            ok: false,
        },
    }
}
