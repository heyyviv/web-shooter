use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use spider::reqwest::Client;

use crate::{
    browser::Chrome,
    fetcher,
    models::ModelSet,
    tools::{
        crawl::{self, CrawlParams},
        fetch::{self, FetchParams},
        links::{self, LinksParams},
        render::{self, RenderParams},
    },
};

const INSTRUCTIONS: &str = "Fast web reader and focused crawler. `fetch` reads pages and PDFs \
you already have URLs or local paths for (pass a query to get only the relevant passages, \
and batch URLs). \
`links` lists a page's links ranked by a query. `crawl` explores a site from a start page \
and returns the most relevant pages as URLs with snippets; then `fetch` the ones you need. \
`render` reads pages that build their content with JavaScript, in headless Chrome: use it when \
`fetch` reports no readable content, since it is slower.";

#[derive(Clone)]
pub struct WebShooter {
    #[allow(dead_code, reason = "read by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
    client: Client,
    models: ModelSet,
    chrome: Chrome,
}

#[tool_router]
impl WebShooter {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            tool_router: Self::tool_router(),
            client: fetcher::build_client()?,
            models: ModelSet::default(),
            chrome: Chrome::default(),
        })
    }

    /// Close Chrome, if `render` started it, and delete its profile.
    pub async fn shutdown(&self) {
        self.chrome.close().await;
    }

    /// Start loading ranking models in the background.
    pub fn preload(&self) {
        self.models.preload();
    }

    #[tool(
        description = "Read one or more web pages or PDFs (URLs, or local PDF paths) as clean markdown. With `query`, returns only the most relevant passages within `max_tokens`; PDF passages are labelled with their page number."
    )]
    async fn fetch(
        &self,
        Parameters(params): Parameters<FetchParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = fetch::run(&self.client, &self.models, params).await;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Read pages that build their content with JavaScript (single-page apps, job boards, search results) by loading them in headless Chrome, then return clean markdown like `fetch`. With `query`, returns only the most relevant passages within `max_tokens`. Slower than `fetch` (about 1-3 s a page): use it when `fetch` reports no readable content. Needs Chrome installed."
    )]
    async fn render(
        &self,
        Parameters(params): Parameters<RenderParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = render::run(&self.chrome, &self.models, params).await;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "List a page's links. With `query`, ranks them by relevance so you can pick which to fetch."
    )]
    async fn links(
        &self,
        Parameters(params): Parameters<LinksParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = links::run(&self.client, &self.models, params).await;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Crawl a site from a start page, following the links most relevant to `query` first. Returns the best pages as URLs with one-line snippets (not full text); `fetch` them for details. Respects robots.txt."
    )]
    async fn crawl(
        &self,
        Parameters(params): Parameters<CrawlParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = crawl::run(&self.client, &self.models, params).await;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for WebShooter {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "web-shooter",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}
