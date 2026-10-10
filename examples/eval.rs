//! Ranking test set: `cargo run --release --example eval [-- flags]`.
//!
//! Each case in `eval/cases.json` names pages, a query, and text the answer
//! must contain. Pages are saved under `eval/snapshots/` on first run so
//! results don't drift as sites change.
//!
//! Flags:
//! - `--refresh`: fetch the snapshots again
//! - `-v`: print the output of cases whose answer was not found
//! - `--sweep`: compare ranking settings, one summary line each
//! - `--bm25`: rank without the embedding model
//!
//! Reported per case:
//! - found: every expected string appears somewhere in the output
//! - top: the highest-scored passage contains the first expected string
//! - tokens: estimated size of the output
//! - ms: time to chunk, rank and pack (extraction excluded)

use std::{fs, path::PathBuf, time::Instant};

use model2vec_rs::model::StaticModel;
use serde::Deserialize;
use web_shooter::{
    embed,
    extract::extract,
    fetcher,
    pack::{PageOut, estimate_tokens},
    rank::{Fusion, RankConfig},
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

struct Outcome {
    found: bool,
    top: bool,
    tokens: usize,
    ms: f64,
    out: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval");
    let cases: Vec<Case> = serde_json::from_str(&fs::read_to_string(root.join("cases.json"))?)?;
    let pages = load_pages(&cases, &root.join("snapshots"), has("--refresh")).await?;
    let model = if has("--bm25") { None } else { embed::load() };

    if has("--sweep") {
        let mut configs = vec![(
            "bm25 only".to_string(),
            None,
            RankConfig {
                fusion: Fusion::Weighted { bm25_weight: 1.0 },
                min_relative: 0.4,
            },
        )];
        for k in [5.0, 20.0, 60.0] {
            for min_relative in [0.5, 0.7, 0.85] {
                configs.push((
                    format!("rrf k={k} min={min_relative}"),
                    model.as_ref(),
                    RankConfig {
                        fusion: Fusion::Rrf { k },
                        min_relative,
                    },
                ));
            }
        }
        for bm25_weight in [0.2, 0.3, 0.4] {
            for min_relative in [0.5, 0.6, 0.7] {
                configs.push((
                    format!("weighted w={bm25_weight} min={min_relative}"),
                    model.as_ref(),
                    RankConfig {
                        fusion: Fusion::Weighted { bm25_weight },
                        min_relative,
                    },
                ));
            }
        }
        for (name, model, config) in configs {
            let outcomes = run_all(&cases, &pages, model, config);
            println!("{name:<30} {}", summary(&outcomes));
        }
        return Ok(());
    }

    let outcomes = run_all(&cases, &pages, model.as_ref(), RankConfig::default());
    println!(
        "{:<26} {:>5} {:>4} {:>7} {:>6}",
        "case", "found", "top", "tokens", "ms"
    );
    for (case, o) in cases.iter().zip(&outcomes) {
        println!(
            "{:<26} {:>5} {:>4} {:>7} {:>6.1}",
            case.name,
            mark(o.found),
            mark(o.top),
            o.tokens,
            o.ms
        );
        if has("-v") && !o.found {
            println!("--- output for {}\n{}\n---", case.name, o.out);
        }
    }
    println!("\n{}", summary(&outcomes));
    Ok(())
}

/// Extracted pages for every case, from snapshots (fetched on first use).
async fn load_pages(
    cases: &[Case],
    snapshots: &PathBuf,
    refresh: bool,
) -> anyhow::Result<Vec<Vec<PageOut>>> {
    fs::create_dir_all(snapshots)?;
    let client = fetcher::build_client()?;
    let mut all = Vec::new();
    for case in cases {
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
        all.push(pages);
    }
    Ok(all)
}

fn run_all(
    cases: &[Case],
    pages: &[Vec<PageOut>],
    model: Option<&StaticModel>,
    config: RankConfig,
) -> Vec<Outcome> {
    cases
        .iter()
        .zip(pages)
        .map(|(case, pages)| {
            let params = FetchParams {
                urls: case.urls.clone(),
                query: Some(case.query.clone()),
                keywords: case.keywords.clone(),
                max_tokens: case.max_tokens,
            };
            let started = Instant::now();
            let out = respond(pages, &params, model, config);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            Outcome {
                found: case.expect.iter().all(|e| out.contains(e.as_str())),
                top: best_passage(&out).contains(case.expect[0].as_str()),
                tokens: estimate_tokens(&out),
                ms,
                out,
            }
        })
        .collect()
}

fn summary(outcomes: &[Outcome]) -> String {
    let n = outcomes.len().max(1);
    let found = outcomes.iter().filter(|o| o.found).count();
    let top = outcomes.iter().filter(|o| o.top).count();
    let tokens = outcomes.iter().map(|o| o.tokens).sum::<usize>() / n;
    let ms = outcomes.iter().map(|o| o.ms).sum::<f64>() / n as f64;
    format!(
        "found {found}/{n} · top {top}/{n} · avg {tokens} tokens · avg {ms:.1} ms",
        n = outcomes.len()
    )
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
