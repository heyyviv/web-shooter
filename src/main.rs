use rmcp::{ServiceExt, transport::stdio};
use web_shooter::server::WebShooter;

// stdout carries the MCP protocol, so diagnostics must go to stderr.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = WebShooter::new()?;
    server.preload();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
