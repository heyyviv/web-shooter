use rmcp::schemars;
use serde::Deserialize;

/// Default token budget for one `fetch` call.
pub const DEFAULT_MAX_TOKENS: usize = 2000;

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
}

/// M0 stub: echoes the request so the MCP wiring can be tested end to end.
pub async fn run(params: FetchParams) -> String {
    let max_tokens = params.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let query = params.query.as_deref().unwrap_or("-");
    let keywords = params.keywords.unwrap_or_default().join(", ");
    let mut out = format!(
        "web-shooter fetch (stub) · query: {query} · keywords: {keywords} · max_tokens: {max_tokens}\n"
    );
    for (i, url) in params.urls.iter().enumerate() {
        out.push_str(&format!("[{}] {url} · not fetched yet\n", i + 1));
    }
    out
}
