use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    sync::LazyLock,
    time::{Duration, Instant},
};

use futures::{StreamExt, stream::FuturesUnordered};
use model2vec_rs::model::StaticModel;
use regex::Regex;
use rmcp::schemars;
use serde::Deserialize;
use spider::{packages::robotparser::parser::RobotFileParser, reqwest::Client, url::Url};

use crate::{
    chunk::{Chunk, MD_LINK, chunk, prose_share},
    links::{self, Link},
    models::ModelSet,
    pack::PageOut,
    rank::{self, Models, RankConfig},
    tools::{fetch::fetch_one, query_text},
};

const DEFAULT_MAX_PAGES: usize = 30;
const MAX_PAGES_LIMIT: usize = 200;
const DEFAULT_MAX_DEPTH: usize = 3;
const DEFAULT_MAX_RESULTS: usize = 10;

/// Pages fetched at once. Crawls mostly stay on one site, so this is also
/// the load put on that site.
const CONCURRENCY: usize = 4;

/// Wall-clock limit; the crawl returns what it has when it runs out.
const TIME_LIMIT: Duration = Duration::from_secs(30);

/// Stop after this many fetched pages in a row that don't make the current
/// top results: the frontier has stopped paying off.
const PATIENCE: usize = 12;

/// A link's priority: how well its own text matches, plus how relevant the
/// page it was found on is (relevant pages tend to link to relevant pages).
const LINK_WEIGHT: f32 = 0.7;

/// Results scoring below this fraction of the best page are left out.
const MIN_RELATIVE: f32 = 0.5;

/// Pages whose text is mostly link text are still crawled for their links
/// but not returned: they are link lists, indexes or utility pages. Measured
/// share of letters and digits outside links: Wikipedia "What links here"
/// 0.03, a blog index 0.07, articles and docs 0.3–1.0.
const THIN_PAGE_PROSE_SHARE: f32 = 0.15;

/// Characters of the best passage shown under each result.
const SNIPPET_CHARS: usize = 200;

/// The `](url)` left over when a link's text spans lines.
static LINK_TAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\]\([^)]*\)\[?").unwrap());

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CrawlParams {
    /// Page to start from.
    pub url: String,
    /// What you are looking for. Links are followed in order of relevance to it.
    pub query: String,
    /// Exact terms or synonyms that should match, to sharpen the query.
    pub keywords: Option<Vec<String>>,
    /// Stop after this many pages (default 30, max 200).
    pub max_pages: Option<usize>,
    /// Maximum link hops from the start page (default 3).
    pub max_depth: Option<usize>,
    /// Also follow links to other sites (default false).
    pub other_sites: Option<bool>,
    /// Maximum pages to return (default 10).
    pub max_results: Option<usize>,
}

/// A URL waiting in the frontier.
struct Candidate {
    priority: f32,
    depth: usize,
    url: String,
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}
impl Eq for Candidate {}
impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Candidate {
    /// Highest priority first; among equals, the shallower page.
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .total_cmp(&other.priority)
            .then_with(|| other.depth.cmp(&self.depth))
    }
}

#[derive(Default)]
struct Stats {
    fetched: usize,
    errors: usize,
    robots_blocked: usize,
    duplicates: usize,
}

