use rmcp::{ServiceExt, transport::stdio};
use web_shooter::{fetcher, server::WebShooter};

// stdout carries the MCP protocol, so diagnostics must go to stderr.
fn main() -> anyhow::Result<()> {
    // SAFETY: nothing else is running yet; the runtime starts below.
    unsafe { fetcher::limit_download_size() };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(serve());
    // Don't wait for the thread blocked reading stdin: after a signal, no
    // more input comes and the process would never exit.
    runtime.shutdown_background();
    result
}

async fn serve() -> anyhow::Result<()> {
    let server = WebShooter::new()?;
    server.preload();
    let service = server.clone().serve(stdio()).await?;
    // Stop on the client closing the connection or on a signal; either way,
    // don't leave Chrome or its temporary profile behind.
    let result = tokio::select! {
        r = service.waiting() => r.map(drop).map_err(anyhow::Error::from),
        _ = stop_signal() => Ok(()),
    };
    server.shutdown().await;
    result
}

/// SIGTERM, SIGINT or SIGHUP (Ctrl-C elsewhere).
async fn stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) =
            (signal(SignalKind::terminate()), signal(SignalKind::hangup()))
        else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hup.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
