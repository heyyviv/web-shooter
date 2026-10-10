# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Milestones M0–M4 and M6 are done. Three tools:
- `fetch` downloads many URLs concurrently and converts them to markdown. Without a query it fits whole pages to the token budget. With `query`/`keywords` it returns passages ranked by BM25 fused with static embeddings, and with `precise: true` it also reranks them with a cross-encoder.
- `links` lists a page's links, ranked by a query if given.
- `crawl` does a best-first crawl from a start page and returns the most relevant pages as URLs with one-line snippets.

Next is M7 (benchmark against Firecrawl and Jina). Milestones are listed under "Plan" below.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Test: `cargo test`
- Debug extraction on one page: `cargo run --example probe -- <url> [variant]`. It prints the size of each converter setting to stderr; pass `final` (or `plain`, `main`, `readability`, …) to print that markdown.
- Ranking test set: `cargo run --release --example eval` (`-v` prints output for failed cases, `--refresh` refetches page snapshots, `--precise` adds the reranker, `--sweep` compares reranker settings, `--bm25` ranks without embeddings). Run it after any change to extraction, chunking, ranking or packing, and report the before/after numbers.
- Run as MCP server: `./target/debug/web-shooter` (speaks JSON-RPC on stdin/stdout, so log only to stderr)
- Register with Claude Code: `claude mcp add web-shooter -- /absolute/path/to/target/release/web-shooter`

## Layout

The crate is a library (`src/lib.rs`) plus a thin binary (`src/main.rs`), so examples and tests can reach the modules.

- `src/server.rs`: `WebShooter`, the MCP handler. It holds the shared HTTP client. Every tool is registered in its `#[tool_router]` impl, which delegates to `src/tools/`.
- `src/tools/<name>.rs`: each tool's parameter struct (doc comments become the JSON schema the LLM sees, so keep them short) and its `run` function.
- `src/fetcher.rs`: the HTTP client (via `spider::reqwest`) and single-page fetch through `spider::page::Page::new_page`.
- `src/extract.rs`: HTML → markdown. It reads the title, canonical URL, JSON-LD, `__NEXT_DATA__` and links in one `lol_html` pass, converts with `spider_transformations`, then cleans up the result in `tidy`.
- `src/links.rs`: `Link` (URL, anchor text, heading above it), URL normalisation and filtering, same-site checks.
- `src/tools/links.rs`, `src/tools/crawl.rs`: the `links` and `crawl` tools. `crawl` keeps a `BinaryHeap` frontier; a link's priority is 0.7 × its own text score + 0.3 × the relevance of the page it was found on, both from `rank::absolute`.
- `src/chunk.rs`: splits markdown into ~200-token chunks at headings, each with its heading path. Skips back-matter sections (References, Notes, External links…).
- `src/rank.rs`: tokenizer (stopwords, Snowball stemming, hyphen joining), BM25 over all chunks in the call, and `hybrid`, which fuses BM25 with embedding similarity. `absolute` gives scores on a fixed 0..1 scale (query-term share averaged with cosine), for comparing links and pages across a crawl; `hybrid` is normalised per call, so it can't be used for that. `RankConfig::default()` holds the tuned fusion and cut-off.
- `src/embed.rs`: `Embedder` loads the `model2vec` static embedding model once in the background (started in `main`). The first query waits for it. If it can't load, ranking falls back to BM25.
- `src/rerank.rs`: `Reranker` wraps the `fastembed` cross-encoder (Jina v2 base). It is not preloaded; it loads on the first `precise` call.
- `src/models.rs`: `ModelSet`, the models a server owns. `ready(precise)` hands ranking the reranker only for `precise` calls.
- `src/pack.rs`: `pack` fits whole pages into the budget, and `pack_passages` packs ranked passages grouped by page (tokens are estimated as bytes / 4).
- `eval/cases.json`: test cases (pages, query, text the answer must contain). Page snapshots are cached in `eval/snapshots/` (git-ignored).

## Gotchas