pub async fn run(client: &Client, models: &ModelSet, params: CrawlParams) -> String {
    let started = Instant::now();
    let query = query_text(Some(&params.query), params.keywords.as_deref());
    let max_pages = params
        .max_pages
        .unwrap_or(DEFAULT_MAX_PAGES)
        .clamp(1, MAX_PAGES_LIMIT);
    let max_depth = params.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    let max_results = params.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
    let other_sites = params.other_sites.unwrap_or(false);

    let start = if params.url.contains("://") {
        params.url.clone()
    } else {
        format!("https://{}", params.url)
    };
    let Some(home) = links::site(&start) else {
        return format!("error: invalid URL {}", params.url);
    };

    models.wait(false).await;
    let embedder = models.ready(false).embedder;

    let mut frontier = BinaryHeap::from([Candidate {
        priority: 1.0,
        depth: 0,
        url: start.clone(),
    }]);
    let mut seen: HashSet<String> = HashSet::from([start.clone()]);
    // Some sites serve one page under several URLs without redirecting;
    // the canonical URL catches most, a content hash the rest.
    let mut seen_canonical: HashSet<String> = HashSet::new();
    let mut seen_content: HashSet<u64> = HashSet::new();
    let mut robots: HashMap<String, Box<RobotFileParser>> = HashMap::new();
    let mut in_flight = FuturesUnordered::new();
    let mut pages: Vec<PageOut> = Vec::new();
    let mut top: Vec<f32> = Vec::new();
    let mut since_better = 0;
    let mut stats = Stats::default();

    let stop = loop {
        // Keep CONCURRENCY fetches going, best candidates first.
        while in_flight.len() < CONCURRENCY && stats.fetched + in_flight.len() < max_pages {
            let Some(next) = frontier.pop() else { break };
            // The start page was asked for explicitly, like `fetch`; robots.txt
            // governs the pages the crawl discovers on its own.
            if next.depth > 0 && !allowed(client, &mut robots, &next.url).await {
                stats.robots_blocked += 1;
                continue;
            }
            in_flight.push(async move {
                let page = fetch_one(client, &next.url).await;
                (next, page)
            });
        }
        if in_flight.is_empty() {
            break if stats.fetched >= max_pages {
                "reached max_pages"
            } else {
                "no more links to follow"
            };
        }
        let remaining = TIME_LIMIT.saturating_sub(started.elapsed());
        let Ok(Some((candidate, page))) = tokio::time::timeout(remaining, in_flight.next()).await
        else {
            break "time limit";
        };

        stats.fetched += 1;
        seen.insert(page.url.clone());
        if !page.ok {
            stats.errors += 1;
            continue;
        }
        let canonical = page.canonical.clone().unwrap_or_else(|| page.url.clone());
        if !seen_canonical.insert(canonical) || !seen_content.insert(fingerprint(&page.body)) {
            stats.duplicates += 1;
            continue;
        }

        let relevance = page_relevance(&query, &page, embedder);
        if enters_top(&mut top, relevance, max_results) {
            since_better = 0;
        } else {
            since_better += 1;
        }

        if candidate.depth < max_depth {
            let new: Vec<&Link> = page
                .links
                .iter()
                .filter(|l| other_sites || links::same_site(&home, &l.url))
                .filter(|l| !seen.contains(&l.url))
                .collect();
            let texts: Vec<String> = new.iter().map(|l| l.scoring_text()).collect();
            let scores = rank::absolute(&query, &texts, embedder);
            for (link, score) in new.into_iter().zip(scores) {
                seen.insert(link.url.clone());
                frontier.push(Candidate {
                    priority: LINK_WEIGHT * score + (1.0 - LINK_WEIGHT) * relevance,
                    depth: candidate.depth + 1,
                    url: link.url.clone(),
                });
            }
        }
        pages.push(page);

        if since_better >= PATIENCE {
            break "no better pages found recently";
        }
    };

    let elapsed = started.elapsed();
    let mut out = format!(
        "crawled {} pages from {start} in {:.1} s",
        stats.fetched,
        elapsed.as_secs_f32()
    );
    let mut notes = Vec::new();
    if stats.errors > 0 {
        notes.push(format!("{} failed", stats.errors));
    }
    if stats.duplicates > 0 {
        notes.push(format!("{} duplicates", stats.duplicates));
    }
    if stats.robots_blocked > 0 {
        notes.push(format!(
            "{} links blocked by robots.txt",
            stats.robots_blocked
        ));
    }
    if !notes.is_empty() {
        out.push_str(&format!(" ({})", notes.join(", ")));
    }
    out.push_str(&format!(" · stopped: {stop}\n"));
    out.push_str(&results(&query, &pages, embedder, max_results));
    out.trim_end().to_string()
}

/// Check robots.txt for the URL's origin, reading it on first use.
async fn allowed(
    client: &Client,
    robots: &mut HashMap<String, Box<RobotFileParser>>,
    url: &str,
) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let origin = format!("{}/", parsed.origin().ascii_serialization());
    if !robots.contains_key(&origin) {
        let mut parser = RobotFileParser::new();
        parser.read(client, &origin).await;
        robots.insert(origin.clone(), parser);
    }
    robots[&origin].can_fetch(crate::fetcher::USER_AGENT, url)
}

fn fingerprint(body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    h.finish()
}

/// How relevant a page is overall: its best passage, on the absolute scale.
fn page_relevance(query: &str, page: &PageOut, embedder: Option<&StaticModel>) -> f32 {
    let texts: Vec<String> = chunk(0, &page.body)
        .iter()
        .map(rank::scoring_text)
        .collect();
    rank::absolute(query, &texts, embedder)
        .into_iter()
        .fold(0.0, f32::max)
}

