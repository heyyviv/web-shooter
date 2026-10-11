//! Debug extraction on one URL: `cargo run --example probe -- <url> [variant]`,
//! or see how chunks rank: `cargo run --example probe -- <url> rank "<query>"`.
use spider_transformations::transformation::content::{
    ReturnFormat, TransformConfig, TransformInput, transform_content_input,
};
use web_shooter::fetcher;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("url");
    let variant = args.next().unwrap_or_default();
    let client = fetcher::build_client()?;
    let f = fetcher::fetch(&client, &url).await;
    eprintln!("status {} · {} bytes · {}", f.status, f.body.len(), f.url);
    let cfg = |main_content, readability, clean_html| TransformConfig {
        return_format: ReturnFormat::Markdown,
        main_content,
        readability,
        clean_html,
        filter_images: true,
        filter_svg: true,
    };
    let parsed = spider::url::Url::parse(&f.url).ok();
    for (name, c) in [
        ("plain", cfg(false, false, false)),
        ("main", cfg(true, false, false)),
        ("main+clean", cfg(true, false, true)),
        ("readability", cfg(false, true, false)),
        ("readability+main", cfg(true, true, true)),
    ] {
        let input = TransformInput {
            url: parsed.as_ref(),
            content: &f.body,
            screenshot_bytes: None,
            encoding: None,
            selector_config: None,
            ignore_tags: None,
        };
        let md = transform_content_input(input, &c);
        if variant == name {
            println!("{md}");
        }
        eprintln!(
            "{name:>18}: {:>7} chars · starts: {:?}",
            md.len(),
            md.chars().take(80).collect::<String>()
        );
    }
    let started = std::time::Instant::now();
    let e = web_shooter::extract::extract(&f.url, &f.body);
    eprintln!("extract took {} ms", started.elapsed().as_millis());
    if variant == "rank" {
        let query = args.next().expect("query");
        let chunks = web_shooter::chunk::chunk(0, &e.markdown);
        let embedder = web_shooter::embed::load();
        let models = web_shooter::rank::Models {
            embedder: embedder.as_ref(),
            reranker: None,
        };
        let ranked = web_shooter::rank::rank(&query, &chunks, models, Default::default());
        let mut order: Vec<usize> = (0..chunks.len()).collect();
        order.sort_by(|&a, &b| ranked.scores[b].total_cmp(&ranked.scores[a]));
        let top = ranked.scores[order[0]];
        for &i in order.iter().take(8) {
            let c = &chunks[i];
            let text: String = c.text.split_whitespace().collect::<Vec<_>>().join(" ");
            println!(
                "{:.2} [{}] {} | {}",
                ranked.scores[i] / top,
                c.index,
                c.heading_path,
                text.chars().take(110).collect::<String>()
            );
        }
        return Ok(());
    }
    if variant == "final" {
        println!("{}", e.markdown);
    }
    eprintln!(
        "{:>18}: {:>7} chars · title: {:?}",
        "final",
        e.markdown.len(),
        e.title
    );
    Ok(())
}