- Use `rmcp::schemars`, not a separate `schemars` dependency, so the versions match.
- `spider` is used with `default-features = false`. Its default and `basic` features pull in io_uring, sqlx and more.
- MathML (`<math>`) is removed during the `lol_html` pass in `extract::scan`, because the converter otherwise emits formulas one character per line.
- In `spider_transformations`, keep `clean_html` off because it empties Wikipedia and Next.js pages. Keep `readability` off because it drops headings. Noise is removed in `extract::tidy` instead.
- The embedding model (`minishlab/potion-retrieval-32M`, override with `WEB_SHOOTER_EMBED_MODEL`) is downloaded into the Hugging Face cache (`~/.cache/huggingface`) on first use. That took ~33 s once; after that it loads in ~50 ms.
- `model2vec-rs` is built with `default-features = false, features = ["hf-hub", "fancy-regex"]` to avoid the oniguruma C library.
- `fastembed` is built with `default-features = false` and rustls features. Its default cache is `./.fastembed_cache` relative to the working directory, so `rerank.rs` points it at the Hugging Face cache.
- The reranker is opt-in (`precise: true`), not on by default as first planned. On the eval it didn't beat embeddings at ranking (top 13/15 vs 14/15) and adds ~300 ms, though it cuts tokens by ~33%. Jina v1 turbo was faster but worse (top 8/15); BGE base was slower and worse. Its model is ~1.1 GB.
- Fusion is a min-max-normalised weighted sum, not RRF as first planned. RRF ranked as well in the eval, but its flat scores defeat the relative cut-off, so replies filled the budget.
- `fetch` and `links` do not check robots.txt: they read pages the client asked for by URL, like a browser. `crawl` checks robots.txt for every page it discovers (not the start page), using `spider::packages::robotparser::parser::RobotFileParser` with `fetcher::USER_AGENT`.
- `crawl` limits: 4 concurrent fetches, 30 s, `max_pages` (default 30, max 200), `max_depth` (default 3), same site unless `other_sites`. It stops early after 12 pages in a row that don't enter the top results.
- `crawl` leaves "thin" pages (less than 15% of their letters and digits outside links: indexes, link lists, Wikipedia's "What links here") out of the results but still follows their links. Duplicates are dropped by canonical URL, then by content hash.
- Markdown links can contain parentheses in the URL (`…/Search_engine_(computing)`); `crawl.rs`'s `MD_LINK` allows one level. The simpler `[^)]*` pattern elsewhere cuts such URLs short.
- Text from separate text nodes is joined with a space (`extract::push_text`) so `<span>Title</span><span>London</span>` doesn't become "TitleLondon".

## Goal

**web-shooter**: a general, intent-driven web crawler exposed as an MCP server. The client LLM states what it wants and which URLs to look at, and web-shooter returns only the relevant content. It is not tied to any one domain.

## Requirements (stated by the user)

- Fast (target: at least as fast as Firecrawl's cloud API), low token consumption, and very precise.
- Compare against current tools (Firecrawl, Crawl4AI, Jina Reader, Exa/Tavily, spider-rs, browser agents) and benchmark against Firecrawl.

## Decided architecture

- **Language and runtime:** Rust, tokio. One long-lived binary.
- **Interface:** MCP server over **stdio**.
- **Engine:** built on the `spider` crate (spider-rs) for fetching, link extraction, robots.txt, throttling, and HTML-to-markdown. spider's own crawl loop is breadth-first with no relevance ordering, so web-shooter drives its own prioritised frontier.
- **No LLM inside the server.** The client LLM supplies starting URLs and queries; there is no built-in web search tool. Token cost is what the tools return, so results are ranked passages or URL/title/snippet lists, never whole pages unless asked.
- **Tools (v1 draft):**
  - `fetch(urls[], query?, keywords?, max_tokens?, precise?)`: relevant passages from one or many pages.
  - `crawl(url, query, keywords?, max_pages?, max_depth?, other_sites?, max_results?)`: focused crawl that returns a ranked URL list with snippets.
  - `links(url, query?, keywords?, max_links?)`: a page's links, ranked.
- **Pipeline:**
  1. **Fetch:** plain HTTP first.
  2. **Extract:** read embedded JSON first (`__NEXT_DATA__`, JSON-LD). Otherwise extract the main content and convert it to markdown. If the result is empty, fall back to headless Chrome.
  3. **Chunk:** split by headings and prefix each chunk with its heading path.
  4. **Rank:** BM25 plus static embeddings (`model2vec`), fused by a normalised weighted sum. Optionally a cross-encoder reranker, **opt-in per call** with `precise: true` (decided after the M4 eval).
  5. **Pack:** fill the token budget and return each passage with its URL, heading path and the page outline.
- **Crawl frontier:** a priority queue. Links are scored by the ranker's cheap stage (BM25 plus embeddings) on anchor text, URL and nearby text. The crawl stops early once it has enough good results.
- **Cache:** SQLite. It stores fetched pages and chunks with an expiry time and uses ETag / conditional requests.

- **Scope of v1:** plain HTTP plus embedded JSON only. Headless Chrome fallback comes in v2.
- **Models:** the embedding model and the reranker are downloaded automatically on first run into a cache folder, not built into the binary.
- **Tool output:** compact markdown text, not JSON. Each passage has a one-line header: `[n] url · Heading > Path · score`.
- **Defaults:** about 2,000 tokens per `fetch` call (adjustable per call). robots.txt is respected, with user agent `web-shooter/<version>` and a per-site delay.
- **Evaluation:** a test set of about 20 queries with known-good passages to measure precision, and a benchmark of about 50 URLs comparing speed and tokens against Firecrawl (needs the user's API key).

## Plan

| # | Milestone |
|---|---|
| M0 | Skeleton: stdio MCP server with a stub `fetch` (done) |
| M1 | Fetch many URLs at once and convert to markdown, read embedded JSON, cut to the token budget (done) |
| M2 | Chunking, BM25 and packing: `fetch` with `query` returns only matching passages (done) |
| M3 | Static embeddings (`model2vec-rs`) fused with BM25, models downloaded on first run (done; weighted fusion instead of RRF) |
| M4 | Cross-encoder reranker (`fastembed`) (done; opt-in via `precise`, not on by default) |
| M6 | `links` and `crawl` tools with the scored frontier and early stop (done) |
| M7 | Benchmark against Firecrawl and Jina, held-out test cases, then tuning (next) |
| M8 | PDF support (research papers) |
| M5 | SQLite cache (`rusqlite`) with ETag (moved after M8: it only speeds up repeat fetches) |
| v2 | Headless Chrome fallback |

The order changed after M4. The user is now trying the server in real tasks (registered at user scope with `claude mcp add --scope user web-shooter`). Turn their real queries into eval cases.

Build the test set gradually from M2 onwards so each ranking change is measured.

## Working agreement

- Agree on scope and design with the user before writing code. An early implementation was started without that and was discarded at the user's request.