/// Track the best `k` relevances; true if `relevance` made it in.
fn enters_top(top: &mut Vec<f32>, relevance: f32, k: usize) -> bool {
    if top.len() < k {
        top.push(relevance);
        return true;
    }
    let (min_i, &min) = top
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.total_cmp(b.1))
        .expect("k > 0");
    if relevance > min {
        top[min_i] = relevance;
        true
    } else {
        false
    }
}

/// Rank the crawled pages by their best passage and list the top ones,
/// each with a one-line snippet of that passage.
fn results(
    query: &str,
    pages: &[PageOut],
    embedder: Option<&StaticModel>,
    max_results: usize,
) -> String {
    let chunks: Vec<Chunk> = pages
        .iter()
        .enumerate()
        .flat_map(|(i, p)| chunk(i, &p.body))
        .collect();
    let models = Models {
        embedder,
        reranker: None,
    };
    let scores = rank::rank(query, &chunks, models, RankConfig::default()).scores;

    // Best chunk per page, skipping thin pages.
    let thin: Vec<bool> = pages
        .iter()
        .map(|p| prose_share(&p.body) < THIN_PAGE_PROSE_SHARE)
        .collect();
    let mut best: Vec<Option<(f32, &Chunk)>> = vec![None; pages.len()];
    for (c, &s) in chunks.iter().zip(&scores) {
        if !thin[c.page] && best[c.page].is_none_or(|(b, _)| s > b) {
            best[c.page] = Some((s, c));
        }
    }
    let top = best.iter().flatten().map(|(s, _)| *s).fold(0.0, f32::max);
    let mut ranked: Vec<(usize, f32, &Chunk)> = best
        .iter()
        .enumerate()
        .filter_map(|(i, b)| b.map(|(s, c)| (i, s, c)))
        .filter(|(_, s, _)| top > 0.0 && *s >= top * MIN_RELATIVE)
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    ranked.truncate(max_results);

    if ranked.is_empty() {
        return "No relevant pages found. Try other keywords, a different start page, or other_sites: true.".into();
    }
    let mut out = String::new();
    for (n, (i, score, chunk)) in ranked.into_iter().enumerate() {
        let page = &pages[i];
        let title = page.title.as_deref().unwrap_or("untitled");
        let url = page.canonical.as_deref().unwrap_or(&page.url);
        out.push_str(&format!(
            "[{}] {url} · {title} · {:.2}\n    {}\n",
            n + 1,
            score / top,
            snippet(chunk)
        ));
    }
    out
}

/// One line: the passage's headings and the start of its text, as plain
/// text (no links, emphasis or quote markers).
fn snippet(chunk: &Chunk) -> String {
    let text = MD_LINK.replace_all(&chunk.text, "$1");
    let text = LINK_TAIL.replace_all(&text, "");
    let text = text
        .lines()
        .map(|l| l.trim_start_matches(['>', ' ', '#']))
        .collect::<Vec<_>>()
        .join(" ")
        .replace("**", "")
        .replace(" > ", " ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut: String = text.chars().take(SNIPPET_CHARS).collect();
    if cut.len() < text.len() {
        cut.push('…');
    }
    if chunk.heading_path.is_empty() {
        cut
    } else {
        format!("{}: {cut}", chunk.heading_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontier_pops_highest_priority_then_shallowest() {
        let mut heap = BinaryHeap::new();
        for (priority, depth, url) in [(0.2, 1, "a"), (0.9, 2, "b"), (0.9, 1, "c")] {
            heap.push(Candidate {
                priority,
                depth,
                url: url.into(),
            });
        }
        let order: Vec<String> = std::iter::from_fn(|| heap.pop().map(|c| c.url)).collect();
        assert_eq!(order, vec!["c", "b", "a"]);
    }

    #[test]
    fn enters_top_keeps_best_k() {
        let mut top = Vec::new();
        assert!(enters_top(&mut top, 0.5, 2));
        assert!(enters_top(&mut top, 0.3, 2));
        assert!(!enters_top(&mut top, 0.2, 2));
        assert!(enters_top(&mut top, 0.4, 2));
        top.sort_by(f32::total_cmp);
        assert_eq!(top, vec![0.4, 0.5]);
    }

    #[test]
    fn snippet_strips_links_and_shortens() {
        let c = Chunk {
            page: 0,
            index: 0,
            heading_path: "Jobs".into(),
            text: format!(
                "[Rust engineer](https://x.com/1)\n> **> Note **> : x](https://y.z)[\n{}",
                "word ".repeat(100)
            ),
            pdf_page: None,
        };
        let s = snippet(&c);
        assert!(s.starts_with("Jobs: Rust engineer Note : x word"), "{s}");
        assert!(s.ends_with('…'));
    }
}
