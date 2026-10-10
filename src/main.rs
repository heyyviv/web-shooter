mod server;
mod tools;

use rmcp::{ServiceExt, transport::stdio};

use crate::server::WebShooter;

// stdout carries the MCP protocol, so diagnostics must go to stderr.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let service = WebShooter::new().serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
