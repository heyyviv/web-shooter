//! Ranking test set: `cargo run --release --example eval [-- --refresh] [-- -v]`.
//!
//! Each case in `eval/cases.json` names pages, a query, and text the answer
//! must contain. Pages are saved under `eval/snapshots/` on first run so
//! results don't drift as sites change; `--refresh` fetches them again.
//!
//! Reported per case:
//! - found: every expected string appears somewhere in the output
//! - top: the highest-scored passage contains the first expected string
//! - tokens: estimated size of the output

use std::{fs, path::PathBuf};

use serde::Deserialize;
use web_shooter::{
    extract::extract,
    fetcher,
    pack::estimate_tokens,
    tools::fetch::{FetchParams, page_out, respond},
};

#[derive(Deserialize)]
struct Case {
    name: String,
    urls: Vec<String>,
    query: String,
    keywords: Option<Vec<String>>,
    max_tokens: Option<usize>,
    expect: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let refresh = args.iter().any(|a| a == "--refresh");
    let verbose = args.iter().any(|a| a == "-v");

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval");
    let cases: Vec<Case> = serde_json::from_str(&fs::read_to_string(root.join("cases.json"))?)?;
    let snapshots = root.join("snapshots");
    fs::create_dir_all(&snapshots)?;
    let client = fetcher::build_client()?;

    let (mut found_n, mut top_n, mut tokens_total) = (0, 0, 0);
    println!("{:<26} {:>5} {:>4} {:>7}", "case", "found", "top", "tokens");
    for case in &cases {
        let mut pages = Vec::new();
        for url in &case.urls {
            let path = snapshots.join(format!("{:016x}.html", fnv(url)));
            if refresh || !path.exists() {
                let f = fetcher::fetch(&client, url).await;
                anyhow::ensure!(f.status == 200, "{url}: HTTP {}", f.status);
                fs::write(&path, &f.body)?;
            }
            let body = fs::read(&path)?;
            pages.push(page_out(url.clone(), Some(extract(url, &body))));
        }

        let params = FetchParams {
            urls: case.urls.clone(),
            query: Some(case.query.clone()),
            keywords: case.keywords.clone(),
            max_tokens: case.max_tokens,
        };
        let out = respond(&pages, &params);
        let found = case.expect.iter().all(|e| out.contains(e.as_str()));
        let top = best_passage(&out).contains(case.expect[0].as_str());
        let tokens = estimate_tokens(&out);

        found_n += found as usize;
        top_n += top as usize;
        tokens_total += tokens;
        println!(
            "{:<26} {:>5} {:>4} {:>7}",
            case.name,
            mark(found),
            mark(top),
            tokens
        );
        if verbose && !found {
            println!("--- output for {}\n{out}\n---", case.name);
        }
    }
    println!(
        "\nfound {found_n}/{n} · top {top_n}/{n} · avg {} tokens",
        tokens_total / cases.len().max(1),
        n = cases.len()
    );
    Ok(())
}

/// The passage block whose `§ heading · score` line has the highest score.
fn best_passage(out: &str) -> &str {
    out.split("\n§ ")
        .skip(1)
        .max_by(|a, b| score_of(a).total_cmp(&score_of(b)))
        .unwrap_or("")
}

fn score_of(block: &str) -> f32 {
    let header = block.lines().next().unwrap_or("");
    header
        .rsplit(" · ")
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

fn mark(ok: bool) -> &'static str {
    if ok { "yes" } else { "NO" }
}

/// Stable file name for a URL's snapshot (FNV-1a).
fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}
