use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use spider::{
    page::Page,
    reqwest::{
        Client,
        header::{ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue},
    },
    url::Url,
};

/// Sent with every request and matched against robots.txt rules.
pub const USER_AGENT: &str = concat!("web-shooter/", env!("CARGO_PKG_VERSION"));

/// Shared HTTP client: one connection pool for every tool call.
///
/// Sends the `Accept` headers a browser would: some sites (e.g. uber.com)
/// answer requests without them with HTTP 406.
pub fn build_client() -> anyhow::Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    Ok(Client::builder()
        .user_agent(USER_AGENT)
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .build()?)
}

/// A fetched page before extraction.
pub struct Fetched {
    /// URL after redirects.
    pub url: String,
    pub status: u16,
    pub body: Vec<u8>,
}

/// Wait before retrying a server error.
const RETRY_DELAY: Duration = Duration::from_millis(500);

/// Fetch one page over plain HTTP. Accepts bare domains like `example.com`.
/// A server error (5xx, including timeouts reported as 524) is retried once:
/// they are often transient.
pub async fn fetch(client: &Client, url: &str) -> Fetched {
    let url = normalize(url);
    let mut page = Page::new_page(&url, client).await;
    if page.status_code.is_server_error() {
        tokio::time::sleep(RETRY_DELAY).await;
        page = Page::new_page(&url, client).await;
    }
    let final_url = match page.get_url_final() {
        "" => url,
        u => u.to_string(),
    };
    Fetched {
        url: final_url,
        status: page.status_code.as_u16(),
        body: page.get_html_bytes_u8().to_vec(),
    }
}

/// The file a local path or `file://` URL names: `/abs/path`, `~/path`, or
/// a Windows path like `C:\docs\a.pdf`. `None` for web URLs and bare domains.
pub fn local_path(url: &str) -> Option<PathBuf> {
    let url = url.trim();
    if url.starts_with("file://") {
        return Url::parse(url).ok()?.to_file_path().ok();
    }
    if let Some(rest) = url.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest));
    }
    let path = Path::new(url);
    (url.starts_with('/') || path.is_absolute()).then(|| path.to_path_buf())
}

fn normalize(url: &str) -> String {
    let url = url.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}
