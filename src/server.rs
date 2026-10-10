use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use spider::reqwest::Client;

use crate::{
    fetcher,
    tools::fetch::{self, FetchParams},
};

const INSTRUCTIONS: &str = "Fast web reader. Pass a query to get only the relevant passages \
instead of whole pages, and batch several URLs into one call.";

#[derive(Clone)]
pub struct WebShooter {
    #[allow(dead_code, reason = "read by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
    client: Client,
}

#[tool_router]
impl WebShooter {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            tool_router: Self::tool_router(),
            client: fetcher::build_client()?,
        })
    }

    #[tool(
        description = "Read one or more web pages as clean markdown. With `query`, returns only the most relevant passages within `max_tokens`."
    )]
    async fn fetch(
        &self,
        Parameters(params): Parameters<FetchParams>,
    ) -> Result<CallToolResult, McpError> {
        let text = fetch::run(&self.client, params).await;
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
