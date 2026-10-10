use futures::{StreamExt, stream};
use rmcp::schemars;
use serde::Deserialize;
use spider::reqwest::Client;

use crate::{
    chunk::{Chunk, chunk},
    extract::extract,
    fetcher,
    models::ModelSet,
    pack::{PageOut, pack, pack_passages},
    rank::{self, Models, RankConfig},
    tools::query_text,
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
    pub query: Option<String>,
    /// Exact terms or synonyms that should match, to sharpen the query.
    pub keywords: Option<Vec<String>>,
    /// Maximum tokens to return across all pages (default 2000).
    pub max_tokens: Option<usize>,
    /// Rescore passages with a slower, more precise model (~0.3 s more, fewer tokens). Off by default.
    pub precise: Option<bool>,
}

pub async fn run(client: &Client, models: &ModelSet, params: FetchParams) -> String {
    let pages: Vec<PageOut> = stream::iter(params.urls.clone())
        .map(|url| async move { fetch_one(client, &url).await })
        .buffered(CONCURRENCY)
        .collect()
        .await;
    let precise = params.precise.unwrap_or(false);
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

pub async fn fetch_one(client: &Client, url: &str) -> PageOut {
    let fetched = fetcher::fetch(client, url).await;
    if !(200..300).contains(&fetched.status) {
        return PageOut::error(fetched.url, format!("HTTP {}", fetched.status));
    }
    // Extraction is CPU-bound; keep it off the async worker threads.
    let url = fetched.url.clone();
    let extracted = tokio::task::spawn_blocking(move || extract(&fetched.url, &fetched.body)).await;
    page_out(url, extracted.ok())
}

/// Wrap an extraction result as a page, turning empty or failed ones into errors.
pub fn page_out(url: String, extracted: Option<crate::extract::Extracted>) -> PageOut {
    match extracted {
        Some(e) if !e.markdown.is_empty() => PageOut {
            url,
            title: e.title,
            body: e.markdown,
            ok: true,
            links: e.links,
            canonical: e.canonical,
        },
        Some(_) => PageOut::error(url, "no readable content (the page may need JavaScript)"),
        None => PageOut::error(url, "extraction failed"),
    }
}
