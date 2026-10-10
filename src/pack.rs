use crate::{
    chunk::{Chunk, outline},
    links::Link,
};

/// Rough token count: about 4 characters per token for English text.
pub fn estimate_tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

/// One fetched page: its markdown and links, or an error.
#[derive(Default)]
pub struct PageOut {
    pub url: String,
    pub title: Option<String>,
    /// Markdown body, or an error message when `ok` is false.
    pub body: String,
    pub ok: bool,
    pub links: Vec<Link>,
    /// The URL the page declares as its canonical one, if any.
    pub canonical: Option<String>,
}

impl PageOut {
    pub fn error(url: String, message: impl Into<String>) -> Self {
        Self {
            url,
            body: message.into(),
            ..Self::default()
        }
    }
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

/// Section names listed per page, so the caller can ask for a different one.
const OUTLINE_MAX: usize = 8;

/// Pack the best-scoring chunks into `max_tokens`, grouped by page.
/// Passages scoring below `min_relative` × the best score are left out.
///
/// Pages appear in order of their best passage; passages within a page are
/// in document order, with neighbours under the same headings merged.
pub fn pack_passages(
    pages: &[PageOut],
    chunks: &[Chunk],
    scores: &[f32],
    min_relative: f32,
    max_tokens: usize,
) -> String {
    let top = scores.iter().copied().fold(0.0_f32, f32::max);

    let page_header = |i: usize| -> String {
        let page = &pages[i];
        if !page.ok {
            return format!("[{}] {} · error: {}\n", i + 1, page.url, page.body);
        }
        let title = page.title.as_deref().unwrap_or("untitled");
        format!("[{}] {} · {title}\n", i + 1, page.url)
    };

    // Every page gets its header line, so the caller can see what matched and what didn't.
    let mut remaining = max_tokens.saturating_sub(
        (0..pages.len())
            .map(|i| estimate_tokens(&page_header(i)) + 4)
            .sum(),
    );

    let mut ranked: Vec<usize> = (0..chunks.len())
        .filter(|&i| top > 0.0 && scores[i] >= top * min_relative)
        .collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));

    // Greedy: take the best passages that still fit. Each may need its own header.
    let mut selected: Vec<usize> = Vec::new();
    for i in ranked {
        let cost = estimate_tokens(&chunks[i].text) + estimate_tokens(&chunks[i].heading_path) + 6;
        if cost <= remaining {
            remaining -= cost;
            selected.push(i);
        }
    }

    let mut by_page: Vec<Vec<usize>> = vec![Vec::new(); pages.len()];
    for &i in &selected {
        by_page[chunks[i].page].push(i);
    }
    let best = |p: usize| {
        by_page[p]
            .iter()
            .map(|&i| scores[i])
            .fold(0.0_f32, f32::max)
    };
    let mut page_order: Vec<usize> = (0..pages.len()).collect();
    page_order.sort_by(|&a, &b| best(b).total_cmp(&best(a)));

    let mut out = String::new();
    for p in page_order {
        out.push_str(&page_header(p));
        if !pages[p].ok {
            out.push('\n');
            continue;
        }
        let picked = &mut by_page[p];
        if picked.is_empty() {
            out.push_str("(no matching passages)\n\n");
            continue;
        }
        picked.sort_by_key(|&i| chunks[i].index);

        // Merge runs of neighbouring chunks under the same headings.
        let mut groups: Vec<(String, f32, String)> = Vec::new();
        let mut prev: Option<&Chunk> = None;
        for &i in picked.iter() {
            let c = &chunks[i];
            let relative = scores[i] / top;
            match (prev, groups.last_mut()) {
                (Some(pc), Some(g))
                    if pc.index + 1 == c.index && pc.heading_path == c.heading_path =>
                {
                    g.1 = g.1.max(relative);
                    g.2.push('\n');
                    g.2.push_str(&c.text);
                }
                _ => groups.push((c.heading_path.clone(), relative, c.text.clone())),
            }
            prev = Some(c);
        }
        for (heading, score, text) in groups {
            let heading = if heading.is_empty() {
                "(top)"
            } else {
                &heading
            };
            out.push_str(&format!("§ {heading} · {score:.2}\n{text}\n"));
        }

        // Point at other sections only when part of the page was left out.
        let page_chunks = chunks.iter().filter(|c| c.page == p).count();
        if picked.len() < page_chunks {
            let sections = outline(&pages[p].body, OUTLINE_MAX);
            let cost = estimate_tokens(&sections.join(" · ")) + 4;
            if !sections.is_empty() && cost <= remaining {
                remaining -= cost;
                out.push_str(&format!("sections: {}\n", sections.join(" · ")));
            }
        }
        out.push('\n');
    }
    if selected.is_empty() {
        out.push_str("No passages matched the query. Try other keywords, or omit query to read pages from the top.");
    }
    out.trim_end().to_string()
}

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
            body: body.into(),
            ok: true,
            ..PageOut::default()
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

    fn chunk(page: usize, index: usize, heading: &str, text: &str) -> Chunk {
        Chunk {
            page,
            index,
            heading_path: heading.into(),
            text: text.into(),
        }
    }

    #[test]
    fn passages_ordered_by_page_relevance_and_merged() {
        let pages = [page("## A\nx\n## B\ny"), page("## C\nz")];
        let chunks = [
            chunk(0, 0, "A", "alpha one"),
            chunk(0, 1, "A", "alpha two"),
            chunk(0, 2, "B", "beta"),
            chunk(1, 0, "C", "gamma"),
        ];
        let out = pack_passages(&pages, &chunks, &[0.5, 0.4, 0.0, 1.0], 0.4, 500);
        assert_eq!(
            out,
            "[2] https://example.com · untitled\n§ C · 1.00\ngamma\n\n\
             [1] https://example.com · untitled\n§ A · 0.50\nalpha one\nalpha two\nsections: A · B"
        );
    }

    #[test]
    fn passages_respect_budget_and_report_misses() {
        let pages = [page("body"), page("body")];
        let chunks = [chunk(0, 0, "", &"w ".repeat(400)), chunk(0, 1, "", "small")];
        let out = pack_passages(&pages, &chunks, &[1.0, 0.9], 0.4, 60);
        assert!(!out.contains("w w"), "big chunk should not fit: {out}");
        assert!(out.contains("§ (top) · 0.90\nsmall"));
        assert!(out.contains("[2] https://example.com · untitled\n(no matching passages)"));
        assert!(estimate_tokens(&out) <= 60);
    }

    #[test]
    fn no_match_says_so() {
        let out = pack_passages(&[page("x")], &[chunk(0, 0, "", "x")], &[0.0], 0.4, 100);
        assert!(out.ends_with("No passages matched the query. Try other keywords, or omit query to read pages from the top."));
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
