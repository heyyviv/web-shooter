# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Goal

**web-shooter**: a general, intent-driven web crawler exposed as an MCP server. The client LLM states what it wants and which URLs to look at, and web-shooter returns only the relevant content. It is not tied to any one domain.

Requirements stated by the user:
- Fast (at least as fast as Firecrawl's cloud API), low token consumption, and very precise.
- Compared against current tools (Firecrawl, Crawl4AI, Jina Reader, Exa/Tavily, spider-rs, browser agents) and benchmarked against Firecrawl.

## Status

Three MCP tools work and are in real use. The server is registered at user scope (`claude mcp add --scope user web-shooter`) and the user has used it for a job search:
- `fetch(urls[], query?, keywords?, max_tokens?, precise?)` downloads pages concurrently and converts them to markdown. Without a query it fits whole pages to the token budget (default 2,000). With `query`/`keywords` it returns ranked passages. `precise: true` adds a cross-encoder reranker. It also reads JSON API responses and the data embedded in JavaScript-shell pages.
- `links(url, query?, keywords?, max_links?)` lists a page's links, ranked by the query if given.
- `crawl(url, query, keywords?, max_pages?, max_depth?, other_sites?, max_results?)` does a best-first crawl from a start page and returns the most relevant pages as URLs with one-line snippets.

Quality: the eval has 19 cases, 4 of them from real use; 17 pass (`cargo run --release --example eval`). The two failures are a hard paraphrase and a location query on a Google Careers posting. Benchmark results are below.

## Commands

- Build: `cargo build` (release: `cargo build --release`). The registered MCP server runs `target/release/web-shooter`, so rebuild in release mode; open Claude Code sessions pick it up after `/mcp` reconnect or a restart.
- Test: `cargo test`
- Ranking test set: `cargo run --release --example eval`. Flags: `-v` prints the output of failed cases and the top passage of misranked ones, `--refresh` refetches page snapshots, `--precise` adds the reranker, `--sweep` compares reranker settings, `--bm25` ranks without embeddings. Run it after any change to extraction, chunking, ranking or packing, and report the before/after numbers.
- Debug one page: `cargo run --release --example probe -- <url> [variant]`. It prints each converter setting's output size and the extraction time to stderr. Pass `final` (or `plain`, `main`, `readability`, …) to print that markdown, or `rank "<query>"` to print the top 8 chunks with their scores.
- Benchmark against Firecrawl and Jina Reader (live network, ~14 Firecrawl credits per run): `FIRECRAWL_API_KEY=… cargo run --release --example bench`. Never write the key into the repo.
- Run the server by hand: `./target/release/web-shooter`. It speaks JSON-RPC on stdin/stdout, so log only to stderr.
- Register with Claude Code: `claude mcp add --scope user web-shooter -- /absolute/path/to/target/release/web-shooter`

## Architecture

- **Rust + tokio, one long-lived binary, MCP over stdio** (`rmcp`).
- **No LLM inside the server.** The client LLM supplies URLs and queries; there is no built-in web search. Token cost is what the tools return, so results are ranked passages or URL/snippet lists, never whole pages unless asked.
- **Built on the `spider` crate** for single-page fetching (`Page::new_page`), robots.txt and HTML-to-markdown (`spider_transformations`). spider's own crawl loop is breadth-first, so `crawl` drives its own prioritised frontier.
- **Pipeline for `fetch`:**
  1. **Fetch** over plain HTTP, up to 16 URLs at once.
  2. **Extract.** A JSON response is rendered record by record. HTML becomes its main content as markdown, plus JSON-LD facts. If the result is nearly empty (a JavaScript shell), the data embedded in its inline scripts is rendered instead.
  3. **Chunk** at headings into ~200-token passages, each with its heading path.
  4. **Rank:** BM25 plus static embeddings (`model2vec`), fused by a normalised weighted sum, then down-weighting for link lists and navigation snippets. `precise: true` reranks the top 6 with a cross-encoder.
  5. **Pack** the best passages into the token budget, grouped by page, with a short section outline when part of a page was left out.
- **No data is kept.** Pages exist only in memory during one tool call and nothing is written to disk. Between calls the server holds only the HTTP connection pool and the models. The user asked about this; keep it true unless they agree to the SQLite cache (M5), which would need expiry, a size cap and an off switch.
- **Output format** (compact markdown, not JSON):
  - `fetch` with a query: one `[n] url · title` line per page, then `§ Heading > Path · score` before each passage.
  - `crawl`: `[n] url · title · score`, then an indented one-line snippet.
  - `links`: `score · anchor text · url`.
- **Models** download into the Hugging Face cache on first use and are never built into the binary.

## Layout

The crate is a library (`src/lib.rs`) plus a thin binary (`src/main.rs`), so examples and tests can reach the modules.

- `src/server.rs`: `WebShooter`, the MCP handler. It holds the shared HTTP client and the `ModelSet`. Every tool is registered in its `#[tool_router]` impl, which delegates to `src/tools/`; the tool descriptions and server instructions there are what the client LLM reads.
- `src/tools/<name>.rs`: each tool's parameter struct (doc comments become the JSON schema the LLM sees, so keep them short) and its `run` function. `tools::query_text` joins query and keywords the same way for all tools.
- `src/tools/crawl.rs`: `BinaryHeap` frontier. A link's priority is 0.7 × its own text score + 0.3 × the relevance of the page it was found on, both from `rank::absolute`.
- `src/fetcher.rs`: the HTTP client (`spider::reqwest`) and single-page fetch with one retry on 5xx.
- `src/extract.rs`: response → markdown. One `lol_html` pass collects the title, canonical URL, links (with anchor text and heading), and inline scripts; it also drops MathML and icon glyphs. Then `spider_transformations` converts the main content and `tidy` removes noise.
- `src/data.rs`: JSON → markdown. Arrays of objects become one `##` section per record (title from a `title`/`name`-like key), short fields on one line, long or HTML fields as text. `embedded_json` finds JSON in inline scripts: `JSON.parse('…')` strings, `= {…}` literals, and JSON script bodies.
- `src/links.rs`: `Link`, URL normalisation and filtering, same-site checks.
- `src/chunk.rs`: chunking, back-matter skipping (References, Notes, External links…), the section outline, `MD_LINK` and `prose_share`.
- `src/rank.rs`: tokenizer (stopwords, Snowball stemming, hyphen joining), BM25, `hybrid` (BM25 + embeddings, normalised per call), `rank` (hybrid, shape factors, optional rerank) and `absolute` (fixed 0..1 scale, for comparing links and pages across a crawl). `RankConfig::default()` holds the tuned settings and the reasons for them.
- `src/embed.rs`, `src/rerank.rs`, `src/models.rs`: the embedding model (preloaded at startup; the first query waits for it), the reranker (loaded on the first `precise` call), and `ModelSet`, which hands ranking the reranker only for `precise` calls. If a model can't load, ranking continues without it.
- `src/pack.rs`: `PageOut`, `pack` (whole pages, fair share of the budget) and `pack_passages`. Tokens are estimated as bytes / 4.
- `eval/cases.json`: test cases (pages, query, text the answer must contain). Page snapshots are cached in `eval/snapshots/` (git-ignored).
- `examples/`: `eval`, `probe`, `bench` (see Commands).

## Benchmark results

First run (2026-10-11) on the original 15 eval cases, single run, live network. Firecrawl `/v2/scrape` and Jina return whole pages, so their "answer present" is near-certain; web-shooter returns only ranked passages.

| Tool | Median latency | Avg tokens | Answer present |
|---|---|---|---|
| web-shooter `fetch` + query | 405 ms | 1,138 | 14/15 |
| Firecrawl (cache allowed) | 1,214 ms | 16,513 | 15/15 |
| Firecrawl fresh (`maxAge: 0`) | 3,341 ms | 16,523 | 15/15 |
| Jina Reader | 625 ms | 15,692 | 15/15 |

Caveats:
- These are the same cases the ranking was tuned on.
- There was one run, so slow origins skew it (paulgraham.com took 0.9–6 s).
- None of these pages need JavaScript or have anti-bot protection, which is where Firecrawl and Jina have an advantage.
- `crawl` hasn't been compared yet.
- The Firecrawl free plan allows about 10 requests a minute, so `bench` spaces its requests 6.5 s apart.

## Decisions and gotchas

Dependencies:
- Use `rmcp::schemars`, not a separate `schemars` dependency, so the versions match.
- `spider` is used with `default-features = false`. Its default and `basic` features pull in io_uring, sqlx and more.
- `model2vec-rs` is built with `default-features = false, features = ["hf-hub", "fancy-regex"]` to avoid the oniguruma C library.
- `fastembed` is built with `default-features = false` and rustls features. Its default cache is `./.fastembed_cache` relative to the working directory, so `rerank.rs` points it at the Hugging Face cache.

Ranking:
- Fusion is a min-max-normalised weighted sum (BM25 weight 0.3, cut-off 0.7 of the top score), not RRF as first planned. RRF ranked as well in the eval, but its flat scores defeat the relative cut-off, so replies filled the budget.
- The reranker is opt-in (`precise: true`), not on by default as first planned. On the eval it didn't beat embeddings at ranking (top 13/15 vs 14/15) and adds ~300 ms, though it cuts tokens by ~33%. Jina v1 turbo was faster but worse (top 8/15); BGE base was slower and worse. Its model (Jina v2 base) is ~1.1 GB.
- `rank::rank` multiplies scores by `shape_factor` in both modes: link lists (3+ links and under 100 letters/digits outside them) × 0.5, very short passages (≤ 6 words, usually navigation) × 0.5. A plain link-density penalty hurt the React case, whose answer sentence shares a chunk with video links.
- The embedding model is `minishlab/potion-retrieval-32M` (override with `WEB_SHOOTER_EMBED_MODEL`). The first download took ~33 s; after that it loads in ~50 ms.

Extraction:
- In `spider_transformations`, keep `clean_html` off because it empties Wikipedia and Next.js pages. Keep `readability` off because it drops headings. Noise is removed in `extract::tidy` instead.
- MathML (`<math>`) is removed in the `lol_html` pass, because the converter otherwise emits formulas one character per line.
- Icon-font glyph names ("place", "arrow_back") are replaced with a space: aria-hidden `<i>`/`<span>` and Material icon classes. Aria-hidden `<script>` is left alone because it can hold page data.
- Pages whose markdown has under 200 visible characters are treated as JavaScript shells, and every inline script that can hold data (no `src`, JS or JSON type) goes through `data::embedded_json`. The rendered blobs are sorted by prose (letters in lines of 6+ words) so content comes before config: on Zoho a font list came first, so `fetch` without a query showed only fonts. JSON keys containing `/` are skipped, because they are asset manifests (Zoho's ran to 1.6 MB of markdown). JSON stored inside JSON strings is parsed and rendered.
- Text from separate text nodes is joined with a space (`extract::push_text`) so `<span>Title</span><span>London</span>` doesn't become "TitleLondon".
- Markdown links can contain parentheses in the URL (`…/Search_engine_(computing)`). `chunk::MD_LINK` allows one level and is used by chunking, ranking and crawl. The simpler `[^)]*` patterns in `extract::tidy` and `rank::LINK_URL` cut such URLs short, which is harmless there.

Fetching and crawling:
- The HTTP client sends browser `Accept`/`Accept-Language` headers (uber.com returned 406 without them) and an honest user agent, `web-shooter/<version>`. It retries a 5xx once after 500 ms.
- `fetch` and `links` do not check robots.txt: they read pages the client asked for by URL, like a browser. `crawl` checks robots.txt for every page it discovers (not the start page), using `spider::packages::robotparser::parser::RobotFileParser` with `fetcher::USER_AGENT`.
- `crawl` limits: 4 concurrent fetches (its politeness control; there is no fixed delay), 30 s, `max_pages` (default 30, max 200), `max_depth` (default 3), same site unless `other_sites`. It stops early after 12 pages in a row that don't enter the top results.
- `crawl` leaves "thin" pages (under 15% of their letters and digits outside links: indexes, link lists, Wikipedia's "What links here") out of the results but still follows their links. Duplicates are dropped by canonical URL, then by content hash.

## What real use showed (job search, 2026-10-11)

- **Works:** single postings in plain HTML (Amazon, Google), Zoho Recruit pages (embedded `JSON.parse` data), and JSON APIs such as Amazon's `search.json`.
- **Still fails without a browser:**
  - Sites whose content is fetched by JavaScript from a separate API: Oracle HCM (Stanford), Atlassian, Amazon's search page.
  - Bot checks: Uber via Cloudflare. Uber's real job system is also Oracle HCM.
  For these, the client can pass the site's JSON API URL to `fetch` when it knows it; otherwise they need the Chrome fallback.
- **Ranking gap:** a vague query ("job location") misses a fact that has no label on the page ("Google Bengaluru, Karnataka, India"). Passing the expected value as a keyword works.
- **Large JSON APIs:** Amazon's `search.json` with 163 jobs is ~68k tokens without a query, so the client LLM preferred curl + jq when it wanted everything. With a query, `fetch` returns only the matching jobs.
- Test without a query too: the Zoho bug only showed up in no-query mode, because the eval only runs queries.

## Plan

| # | Milestone | Status |
|---|---|---|
| M0 | Skeleton: stdio MCP server | done |
| M1 | Fetch many URLs, convert to markdown, fit the token budget | done |
| M2 | Chunking, BM25 and passage packing | done |
| M3 | Static embeddings fused with BM25 | done (weighted fusion instead of RRF) |
| M4 | Cross-encoder reranker | done (opt-in via `precise`) |
| M6 | `links` and `crawl` with the scored frontier and early stop | done |
| M7 | Benchmark against Firecrawl and Jina, real-use fixes, held-out cases | in progress: single-page benchmark and real-use fixes done; held-out cases and the crawl comparison remain (the crawl comparison costs Firecrawl credits, so ask first) |
| v2 | Headless Chrome fallback, only when plain HTTP returns an empty shell | proposed next, ahead of M8, because 4 of the user's failing sites need it; not yet agreed |
| M8 | PDF support (research papers) | planned |
| M5 | SQLite cache with ETag | planned last; needs expiry, size cap and an off switch (see "No data is kept") |

Keep adding the user's real queries to `eval/cases.json`, and keep some cases out of tuning so there is an honest held-out number.

## Working agreement

- Agree on scope and design with the user before writing code. An early implementation was started without that and was discarded at the user's request. Small bug fixes found in real use are fine to make directly; new features and changes of plan need agreement.
- Report results honestly, including failures and what the numbers don't show (cases tuned on, single runs, missing comparisons).
