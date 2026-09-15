<p align="center">
  <img src="docs/crw-camofox.png" alt="crw-camofox" width="220" />
</p>

<h1 align="center">crw-camofox</h1>

<p align="center">
  Self-hosted, Rust-native web crawler &amp; scraper for AI agents
</p>

<p align="center">
  <a href="https://github.com/adambenhassen/crw-camofox/actions/workflows/ci.yml"><img src="https://github.com/adambenhassen/crw-camofox/actions/workflows/ci.yml/badge.svg?branch=main&event=push" alt="CI"></a>
  <a href="https://github.com/adambenhassen/crw-camofox/releases"><img src="https://img.shields.io/github/v/release/adambenhassen/crw-camofox?sort=semver&color=blue" alt="Latest release"></a>
  <a href="https://github.com/adambenhassen/crw-camofox/pkgs/container/crw-camofox"><img src="https://img.shields.io/badge/ghcr.io-crw--camofox-2496ED?logo=docker&logoColor=white" alt="Docker image"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://github.com/adambenhassen/crw-camofox/stargazers"><img src="https://img.shields.io/github/stars/adambenhassen/crw-camofox?style=social" alt="GitHub Stars"></a>
</p>

The open-source alternative to Firecrawl: one static Rust binary, ~50 MB RAM
idle, a Firecrawl-compatible REST API on **both `/v1/*` and `/v2/*`** (scrape,
crawl, map, search, extract, plus v2 batch & parse) — a drop-in for the official
Firecrawl SDKs — plus first-class MCP. This fork of [`crw`](https://github.com/us/crw)
swaps the browser layer to [Camofox](https://github.com/redf0x1/camofox-browser)
(Firefox anti-detect) and re-backs search on it — details
[below](#-this-is-the-camofox-fork). Self-host free under AGPL-3.0; upstream offers
a managed API at `api.fastcrw.com`, this fork is self-host only.

Works with Claude Code, Cursor, Windsurf, Cline, Copilot, Continue.dev, Codex and Gemini CLI — setup per client in [`docs/docs/mcp-clients.md`](docs/docs/mcp-clients.md).

---

## 🦊 This is the Camofox fork

Camofox is the Camoufox/Firefox anti-detect browser, driven over its REST API.
Changes vs. upstream — all **additive and config-toggled**:

| Area | Upstream `crw` | This fork |
|------|----------------|-----------|
| Default JS render ladder | `HTTP → LightPanda → Chrome` (CDP) | `HTTP → LightPanda → Camofox` (Firefox) `→ Byparr` (Cloudflare challenges only) |
| Stealth tier | browserless Chromium (SSPL); opt-in in-process Camoufox tier | **Camofox** ([camofox-browser](https://github.com/redf0x1/camofox-browser), REST-driven) — engine-level fingerprint evasion, the default tier, shared by render *and* search |
| `/v1/search` backend | SearXNG sidecar | **8 engines built in** — Google, Bing, DuckDuckGo, Wikipedia, YouTube, Reddit, Amazon, GitHub (no sidecar) |
| Interactive MCP | `crw-browse` (CDP, 2 tools) | Upstream **[`camofox-mcp`](https://github.com/redf0x1/camofox-mcp)** wired into the Docker stack — 47 tools over Camofox REST |

**What sets this fork apart:**

- **One browser for rendering and search.** `/v1/search` runs Google through the same
  Camofox browser that renders pages, so `docker compose up` gives working search with
  no SearXNG sidecar to deploy, version or keep healthy. Upstream's opt-in Camoufox tier
  is a renderer only; its search still needs the sidecar.
- **Many engines, one ranked list.** Search defaults to Google but can query up to four of
  Google, Bing, DuckDuckGo, Wikipedia, YouTube, Reddit, Amazon, and GitHub in a single call
  (run sequentially, so latency scales with engine count), deduping by URL and agreement-ranking
  the merged results.
- **Cloudflare challenges get cleared.** The Camofox tab waits out a "Just a moment"
  interstitial (`renderer.camofox.challenge_wait_ms`, default 20 s). When a Turnstile
  checkbox remains, the ladder hands the page to a bundled
  [Byparr](https://github.com/ThePhaseless/Byparr) solver, which clicks it. The
  `cf_clearance` cookie either tier earns is cached per host, so later scrapes of that host
  go out over plain HTTP (about 1 s instead of a browser render).

In production this fork backs the Hermes agent over MCP, with Hermes' native `web` and
`browser` tools disabled; the Camofox search backend returns results where the SearXNG
sidecar came back empty, and the render tier loads pages behind bot checks.

---

## Quickstart

Self-host the full stack with one command — no auth:

```bash
docker compose up -d        # crw + lightpanda + camofox + byparr + camofox-mcp
```

This brings up the REST API on `localhost:3000` plus the real render ladder
(HTTP → LightPanda → Camofox, then Byparr for Cloudflare challenges) and
Camofox-driven search, so JS-heavy pages, challenge-walled pages and web search
work out of the box. Set `CRW_HOST_PORT` and `CRW_BIND_ADDRESS` in `.env` to
change the published port or bind to `127.0.0.1` only.

First request:

```bash
curl -X POST http://localhost:3000/v1/scrape \
  -H "Content-Type: application/json" \
  -d '{"url": "https://example.com", "formats": ["markdown"], "onlyMainContent": true}'
```

```json
{
  "success": true,
  "data": {
    "markdown": "# Example Domain\n\nThis domain is for use in illustrative examples...",
    "metadata": { "title": "Example Domain", "sourceURL": "https://example.com", "statusCode": 200 }
  }
}
```

The full REST surface (`/v1/*` + `/v2/*`) is listed under
[API endpoints](#api-endpoints) below. Configuration (auth, proxies, render
ladder, search engines) lives in [`config.default.toml`](config.default.toml)
and [`docs/docs/configuration.md`](docs/docs/configuration.md).

### MCP

The Compose stack exposes two MCP servers over Streamable HTTP — point any
MCP agent (Claude Code, Cursor, Windsurf, Cline, Continue.dev, Codex, Gemini CLI)
at whichever fits the job:

**Scraping** — `crw`'s own `/mcp`, 6 tools to *fetch* pages (`crw_scrape`,
`crw_crawl`, `crw_check_crawl_status`, `crw_map`, `crw_search`, `crw_parse_file`):

```bash
claude mcp add --transport http crw http://localhost:3000/mcp
```

**Interactive browser** — for agents that must *operate* a site (log in, fill
forms, click through flows), the upstream
[`camofox-mcp`](https://github.com/redf0x1/camofox-mcp) server drives a live
[Camofox](https://github.com/redf0x1/camofox-browser) (Firefox) browser: 47 tools
(navigate, click, type, scroll, evaluate, screenshot, cookies, …). It's a separate
server on `localhost:9378` and needs a bearer token; the stack ships a
loopback-only dev key:

```bash
claude mcp add --transport http camofox http://localhost:9378/mcp \
  --header "Authorization: Bearer crw-local-dev-insecure-default-key"
```

> [!WARNING]
> These tools drive a real browser. Before exposing port 9378 beyond localhost,
> set your own `CAMOFOX_HTTP_API_KEY` in `.env` (≥32 chars, e.g.
> `openssl rand -hex 24`) and use that token instead of the dev key.

### Agent skills

Drop-in [Agent Skills](https://docs.claude.com/en/docs/claude-code/skills) that teach an
agent when to use each tool suite live in [`skills/`](skills/):

- [**`crw-web`**](skills/crw-web/SKILL.md) — the crw tools (scrape / search / crawl / map / parse): when to use each, `crw_search` engine selection, and output limits.
- [**`camofox-browser`**](skills/camofox-browser/SKILL.md) — the camofox-mcp interactive browser: the full tool reference and the "escalate only for real interactivity" rule. Requires the `camofox-mcp` server.

---

## Why crw-camofox?

- **Rust-native engine** — the core is one static Rust binary (no Redis, Node.js, or Python). The Camofox (Firefox) browser runs as a separate container, pulled in only for JS rendering, stealth, and search — plain HTTP fetches never touch it.
- **Light idle footprint** — the engine idles around ~50 MB; the Camofox browser only spins up for heavy renders. Browser-render-first stacks (Firecrawl, Crawl4AI) carry a Chromium heap baseline measured in hundreds of MB before a single request lands.
- **Firecrawl-compatible drop-in** — both the `/v1/*` and `/v2/*` surfaces (scrape, crawl, map, search, extract; plus v2-only batch & parse) with compatible request/response shapes. The v2 API is a drop-in for the official `firecrawl-py` v4 SDK (`FirecrawlApp(api_url="http://localhost:3000")`) — swap the base URL and keep your code.
- **Change tracking** — diff a page against a prior snapshot (markdown git-diff, per-field JSON, or both) with an optional LLM "meaningful-change" judge. A stateless `changeTracking` primitive in the engine — wire it into your own scheduler. See [`docs/docs/monitoring.md`](docs/docs/monitoring.md).
- **AGPL-3.0, self-host only** — run the whole stack yourself under AGPL-3.0. This fork operates no managed tier: no account and no usage metering. Bearer auth is optional and yours to configure.

Against upstream `crw` and the three most-cited alternatives — descriptive
shape, not a benchmark:

| | **crw-camofox** | fastCRW (upstream) | Firecrawl | Crawl4AI | Spider |
|---|---|---|---|---|---|
| Language | Rust | Rust | Node.js + Playwright | Python + Playwright | Rust |
| License | AGPL-3.0 | AGPL-3.0 (commercial avail.) | AGPL-3.0 (commercial avail.) | Apache-2.0 | Source-available / commercial ([spider.cloud](https://spider.cloud)) |
| Self-host footprint | Static binary + Camofox container (+ Byparr challenge solver) | Static binary + browser + SearXNG sidecar | Multi-container | Single large image (browser bundled) | Managed-first; self-host via crate |
| Memory baseline (idle) | ~50 MB | ~50 MB | Large (Chromium heap) | Large (Chromium heap) | Light (Rust) |
| Stealth tier | **Anti-detect by default** (Camofox/Firefox) | browserless Chromium (SSPL), opt-in | Playwright Chromium | Playwright Chromium | — |
| Search backend | **8 engines** (Google, Bing, DuckDuckGo, Wikipedia, YouTube, Reddit, Amazon, GitHub) | SearXNG sidecar | Built-in | Built-in | Built-in |
| Firecrawl-compat API | Yes — **v1 + v2** | Yes — **v1 + v2** | Native | No | No |
| MCP server | `crw-mcp` **+ 47** interactive-browser tools | `crw-mcp` only | Separate package | Community add-on | No first-party |
| Hosted option | Self-host | `api.fastcrw.com` | firecrawl.dev | None official | spider.cloud (primary product) |

---

## API endpoints

| Method | Endpoint | Description |
|---|---|---|
| `POST` | `/v1/scrape` | Scrape a single URL, optionally with LLM extraction or summary |
| `POST` | `/v1/crawl` | Start async BFS crawl (returns job ID) |
| `GET` | `/v1/crawl/:id` | Check crawl status and retrieve results |
| `DELETE` | `/v1/crawl/:id` | Cancel a running crawl job |
| `POST` | `/v1/map` | Discover all URLs on a site |
| `POST` | `/v1/search` | Web search via Camofox-driven engines (Google default; 8 selectable), with optional content scraping |
| `GET` | `/v1/search/research/papers` | Paper search over Camofox web search merged with OpenAlex and Semantic Scholar; `.../papers/:id` and `.../papers/:id/similar` |
| `GET` | `/v1/search/research/github` | Repository search through the GitHub engine |
| `POST` | `/v1/change-tracking/diff` | Diff a scrape against a supplied snapshot (the [monitoring](docs/docs/monitoring.md) primitive) — single or batch |
| `GET` | `/v1/capabilities` | Feature and limit discovery |
| `GET` | `/health`, `/ready`, `/openapi.json` | Liveness, readiness and schema (no auth required) |
| `GET` | `/metrics` | Prometheus metrics (behind the API-key boundary when `[auth].api_keys` is set) |
| `POST` | `/mcp` | Streamable HTTP MCP transport |

**Firecrawl v2 surface** — `scrape`, `crawl`, `map`, `search` are also served under `/v2/*` with Firecrawl v2 request/response shapes, plus v2-only `POST /v2/extract` (async structured JSON via JSON Schema; poll `GET /v2/extract/:id`), `POST /v2/batch/scrape`, `POST /v2/parse` (PDF/doc → markdown), and `GET /v2/crawl/active`. `GET /v2/crawl/:id/errors` and `GET /v2/batch/scrape/:id/errors` list each failed URL with its reason; failed pages also stay in the results as documents marked `block` and are counted in `blocked`. This makes the official `firecrawl-py` v4 SDK a drop-in: `FirecrawlApp(api_url="http://localhost:3000")`.

Full reference in [`docs/docs/rest-api.md`](docs/docs/rest-api.md).
The Firecrawl compatibility matrix (field-by-field diff) lives in
[`COMPATIBILITY-firecrawl.md`](COMPATIBILITY-firecrawl.md).

---

## Security

- **SSRF protection** — blocks loopback, private IPs, cloud metadata (`169.254.x.x`), IPv6 mapped addresses, and non-HTTP schemes (`file://`, `data:`). The browser tiers check every outbound request, Camofox refuses pages that end on an internal address, and a per-request LLM `baseUrl` pointing at a private address is rejected
- **Auth** — optional Bearer token with constant-time comparison; `/metrics` and `/admin/*` sit inside the same boundary
- **CORS** — off by default; list browser origins in `server.cors_allowed_origins`
- **Proxies** — a malformed proxy URL fails startup (or returns 400 per request) instead of sending traffic directly
- **robots.txt** — RFC 9309 compliant with wildcard patterns
- **Rate limiting** — token-bucket algorithm, returns 429 with `error_code`
- **Resource limits** — max request body 1 MB; per-crawl depth and page count bounded (configurable; defaults: depth 2, 100 pages)

[Full hardening guide →](docs/docs/self-hosting-hardening.md)

---

## Build from source

This fork is distributed as the multi-arch Docker image
**`ghcr.io/adambenhassen/crw-camofox`** (`linux/amd64` + `linux/arm64`) used by the
Compose stack above; upstream's `npm`/`pip`/`brew`/`cargo`/`apt` packages are
**not** this fork (they default to Chrome + SearXNG). To build the binaries yourself:

```bash
git clone https://github.com/adambenhassen/crw-camofox
cd crw-camofox
cargo build --release -p crw-server --features cdp,camofox -p crw-mcp -p crw-cli
```

---

## Contributing

Contributions are welcome — issues and PRs both.

1. Fork the repository
2. Install pre-commit hooks: `make hooks`
3. Create your feature branch (`git checkout -b feat/my-feature`)
4. Commit your changes (`git commit -m 'feat: add my feature'`)
5. Push to the branch (`git push origin feat/my-feature`)
6. Open a Pull Request

The pre-commit hook runs the same checks as CI (`cargo fmt`, `cargo clippy`,
`cargo test`). Run manually with `make check`.

<a href="https://github.com/adambenhassen/crw-camofox/graphs/contributors">
  <img alt="contributors" src="https://contrib.rocks/image?repo=adambenhassen/crw-camofox"/>
</a>

---

## License

crw-camofox is open source under [AGPL-3.0](LICENSE). If you embed it in a
closed-source product or expose it as a hosted service to third parties,
AGPL's source-availability requirements apply to your deployment. This fork
is community-maintained and self-host only — it offers no managed tier or
commercial carve-out; for commercial licensing, see [upstream `crw`](https://github.com/us/crw).

---

**It is the sole responsibility of end users to respect websites' policies
when scraping.** Users are advised to adhere to applicable privacy
policies and terms of use. By default, crw-camofox respects `robots.txt`
directives.
