//! Live benchmark against other readers: `cargo run --release --example bench`.
//!
//! For every case in `eval/cases.json`, measures latency, tokens returned and
//! whether the expected answer text is present, for:
//! - web-shooter `fetch` with the case's query (live, not snapshots)
//! - Firecrawl `/v2/scrape` markdown, cache allowed and fresh (`maxAge: 0`),
//!   if `FIRECRAWL_API_KEY` is set (each URL costs one credit per mode)
//! - Jina Reader (`r.jina.ai`), no key needed
//!
//! Firecrawl and Jina return whole pages and take no query, so their output
//! is the full page markdown. Each distinct URL is fetched once per tool,
//! one request at a time (both rate-limit free plans); a multi-URL case's
//! latency is its slowest URL, as if fetched in parallel. web-shooter gets a
//! new HTTP client per case so it never benefits from warm connections.

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use spider::reqwest::Client;
use web_shooter::{
    fetcher,
    models::ModelSet,
    pack::estimate_tokens,
    tools::fetch::{self, FetchParams},
};

/// Gap between Firecrawl requests, to stay under the free plan's rate limit.
const FIRECRAWL_PACING: Duration = Duration::from_millis(6500);

#[derive(Deserialize)]
struct Case {
    name: String,
    urls: Vec<String>,
    query: String,
    keywords: Option<Vec<String>>,
    expect: Vec<String>,
}

/// One tool's result for one case.
#[derive(Clone, Copy, Default)]
struct Run {
    ms: f64,
    tokens: usize,
    found: bool,
    failed: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval");
    let cases: Vec<Case> = serde_json::from_str(&fs::read_to_string(root.join("cases.json"))?)?;
    let external = Client::builder().timeout(Duration::from_secs(60)).build()?;
    let firecrawl_key = std::env::var("FIRECRAWL_API_KEY").ok();

    let models = ModelSet::default();
    models.wait(false).await;

    // Whole-page tools: fetch each distinct URL once per tool.
    let mut urls: Vec<&String> = cases.iter().flat_map(|c| &c.urls).collect();
    urls.sort();
    urls.dedup();
    let mut tools: Vec<(&str, HashMap<String, (f64, Option<String>)>)> = Vec::new();
    if let Some(key) = &firecrawl_key {
        for (name, fresh) in [("firecrawl", false), ("firecrawl-fresh", true)] {
            let mut results = HashMap::new();
            for u in &urls {
                let t = Instant::now();
                let md = firecrawl(&external, key, u, fresh).await;
                results.insert((*u).clone(), (ms(t), md));
                // The free plan allows about 10 requests a minute.
                tokio::time::sleep(FIRECRAWL_PACING).await;
            }
            tools.push((name, results));
        }
    } else {
        eprintln!("FIRECRAWL_API_KEY not set: skipping Firecrawl");
    }
    let mut jina_results = HashMap::new();
    for u in &urls {
        // Sequential: the keyless Jina endpoint is rate-limited.
        let t = Instant::now();
        let md = jina(&external, u).await;
        jina_results.insert((*u).clone(), (ms(t), md));
    }
    tools.push(("jina", jina_results));

    let mut names = vec!["web-shooter"];
    names.extend(tools.iter().map(|(n, _)| *n));
    let mut runs: Vec<Vec<Run>> = Vec::new();
    for case in &cases {
        let params = FetchParams {
            urls: case.urls.clone(),
            query: Some(case.query.clone()),
            keywords: case.keywords.clone(),
            max_tokens: None,
            precise: None,
        };
        let client = fetcher::build_client()?;
        let t = Instant::now();
        let out = fetch::run(&client, &models, params).await;
        let mut row = vec![Run {
            ms: ms(t),
            tokens: estimate_tokens(&out),
            found: case.expect.iter().all(|e| out.contains(e.as_str())),
            failed: false,
        }];
        for (_, results) in &tools {
            row.push(whole_page_run(case, results));
        }
        runs.push(row);
    }

    print!("{:<24}", "case");
    for n in &names {
        print!(" | {n:>22}");
    }
    println!("\n{:<24}{}", "", " |     ms  tokens answer".repeat(names.len()));
    for (case, row) in cases.iter().zip(&runs) {
        print!("{:<24}", case.name);
        for r in row {
            if r.failed {
                print!(" | {:>22}", "failed");
            } else {
                let answer = if r.found { "yes" } else { "NO" };
                print!(" | {:>6.0} {:>7} {:>6}", r.ms, r.tokens, answer);
            }
        }
        println!();
    }
    println!();
    for (i, name) in names.iter().enumerate() {
        let col: Vec<Run> = runs.iter().map(|r| r[i]).collect();
        let ok: Vec<&Run> = col.iter().filter(|r| !r.failed).collect();
        let mut lat: Vec<f64> = ok.iter().map(|r| r.ms).collect();
        lat.sort_by(f64::total_cmp);
        let median = lat.get(lat.len() / 2).copied().unwrap_or(0.0);
        let mean_tokens = ok.iter().map(|r| r.tokens).sum::<usize>() / ok.len().max(1);
        let found = ok.iter().filter(|r| r.found).count();
        println!(
            "{name:<16} median {median:>6.0} ms · avg {mean_tokens:>6} tokens · answer {found}/{} · failed {}",
            col.len(),
            col.len() - ok.len()
        );
    }
    Ok(())
}

/// A whole-page tool's result for a case: slowest URL, all markdown together.
fn whole_page_run(case: &Case, results: &HashMap<String, (f64, Option<String>)>) -> Run {
    let mut run = Run::default();
    let mut text = String::new();
    for url in &case.urls {
        let (ms, md) = &results[url];
        run.ms = run.ms.max(*ms);
        match md {
            Some(md) => text.push_str(md),
            None => run.failed = true,
        }
    }
    run.tokens = estimate_tokens(&text);
    run.found = case.expect.iter().all(|e| text.contains(e.as_str()));
    run
}

async fn firecrawl(client: &Client, key: &str, url: &str, fresh: bool) -> Option<String> {
    let mut body = json!({ "url": url, "formats": ["markdown"], "onlyMainContent": true });
    if fresh {
        body["maxAge"] = json!(0);
    }
    let resp: Value = client
        .post("https://api.firecrawl.dev/v2/scrape")
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let md = resp.pointer("/data/markdown").and_then(Value::as_str);
    if md.is_none() {
        eprintln!("firecrawl {url} (fresh: {fresh}): {}", resp.get("error").unwrap_or(&resp));
    }
    md.map(String::from)
}

async fn jina(client: &Client, url: &str) -> Option<String> {
    let resp = client
        .get(format!("https://r.jina.ai/{url}"))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}
