/// Rough token count: about 4 characters per token for English text.
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// One page's result, ready to be packed into the tool output.
pub struct PageOut {
    pub url: String,
    pub title: Option<String>,
    /// Markdown body, or an error message when `ok` is false.
    pub body: String,
    pub ok: bool,
}

/// Fit all pages into `max_tokens`. Each page gets a fair share; budget a
/// short page doesn't use is passed on to the longer ones.
pub fn pack(pages: &[PageOut], max_tokens: usize) -> String {
    let headers: Vec<String> = pages
        .iter()
        .enumerate()
        .map(|(i, page)| {
            if page.ok {
                let title = page.title.as_deref().unwrap_or("untitled");
                let total = estimate_tokens(&page.body);
                format!("[{}] {} · {title} · ~{total} tokens\n", i + 1, page.url)
            } else {
                format!("[{}] {} · error: {}\n", i + 1, page.url, page.body)
            }
        })
        .collect();

    // Headers and possible truncation notes come out of the budget first.
    let overhead: usize = headers.iter().map(|h| estimate_tokens(h)).sum::<usize>()
        + pages.len() * estimate_tokens(TRUNCATION_NOTE);
    let mut remaining = max_tokens.saturating_sub(overhead);

    let mut order: Vec<usize> = (0..pages.len()).filter(|&i| pages[i].ok).collect();
    order.sort_by_key(|&i| estimate_tokens(&pages[i].body));
    let mut shares = vec![0; pages.len()];
    for (n, &i) in order.iter().enumerate() {
        let fair = remaining / (order.len() - n);
        shares[i] = estimate_tokens(&pages[i].body).min(fair);
        remaining -= shares[i];
    }

    let mut out = String::new();
    for (i, page) in pages.iter().enumerate() {
        out.push_str(&headers[i]);
        if page.ok {
            let body = truncate(&page.body, shares[i] * 4);
            out.push_str(body);
            if body.len() < page.body.len() {
                let left = estimate_tokens(&page.body) - estimate_tokens(body);
                out.push_str(&TRUNCATION_NOTE.replace("{left}", &left.to_string()));
            }
        }
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

const TRUNCATION_NOTE: &str =
    "\n… [truncated, ~{left} more tokens: pass a query or raise max_tokens]";

/// Cut to at most `max_bytes`, preferring a paragraph, then a line, boundary.
fn truncate(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let head = &s[..end];
    // Only back off to a boundary if it doesn't throw away more than a quarter.
    let floor = end * 3 / 4;
    match head.rfind("\n\n").or_else(|| head.rfind('\n')) {
        Some(cut) if cut >= floor => &s[..cut],
        _ => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(body: &str) -> PageOut {
        PageOut {
            url: "https://example.com".into(),
            title: None,
            body: body.into(),
            ok: true,
        }
    }

    #[test]
    fn short_page_budget_goes_to_long_page() {
        let pages = [page(&"a".repeat(40)), page(&"b".repeat(4000))];
        let out = pack(&pages, 200);
        assert!(out.contains(&"a".repeat(40)));
        // The long page gets everything left after headers and the short page.
        assert!(out.matches('b').count() > 80 * 4);
        assert!(estimate_tokens(&out) <= 200);
    }

    #[test]
    fn errors_use_no_body_budget() {
        let mut failed = page("HTTP 404");
        failed.ok = false;
        let out = pack(&[failed, page(&"b".repeat(4000))], 300);
        assert!(out.starts_with("[1] https://example.com · error: HTTP 404\n"));
        assert!(estimate_tokens(&out) <= 300);
    }

    #[test]
    fn truncate_prefers_paragraph_boundary() {
        let s = format!("{}\n\n{}", "x".repeat(90), "y".repeat(50));
        assert_eq!(truncate(&s, 100), "x".repeat(90));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "é".repeat(10);
        assert_eq!(truncate(&s, 5), "éé");
    }
}
