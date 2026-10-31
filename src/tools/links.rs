use rmcp::schemars;
use serde::Deserialize;
use spider::reqwest::Client;

use crate::{
    links::{self, Link},
    models::ModelSet,
    rank,
    tools::{fetch::fetch_one, query_text},
};

/// Default number of links returned.
const DEFAULT_MAX_LINKS: usize = 30;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LinksParams {
    /// Page whose links to list.
    pub url: String,
    /// Rank links by relevance to this. Without it, links are listed in page order.
    pub query: Option<String>,
    /// Exact terms or synonyms that should match, to sharpen the query.
    pub keywords: Option<Vec<String>>,
    /// Maximum links to return (default 30).
    pub max_links: Option<usize>,
}

pub async fn run(client: &Client, models: &ModelSet, params: LinksParams) -> String {
    let page = fetch_one(client, &params.url).await;
    if !page.ok {
        return format!("{} · error: {}", page.url, page.body);
    }
    let max_links = params.max_links.unwrap_or(DEFAULT_MAX_LINKS);
    let query = query_text(params.query.as_deref(), params.keywords.as_deref());
    let site = links::site(&page.url).unwrap_or_default();
    let title = page.title.as_deref().unwrap_or("untitled");
    let total = page.links.len();

    let ranked: Vec<(Option<f32>, &Link)> = if query.is_empty() {
        page.links
            .iter()
            .map(|l| (None, l))
            .take(max_links)
            .collect()
    } else {
        models.wait(false).await;
        let texts: Vec<String> = page.links.iter().map(Link::scoring_text).collect();
        // Embedding is CPU work; let other tasks use this thread.
        let scores = tokio::task::block_in_place(|| {
            rank::absolute(&query, &texts, models.ready(false).embedder)
        });
        let mut scored: Vec<(Option<f32>, &Link)> =
            scores.into_iter().map(Some).zip(&page.links).collect();
        scored.sort_by(|a, b| b.0.unwrap_or(0.0).total_cmp(&a.0.unwrap_or(0.0)));
        scored.truncate(max_links);
        scored
    };

    let order = if query.is_empty() {
        "in page order"
    } else {
        "ranked by query"
    };
    let mut out = format!(
        "{} · {title} · {total} links, showing {} {order}\n",
        page.url,
        ranked.len()
    );
    for (score, link) in ranked {
        let text = match (link.text.as_str(), link.heading.as_str()) {
            ("", "") => "(no text)",
            ("", heading) => heading,
            (text, _) => text,
        };
        let score = score.map(|s| format!("{s:.2} · ")).unwrap_or_default();
        let external = if links::same_site(&site, &link.url) {
            ""
        } else {
            " (other site)"
        };
        out.push_str(&format!("{score}{text} · {}{external}\n", link.url));
    }
    out.trim_end().to_string()
}
