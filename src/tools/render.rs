use crate::{
    browser::Chrome,
    extract::extract,
    fetcher,
    pack::PageOut,
    tools::fetch::{self, page_out},
};

/// Render one page in Chrome and extract it like a fetched one.
pub async fn render_one(chrome: &Chrome, url: &str) -> PageOut {
    // Local files never reach Chrome: `fetch` reads local PDFs and nothing else.
    if fetcher::local_path(url).is_some() {
        return PageOut::error(url.to_string(), "render reads web pages; use `fetch` for local PDFs");
    }
    let url = fetcher::normalize(url);
    let rendered = match chrome.render(&url).await {
        Ok(r) => r,
        Err(e) => return PageOut::error(url, e),
    };
    let page_url = rendered.url.clone();
    match rendered.status {
        Some(202) => return PageOut::error(page_url, fetch::BLOCKED),
        Some(s) if !(200..300).contains(&s) => {
            return PageOut::error(page_url, format!("HTTP {s}"));
        }
        _ => {}
    }
    let extracted =
        tokio::task::spawn_blocking(move || extract(&rendered.url, rendered.html.as_bytes()))
            .await
            .ok();
    match extracted {
        // Still empty once the scripts have run: usually a bot check or a login wall.
        Some(e) if e.markdown.is_empty() && e.error.is_none() => PageOut::error(
            page_url,
            "no readable content after rendering (a bot check or login page?)",
        ),
        e => page_out(page_url, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_files_are_refused() {
        let chrome = Chrome::default();
        for url in ["file:///etc/passwd", "/Users/me/.ssh/config", "~/notes.txt"] {
            let page = render_one(&chrome, url).await;
            assert!(!page.ok && page.body.contains("use `fetch`"), "{url}: {}", page.body);
        }
        assert!(chrome.render("chrome://settings").await.is_err());
    }
}
