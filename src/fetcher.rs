use std::time::Duration;

use spider::{page::Page, reqwest::Client};

/// Shared HTTP client: one connection pool for every tool call.
pub fn build_client() -> anyhow::Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("web-shooter/", env!("CARGO_PKG_VERSION")))
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

/// Fetch one page over plain HTTP. Accepts bare domains like `example.com`.
pub async fn fetch(client: &Client, url: &str) -> Fetched {
    let url = normalize(url);
    let page = Page::new_page(&url, client).await;
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

fn normalize(url: &str) -> String {
    let url = url.trim();
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("https://{url}")
    }
}
