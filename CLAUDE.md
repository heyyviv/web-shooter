# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Status

Milestone M0 done: a stdio MCP server with a stub `fetch` tool. Next is M1 (fetch pages and convert them to markdown). Milestones M0–M7 are listed under "Plan" below.

## Commands

- Build: `cargo build` (release: `cargo build --release`)
- Run as MCP server: `./target/debug/web-shooter` (speaks JSON-RPC on stdin/stdout, so log only to stderr)
- Register with Claude Code: `claude mcp add web-shooter -- /absolute/path/to/target/release/web-shooter`

## Layout

- `src/main.rs`: starts the server on stdio.
- `src/server.rs`: `WebShooter`, the MCP handler. Every tool is registered in its `#[tool_router]` impl, which delegates to `src/tools/`.
- `src/tools/<name>.rs`: each tool's parameter struct (doc comments become the JSON schema the LLM sees, so keep them short) and its `run` function.
- Use `rmcp::schemars`, not a separate `schemars` dependency, so the versions match.

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
  - `fetch(urls[], query?, keywords?, max_tokens?)`: relevant passages from one or many pages.
  - `crawl(start_url, query, keywords?, max_pages?, depth?)`: focused crawl that returns a ranked URL list.
  - `links(url, query?)`: a page's links, ranked.
- **Pipeline:**
  1. **Fetch:** plain HTTP first.
  2. **Extract:** read embedded JSON first (`__NEXT_DATA__`, JSON-LD). Otherwise extract the main content and convert it to markdown. If the result is empty, fall back to headless Chrome.
  3. **Chunk:** split by headings and prefix each chunk with its heading path.
  4. **Rank:** BM25 plus static embeddings (`model2vec`), merged with reciprocal rank fusion (RRF). Then a cross-encoder reranker, **on by default**.
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
| M1 | Fetch many URLs at once and convert to markdown, read embedded JSON, cut to the token budget |
| M2 | Chunking, BM25 and packing: `fetch` with `query` returns only matching passages |
| M3 | Static embeddings (`model2vec-rs`) merged with BM25 by RRF, models downloaded on first run |
| M4 | Cross-encoder reranker (`fastembed`), on by default |
| M5 | SQLite cache (`rusqlite`) with ETag |
| M6 | `links` and `crawl` tools with the scored frontier and early stop |
| M7 | Test set and Firecrawl benchmark, then tuning |
| v2 | Headless Chrome fallback |

Build the test set gradually from M2 onwards so each ranking change is measured.

## Working agreement

- Agree on scope and design with the user before writing code. An early implementation was started without that and was discarded at the user's request.
