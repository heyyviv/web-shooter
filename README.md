# web-shooter

A fast, low-token web reader and focused crawler for AI assistants, served over [MCP](https://modelcontextprotocol.io). Instead of handing the assistant whole pages, it returns only the passages or pages that match what the assistant asked for.

On our test set it answers in a median of **405 ms** with about **1,100 tokens**, where Firecrawl and Jina Reader return about 16,000 tokens per page (details under [Benchmark](#benchmark)).

- **No LLM inside.** The assistant decides what to read; web-shooter fetches, cleans, ranks and trims. Ranking runs locally on the CPU in milliseconds.
- **Local.** One Rust binary on your machine, started by your MCP client over stdio.
- **Keeps nothing.** Pages live in memory for one tool call only; nothing is written to disk.

## Tools

| Tool | Use it when | Returns |
| --- | --- | --- |
| `fetch(urls, query?, keywords?, max_tokens?, precise?)` | You have URLs and want their content | With a query: the best passages, grouped by page. Without: whole pages trimmed to `max_tokens` (default 2,000) |
| `links(url, query?, keywords?, max_links?)` | You want to choose which page to read next | The page's links, ranked by the query if given |
| `crawl(url, query, keywords?, max_pages?, max_depth?, other_sites?, max_results?)` | You have a start page and a goal, not the URLs | The most relevant pages found, as URLs with a one-line snippet each |

`fetch` also reads JSON API responses (one section per record) and the data embedded in JavaScript-rendered pages. `precise: true` adds a slower cross-encoder reranker (about 300 ms more, about a third fewer tokens). `crawl` respects robots.txt, stays on the start site by default, fetches 4 pages at a time and stops after 30 pages or 30 seconds by default.

## Setup

### 1. Install Rust

You need Rust **1.88 or later**. If you don't have it:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable
```

### 2. Build

```sh
git clone https://github.com/heyyviv/web-shooter.git
cd web-shooter
cargo build --release
```

The binary is `target/release/web-shooter`. The first build downloads dependencies, including ONNX Runtime for the optional reranker, and takes a few minutes.

### 3. Register it with your MCP client

**Claude Code** (available in all your projects):

```sh
claude mcp add --scope user web-shooter -- "$(pwd)/target/release/web-shooter"
```

Then start a new session, or run `/mcp` to connect. Check with `claude mcp list`; it should show `web-shooter … ✔ Connected`.

**Claude Desktop, Cursor and other MCP clients**: add a stdio server to the client's MCP config, using the absolute path to the binary:

```json
{
  "mcpServers": {
    "web-shooter": {
      "command": "/absolute/path/to/web-shooter/target/release/web-shooter"
    }
  }
}
```

### 4. First run

The first query downloads the embedding model (`minishlab/potion-retrieval-32M`, about 130 MB, roughly 30 seconds) into the Hugging Face cache, `~/.cache/huggingface`. After that it loads in about 50 ms. The reranker (about 1.1 GB) downloads only on the first `precise: true` call. If a model can't be downloaded, for example offline, ranking falls back to keyword matching instead of failing.

### Updating

```sh
git pull
cargo build --release
```

Running sessions keep the old binary: reconnect with `/mcp` in Claude Code, or restart your client.

### Removing

```sh
claude mcp remove --scope user web-shooter
```

This only unregisters it. To free the disk space too, delete the repo folder and the cached models:

```sh
rm -rf ~/.cache/huggingface/hub/models--minishlab--potion-retrieval-32M \
       ~/.cache/huggingface/hub/models--jinaai--jina-reranker-v2-base-multilingual
```

## Usage

Ask your assistant to use it by name. For example:

- "Use web-shooter `crawl` on https://blog.rust-lang.org with the query 'security vulnerabilities in cargo'."
- "Use web-shooter `fetch` on these 5 job posting URLs with the query 'visa sponsorship, location, years of experience'."
- "Use web-shooter `links` on this careers page with the query 'machine learning roles', then fetch the top 3."

Tips:

- Pass several URLs to one `fetch` call; they are fetched in parallel.
- Add `keywords` with exact terms or synonyms when the answer may not share words with the question (a city name, a product code).
- `crawl` returns URLs and snippets, not page text. Follow up with `fetch` on the pages you want.

### Configuration

| Environment variable | Effect |
| --- | --- |
| `WEB_SHOOTER_EMBED_MODEL` | A different model2vec embedding model (Hugging Face repo id or local folder) |
| `WEB_SHOOTER_RERANK_MODEL` | A different reranker for `precise`: `bge-base`, `bge-m3` or `jina-turbo` |
| `HF_HOME` | Where models are cached (default `~/.cache/huggingface`) |

## What it can't read yet

web-shooter does not run JavaScript. Pages that load their content from a separate API after the page arrives come back empty: Oracle HCM career sites, many search-results pages, and sites behind bot checks such as Cloudflare. If you know the site's JSON API URL, pass that to `fetch`; otherwise use a browser tool. A headless-Chrome fallback is planned. PDFs are not supported yet either.

## Benchmark

The same 15 questions, single run, live network ([`examples/bench.rs`](examples/bench.rs)):

| Tool | Median latency | Avg tokens returned | Answer present |
| --- | --- | --- | --- |
| web-shooter `fetch` with a query | 405 ms | 1,138 | 14/15 |
| Jina Reader | 625 ms | 15,692 | 15/15 |
| Firecrawl, cache allowed | 1,214 ms | 16,513 | 15/15 |
| Firecrawl, fresh fetch | 3,341 ms | 16,523 | 15/15 |

Caveats: Firecrawl and Jina return whole pages, so they always contain the answer. web-shooter's ranking was tuned on these same questions. None of these pages needed JavaScript, which is where Firecrawl and Jina are strongest.

## Development

```sh
cargo test                                            # unit tests
cargo run --release --example eval                    # ranking test set (eval/cases.json)
cargo run --release --example probe -- <url> final    # markdown extracted from one page
cargo run --release --example probe -- <url> rank "<query>"   # top passages with scores
FIRECRAWL_API_KEY=… cargo run --release --example bench       # benchmark (uses Firecrawl credits)
```

`CLAUDE.md` describes the architecture, the design decisions and the reasons behind them.
