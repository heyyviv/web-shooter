# web-shooter

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)

A fast web reader for AI assistants, served over [MCP](https://modelcontextprotocol.io). It reads web pages and PDFs and returns only the passages that answer the question, not whole pages. It runs locally and keeps nothing.

## Why it's better

Live benchmark on 32 questions (web pages, JSON APIs, PDFs), 2026-10-31:

| Tool | Median latency | Avg tokens returned | Answer present |
| --- | --- | --- | --- |
| **web-shooter** | **793 ms** | **1,081** | **29/32** |
| Firecrawl, cache allowed | 1,207 ms | 25,409 | 28/32 |
| Firecrawl, fresh | 1,548 ms | 25,025 | 28/32 |
| Jina Reader | 2,046 ms | 25,345 | 24/32 |

Faster, and about **23× fewer tokens**, with the same accuracy. Caveats and the held-out check are in [docs/guide.md](docs/guide.md#benchmark-details).

## Tools

| Tool | Use it when | Returns |
| --- | --- | --- |
| `fetch(urls, query?, keywords?, max_tokens?)` | You have URLs (or local PDF paths) | The best passages, grouped by page; whole pages without a query |
| `links(url, query?, keywords?, max_links?)` | You want to choose which page to read next | The page's links, ranked by the query |
| `crawl(url, query, keywords?, max_pages?, max_depth?, other_sites?, max_results?)` | You have a start page and a goal, not the URLs | The most relevant pages, as URLs with one-line snippets |
| `render(urls, query?, keywords?, max_tokens?)` | `fetch` finds no content: the page is built by JavaScript | Like `fetch`, after loading the page in headless Chrome |

## Setup

**1. Install** (macOS on Apple Silicon, Linux, Windows):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/heyyviv/web-shooter/releases/latest/download/web-shooter-installer.sh | sh
```

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/heyyviv/web-shooter/releases/latest/download/web-shooter-installer.ps1 | iex"
```

Or from source (Rust 1.88+, and the only route on Intel Macs):

```sh
cargo install --git https://github.com/heyyviv/web-shooter
```

**2. Register it.** In Claude Code:

```sh
claude mcp add --scope user web-shooter -- web-shooter
```

Then run `/mcp` or start a new session. For Claude Desktop, Cursor and other clients, add a stdio server with the absolute path to the binary:

```json
{ "mcpServers": { "web-shooter": { "command": "/absolute/path/to/web-shooter" } } }
```

The first query downloads a 30 MB embedding model. `render` needs Chrome installed.

## More

[docs/guide.md](docs/guide.md) covers usage, configuration, updating, limits and development. [docs/how-it-works.md](docs/how-it-works.md) explains the internals.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
