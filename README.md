# web-shooter

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A fast, low-token web reader and focused crawler for AI assistants, served over [MCP](https://modelcontextprotocol.io). Instead of handing the assistant whole pages, it returns only the passages or pages that match what the assistant asked for.

How it works: [docs/how-it-works.md](docs/how-it-works.md).

On our test set it answers in a median of **405 ms** with about **1,100 tokens**, where Firecrawl and Jina Reader return about 16,000 tokens per page (details under [Benchmark](#benchmark)).

Example: asking one Rust Book page "what are the rules of ownership in rust" returns this, not the whole chapter:

```
[1] https://doc.rust-lang.org/book/ch04-01-what-is-ownership.html · What is Ownership? - The Rust Programming Language
§ The Rust Programming Language > What Is Ownership? > Ownership Rules · 1.00
First, let’s take a look at the ownership rules. Keep these rules in mind as we
work through the examples that illustrate them:
* Each value in Rust has an *owner*.
* There can only be one owner at a time.
* When the owner goes out of scope, the value will be dropped.
```

- **No LLM inside.** The assistant decides what to read; web-shooter fetches, cleans, ranks and trims. Ranking runs locally on the CPU in milliseconds.
- **Local.** One Rust binary on your machine, started by your MCP client over stdio.
- **Keeps nothing.** Pages live in memory for one tool call only; nothing is written to disk.

## Tools

| Tool | Use it when | Returns |
| --- | --- | --- |
| `fetch(urls, query?, keywords?, max_tokens?)` | You have URLs and want their content | With a query: the best passages, grouped by page. Without: whole pages trimmed to `max_tokens` (default 2,000) |
| `links(url, query?, keywords?, max_links?)` | You want to choose which page to read next | The page's links, ranked by the query if given |
| `crawl(url, query, keywords?, max_pages?, max_depth?, other_sites?, max_results?)` | You have a start page and a goal, not the URLs | The most relevant pages found, as URLs with a one-line snippet each |
| `render(urls, query?, keywords?, max_tokens?)` | `fetch` reports no readable content: the page builds itself with JavaScript | Like `fetch`, after loading the pages in headless Chrome (about 1–8 s a page) |

`fetch` also reads PDFs, from a URL or a local path (`/path/paper.pdf`, `~/…`, `file://…`): headings are recovered from font sizes and section numbers, two-column papers are read column by column, running headers and footers are dropped, and each passage is labelled with its page (`§ 5.3 Log replication · p.7`). It reads JSON API responses (one section per record) and the data embedded in JavaScript-rendered pages too. `crawl` respects robots.txt, stays on the start site by default, fetches 4 pages at a time and stops after 30 pages or 30 seconds by default.

## Setup

### 1. Install

Pick one. Each gives you a single `web-shooter` binary.

**Prebuilt binary** (macOS on Apple Silicon, Linux x86_64 and ARM64, Windows x86_64), from the [latest release](https://github.com/heyyviv/web-shooter/releases/latest):

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/heyyviv/web-shooter/releases/latest/download/web-shooter-installer.sh | sh
```

```powershell
# Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/heyyviv/web-shooter/releases/latest/download/web-shooter-installer.ps1 | iex"
```

This puts `web-shooter` in `~/.cargo/bin`. Read the installer script first if you like; it only downloads the matching archive from the release page.

**From source** (needs Rust **1.88 or later**; Intel Macs must use this route, since there is no prebuilt binary for them):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # if you don't have Rust
cargo install --git https://github.com/heyyviv/web-shooter
```

The first build downloads dependencies and takes a few minutes.

To include the optional cross-encoder reranker, build with `--features rerank`. It adds a `precise` option to `fetch` (about 300 ms more per call, about a third fewer tokens, no better at ranking in our tests), ONNX Runtime to the binary (15 MB → 35 MB) and a 1.1 GB model download on first use.

### 2. Register it with your MCP client

**Claude Code** (available in all your projects):

```sh
claude mcp add --scope user web-shooter -- web-shooter
```

Then start a new session, or run `/mcp` to connect. Check with `claude mcp list`; it should show `web-shooter … ✔ Connected`. If your shell can't find `web-shooter`, use the absolute path (`$(which web-shooter)`).

**Claude Desktop, Cursor and other MCP clients**: add a stdio server to the client's MCP config, using the absolute path to the binary:

```json
{
  "mcpServers": {
    "web-shooter": {
      "command": "/absolute/path/to/web-shooter"
    }
  }
}
```

### 3. First run

The first query downloads the embedding model (`minishlab/potion-base-8M`, about 30 MB) into the Hugging Face cache, `~/.cache/huggingface`. After that it loads in about 20 ms, and the server uses about 90 MB of memory. If you built with the reranker, it (about 1.1 GB) downloads only on the first `precise: true` call. If a model can't be downloaded, for example offline, ranking falls back to keyword matching instead of failing.

### Updating

Run the installer command again, or `cargo install --git https://github.com/heyyviv/web-shooter --force` if you built from source.

Running sessions keep the old binary: reconnect with `/mcp` in Claude Code, or restart your client.

### Removing

```sh
claude mcp remove --scope user web-shooter
```

This only unregisters it. To free the disk space too, delete the binary (`rm ~/.cargo/bin/web-shooter`) and the cached models:

```sh
rm -rf ~/.cache/huggingface/hub/models--minishlab--potion-base-8M \
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
| `WEB_SHOOTER_RERANK_MODEL` | A different reranker for `precise` (only with `--features rerank`): `bge-base`, `bge-m3` or `jina-turbo` |
| `HF_HOME` | Where models are cached (default `~/.cache/huggingface`) |
| `WEB_SHOOTER_CHROME` | The Chrome or Chromium executable `render` uses (default: found in the usual install locations) |

## What it can't read yet

`fetch` does not run JavaScript, so pages that load their content from a separate API after the page arrives come back empty (Oracle HCM career sites, many search-results pages); `render` reads those in headless Chrome. It needs Chrome (or Chromium, Edge, Brave) installed; nothing is downloaded. Chrome starts on the first `render` call, runs with a fresh temporary profile that is deleted when it closes, and closes after 2 idle minutes. Sites behind bot checks such as Cloudflare are reported as blocked: web-shooter doesn't get past them.

Scanned PDFs (images with no text layer) need OCR, which web-shooter doesn't do; `fetch` says so. Tables in PDFs come out as markdown tables (complex ones imperfectly); equations come out as plain text in reading order.

## Benchmark

Single run, live network, 2026-10-31 ([`examples/bench.rs`](examples/bench.rs)), on the 32 eval questions (web pages, JSON APIs and 12 PDFs):

| Tool | Median latency | Avg tokens returned | Answer present |
| --- | --- | --- | --- |
| web-shooter `fetch` with a query | **793 ms** | **1,081** | 29/32 |
| Firecrawl, cache allowed | 1,207 ms (1.5× slower) | 25,409 | 28/32 |
| Firecrawl, fresh fetch | 1,548 ms (2× slower) | 25,025 | 28/32 |
| Jina Reader | 2,046 ms (2.6× slower) | 25,345 | 24/32 |

Six PDF questions check tables and exponents in web-shooter's markdown format (`| No NSP | 83.9 |`, `10^−9`), which favours it: Firecrawl also writes PDF tables as markdown and passed 4 of them, Jina none. On long PDFs (IRS, NIST) cached Firecrawl is faster, since web-shooter downloads the whole file on every call. The first run (2026-10-11, 15 questions) had web-shooter at 405 ms, Jina 625 ms and Firecrawl 1,214 ms (3,341 ms fresh).

### Held-out check

[`eval/heldout.json`](eval/heldout.json) has 8 questions on pages that were not used while tuning (Rust Book, MDN, PEP 8, Wikipedia, Effective Go, SQLite, Paul Graham, Docker docs). Run with `-- --heldout`.

| | Result |
| --- | --- |
| Answer present in output | 8/8 |
| Best-scored passage contains the answer | 6/8 |
| Avg tokens returned | 1,134 (Jina Reader: about 13,700) |
| Median latency, 3 live runs | 434, 438 and 658 ms (Jina Reader: 688, 697 and 2,110 ms) |

The two misses (a Paul Graham essay and the Docker best-practices page) still had the answer in the output, but not in the top-scored passage. Firecrawl was not run on these cases. Latency swings a lot between runs, so treat the speed difference with Jina as roughly even to somewhat faster, not a fixed multiple.

Caveats: Firecrawl and Jina return whole pages, so they always contain the answer. The 15 original questions were used for tuning, so the held-out numbers are the more honest ones, but 8 cases is a small sample. None of these pages needed JavaScript, which is where Firecrawl and Jina are strongest.

## Development

```sh
cargo test                                            # unit tests
cargo run --release --example eval                    # ranking test set (eval/cases.json)
cargo run --release --example eval -- --heldout       # cases kept out of tuning (eval/heldout.json)
cargo run --release --example probe -- <url> final    # markdown extracted from one page
cargo run --release --example probe -- <url> rank "<query>"   # top passages with scores
FIRECRAWL_API_KEY=… cargo run --release --example bench       # benchmark (uses Firecrawl credits)
```

[`docs/how-it-works.md`](docs/how-it-works.md) explains the pipeline, crawling, ranking and data handling in more depth. `CLAUDE.md` records the design decisions and the reasons behind them.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
