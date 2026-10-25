use rmcp::{ServiceExt, transport::stdio};
use web_shooter::{fetcher, server::WebShooter};

// stdout carries the MCP protocol, so diagnostics must go to stderr.
fn main() -> anyhow::Result<()> {
    // SAFETY: nothing else is running yet; the runtime starts below.
    unsafe { fetcher::limit_download_size() };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve())
}

async fn serve() -> anyhow::Result<()> {
    let server = WebShooter::new()?;
    server.preload();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
