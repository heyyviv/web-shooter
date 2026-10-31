# web-shooter guide

Everything beyond setup: usage, configuration, maintenance, limits, benchmark details and development. How the pipeline works is in [how-it-works.md](how-it-works.md).

## Example

Asking one Rust Book page "what are the rules of ownership in rust" returns this, not the whole chapter:

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

## What the tools read

`fetch` also reads PDFs, from a URL or a local path (`/path/paper.pdf`, `~/…`, `file://…`): headings are recovered from font sizes and section numbers, two-column papers are read column by column, running headers and footers are dropped, tables become markdown tables, exponents and subscripts are kept (`10^20`, `d_k`), and each passage is labelled with its page (`§ 5.3 Log replication · p.7`). It reads JSON API responses (one section per record) and the data embedded in JavaScript-rendered pages too.

`crawl` respects robots.txt, stays on the start site by default, fetches 4 pages at a time and stops after 30 pages or 30 seconds by default.

`render` needs Chrome (or Chromium, Edge, Brave) installed; nothing is downloaded. Chrome starts on the first `render` call, runs with a fresh temporary profile that is deleted when it closes, and closes after 2 idle minutes. It reads only http(s) pages.

## Usage

Ask your assistant to use it by name. For example:

- "Use web-shooter `crawl` on https://blog.rust-lang.org with the query 'security vulnerabilities in cargo'."
- "Use web-shooter `fetch` on these 5 job posting URLs with the query 'visa sponsorship, location, years of experience'."
- "Use web-shooter `links` on this careers page with the query 'machine learning roles', then fetch the top 3."

Tips:

- Pass several URLs to one `fetch` call; they are fetched in parallel.
- Add `keywords` with exact terms or synonyms when the answer may not share words with the question (a city name, a product code).
- `crawl` returns URLs and snippets, not page text. Follow up with `fetch` on the pages you want.
- When `fetch` reports no readable content, try `render`.

## Configuration

| Environment variable | Effect |
| --- | --- |
| `WEB_SHOOTER_EMBED_MODEL` | A different model2vec embedding model (Hugging Face repo id or local folder) |
| `WEB_SHOOTER_RERANK_MODEL` | A different reranker for `precise` (only with `--features rerank`): `bge-base`, `bge-m3` or `jina-turbo` |
| `HF_HOME` | Where models are cached (default `~/.cache/huggingface`) |
| `WEB_SHOOTER_CHROME` | The Chrome or Chromium executable `render` uses (default: found in the usual install locations) |

## First run

The first query downloads the embedding model (`minishlab/potion-base-8M`, about 30 MB) into the Hugging Face cache, `~/.cache/huggingface`. After that it loads in about 20 ms, and the server uses about 90 MB of memory. If a model can't be downloaded, for example offline, ranking falls back to keyword matching instead of failing.

## Optional reranker

Build with `--features rerank` to add a `precise` option to `fetch`: a cross-encoder rescoring the top passages. It costs about 300 ms more per call and returns about a third fewer tokens, but ranked no better in our tests. It adds ONNX Runtime to the binary (about 20 MB) and a 1.1 GB model download on the first `precise: true` call.

## Updating

Run the installer command again, or `cargo install --git https://github.com/heyyviv/web-shooter --force` if you built from source.

Running sessions keep the old binary: reconnect with `/mcp` in Claude Code, or restart your client.

## Removing

```sh
claude mcp remove --scope user web-shooter
```

This only unregisters it. To free the disk space too, delete the binary (`rm ~/.cargo/bin/web-shooter`) and the cached models:

```sh
rm -rf ~/.cache/huggingface/hub/models--minishlab--potion-base-8M \
       ~/.cache/huggingface/hub/models--jinaai--jina-reranker-v2-base-multilingual
```

## What it can't read yet

- Sites behind bot checks such as Cloudflare: `render` reports them as blocked; web-shooter doesn't get past them.
- Content behind clicks: "load more" buttons, infinite scroll, pagination, logins. `render` loads and waits but doesn't click or scroll.
- Scanned PDFs (images with no text layer) need OCR, which web-shooter doesn't do; `fetch` says so.
- Complex tables (stacked headers, two tables side by side) come out imperfectly; equations come out as plain text in reading order.

## Benchmark details

The README's table is a single live run on 2026-10-31 ([`examples/bench.rs`](../examples/bench.rs)) over the 32 eval questions (web pages, JSON APIs and 12 PDFs). Answer present: web-shooter 29/32, Firecrawl 28/32 (both modes), Jina Reader 24/32 (2 pages failed to load).

- Firecrawl and Jina return whole pages, so the answer is nearly always somewhere in them; web-shooter returns ranked passages. Its other miss is "who designed Rust" (the page says "created").
- Six PDF questions check tables and exponents in web-shooter's markdown format (`| No NSP | 83.9 |`, `10^−9`), which favours it: Firecrawl also writes PDF tables as markdown and passed 4 of them, Jina none.
- On long PDFs (IRS, NIST) cached Firecrawl is faster, since web-shooter downloads the whole file on every call.
- The questions were used for tuning, and one run on a live network swings a lot.
- The first run (2026-10-11, 15 questions): web-shooter 405 ms, Jina 625 ms, Firecrawl 1,214 ms (3,341 ms fresh).

### Held-out check

[`eval/heldout.json`](../eval/heldout.json) has 15 questions (7 of them PDFs) on pages never used for tuning. Run with `cargo run --release --example eval -- --heldout`. On 2026-10-31: the answer was in the output for 14/15 and in the top-scored passage for 12/15, at about 1,400 tokens on average. The miss is Llama 2's learning rate: extracted correctly, but it didn't make the 2,000-token budget for a 77-page paper.

## Development

```sh
cargo test                                            # unit tests
cargo run --release --example eval                    # ranking test set (eval/cases.json)
cargo run --release --example eval -- --heldout       # cases kept out of tuning (eval/heldout.json)
cargo run --release --example probe -- <url> final    # markdown extracted from one page
cargo run --release --example probe -- <url> rank "<query>"   # top passages with scores
FIRECRAWL_API_KEY=… cargo run --release --example bench       # benchmark (uses Firecrawl credits)
```

`CLAUDE.md` records the design decisions and the reasons behind them.
