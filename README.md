<p align="center">
  <img src="docs/crw-camofox.png" alt="crw-camofox" width="220" />
</p>

<h1 align="center">crw-camofox</h1>

<p align="center">
  The self-hosted web scraper that gets past bot walls.<br/>
  One Rust binary, a Firefox that fingerprints like a human, and a Firecrawl-compatible API.
</p>

<p align="center">
  <a href="https://github.com/adambenhassen/crw-camofox/actions/workflows/ci.yml"><img src="https://github.com/adambenhassen/crw-camofox/actions/workflows/ci.yml/badge.svg?branch=feat%2Fcamofox-renderer&event=push" alt="CI"></a>
  <a href="https://github.com/adambenhassen/crw-camofox/releases"><img src="https://img.shields.io/github/v/release/adambenhassen/crw-camofox?sort=semver&color=blue" alt="Latest release"></a>
  <a href="https://github.com/adambenhassen/crw-camofox/pkgs/container/crw-camofox"><img src="https://img.shields.io/badge/ghcr.io-crw--camofox-2496ED?logo=docker&logoColor=white" alt="Docker image"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue.svg" alt="License"></a>
  <a href="https://github.com/adambenhassen/crw-camofox/stargazers"><img src="https://img.shields.io/github/stars/adambenhassen/crw-camofox?style=social" alt="GitHub Stars"></a>
</p>

Scrape, crawl, map, search and extract from one `docker compose up`. Pages
behind Cloudflare, sites that block headless Chrome, and web search all work
out of the box, and every result comes back as clean markdown or JSON through
the same `/v1` and `/v2` API the Firecrawl SDKs already speak. Point any MCP
agent at it (Claude Code, Cursor, Windsurf, Cline, Codex, Gemini CLI) and it
gets six scraping tools plus a 47-tool interactive browser.

This is a fork of [`crw`](https://github.com/us/crw) that swaps the browser
layer for [Camofox](https://github.com/redf0x1/camofox-browser) and re-backs
search on it. Self-host free under AGPL-3.0; there is no managed tier.

---

## 🦊 This is the Camofox fork

[Camofox](https://github.com/redf0x1/camofox-browser) wraps
[Camoufox](https://camoufox.com), a Firefox fork with fingerprint spoofing in
the C++ engine rather than injected JavaScript, behind a REST API. This fork
makes it the default browser for both rendering and search. Everything below
is additive and config-toggled:

| Area | Upstream `crw` | This fork |
|------|----------------|-----------|
| JS render ladder | `HTTP → Chrome-impersonated HTTP → LightPanda → Chrome` (CDP) | `HTTP → LightPanda → Camofox`, then **Byparr** for Cloudflare challenges |
| Stealth tier | browserless Chromium (SSPL); a Camofox tier exists but is opt-in and stays out of the auto ladder | **Camofox** on the auto ladder by default, shared by render *and* search |
| `/v1/search` | SearXNG sidecar | **8 engines built in**: Google, Bing, DuckDuckGo, Wikipedia, YouTube, Reddit, Amazon, GitHub |
| Interactive MCP | `crw-browse` (CDP, 2 tools) | [`camofox-mcp`](https://github.com/redf0x1/camofox-mcp) in the Compose stack: **47 tools** over the same browser |
| Cloudflare | — | Waits out "Just a moment", hands Turnstile to [Byparr](https://github.com/ThePhaseless/Byparr), caches `cf_clearance` per host |

**How a scrape moves through the ladder:**

```mermaid
flowchart LR
    A[HTTP fetch] -->|needs JS| B[LightPanda]
    B -->|blocked or thin| C[Camofox<br/>Firefox anti-detect]
    C -->|Turnstile checkbox| D[Byparr solver]
    C -->|cleared| E[cf_clearance cached per host]
    D -->|cleared| E
    E -.->|next scrape of that host<br/>skips the browser| A
```

- **One browser, no sidecar.** Search runs Google through the Camofox tab that
  already renders your pages. `docker compose up` gives working search with no
  SearXNG to deploy, version or keep healthy.
- **Eight engines, one ranked list.** Query up to four of the eight engines in a
  single call. Results are deduped by URL and ranked by how many engines agree.
  Engines run sequentially on one warm tab, so latency scales with engine count.
- **Cloudflare gets cleared, then cached.** The Camofox tab waits up to 20 s for
  a "Just a moment" interstitial. If a Turnstile checkbox remains, Byparr clicks
  it. Either way the `cf_clearance` cookie is cached per host, and later scrapes
  of that host go out over plain HTTP in about a second instead of a render.
- **Failures are reported, not hidden.** Origin error pages, parking pages and
  vendor block pages served as `200` fail with a reason instead of shipping as
  content. Crawl and batch keep failed pages as documents marked `block`.

In production this fork backs the Hermes agent over MCP with Hermes' native
`web` and `browser` tools disabled. The Camofox search backend returns results
where the SearXNG sidecar came back empty, and the render tier loads pages
behind bot checks that the previous stack could not.

---

## Why crw-camofox?

- **Rust-native engine.** One static binary, no Redis, Node.js, Python or
  message broker. The browser is a separate container that only wakes up for
  JS rendering, stealth and search; plain HTTP fetches never touch it.
- **Light when idle.** The engine idles around 50 MB. LightPanda, the first
  browser tier, is a from-scratch headless browser written in Zig that
  [claims](https://github.com/lightpanda-io/browser) about 16× less memory than
  Chrome. Browser-first stacks carry a Chromium heap before the first
  request lands.
- **Firecrawl drop-in.** Both `/v1/*` and `/v2/*` with Firecrawl request and
  response shapes. The official `firecrawl-py` v4 SDK works as is:
  `Firecrawl(api_key="any", api_url="http://localhost:3000")`.
- **Change tracking.** Diff a page against a prior snapshot (markdown git-diff,
  per-field JSON, or both) with an optional LLM "meaningful change" judge. A
  stateless primitive you wire into your own scheduler. See
  [`docs/docs/monitoring.md`](docs/docs/monitoring.md).
- **Yours to run.** AGPL-3.0, no account, no metering, no phone-home. Bearer
  auth, proxies, rate limits and the render ladder are all yours to configure.

Against upstream `crw` and the three most-cited alternatives. Descriptive
shape, not a benchmark:

| | **crw-camofox** | fastCRW (upstream) | Firecrawl | Crawl4AI | Spider |
|---|---|---|---|---|---|
| Language | Rust | Rust | Node.js + Playwright | Python + Playwright | Rust |
| License | AGPL-3.0 | AGPL-3.0, commercial available | AGPL-3.0, commercial available | Apache-2.0 | MIT crate; [spider.cloud](https://spider.cloud) managed |
| Self-host shape | Static binary + Camofox container (+ Byparr) | Static binary + browser + SearXNG sidecar | Six containers: api, worker, playwright, redis, postgres, rabbitmq | One image with Playwright browsers bundled | Rust crate, no service |
| Stealth tier | **Anti-detect by default** (Camofox/Firefox) | browserless Chromium; Camofox opt-in, off the auto ladder | Playwright Chromium | Playwright Chromium | — |
| Web search | **8 engines**, no sidecar | SearXNG sidecar | Built-in | — | Via spider.cloud |
| Firecrawl-compat API | **v1 + v2** | v1 + v2 | Native | No | No |
| MCP server | `crw-mcp` **+ 47** interactive-browser tools | `crw-mcp` | Separate package | Bundled in the Docker image | `spider_mcp` crate |
| Hosted option | None, self-host only | `api.fastcrw.com` | firecrawl.dev | None official | spider.cloud |

---

## Quickstart

```bash
git clone https://github.com/adambenhassen/crw-camofox && cd crw-camofox
docker compose up -d        # crw + lightpanda + camofox + byparr + camofox-mcp
```

That is the whole stack: the REST API on `localhost:3000`, the full render
ladder, Cloudflare solving and search. No auth by default. Set `CRW_HOST_PORT`
and `CRW_BIND_ADDRESS` in `.env` to change the published port or bind to
`127.0.0.1` only.

**Scrape a page:**

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

**Search the web, three engines at once:**

```bash
curl -X POST http://localhost:3000/v1/search \
  -H "Content-Type: application/json" \
  -d '{"query": "rust async runtime", "engines": ["google", "duckduckgo", "github"], "limit": 5}'
```

**Or use the Firecrawl SDK you already have:**

```python
from firecrawl import Firecrawl

fc = Firecrawl(api_key="any", api_url="http://localhost:3000")
doc = fc.scrape("https://example.com", formats=["markdown"])
```

Configuration (auth, proxies, render ladder, search engines) lives in
[`config.default.toml`](config.default.toml) and
[`docs/docs/configuration.md`](docs/docs/configuration.md). The full REST
surface is under [API endpoints](#api-endpoints).

### MCP

The Compose stack exposes two MCP servers over Streamable HTTP. Setup per
client (Claude Code, Cursor, Windsurf, Cline, Copilot, Continue.dev, Codex,
Gemini CLI) is in [`docs/docs/mcp-clients.md`](docs/docs/mcp-clients.md).

**Scraping**: `crw`'s own `/mcp`, six tools to *fetch* pages (`crw_scrape`,
`crw_crawl`, `crw_check_crawl_status`, `crw_map`, `crw_search`, `crw_parse_file`):

```bash
claude mcp add --transport http crw http://localhost:3000/mcp
```

**Interactive browser**: for agents that must *operate* a site (log in, fill
forms, click through flows), [`camofox-mcp`](https://github.com/redf0x1/camofox-mcp)
drives a live Camofox browser with 47 tools (navigate, click, type, scroll,
evaluate, screenshot, cookies, sessions, batch). It runs on `localhost:9378`
and needs a bearer token; the stack ships a loopback-only dev key:

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

- [**`crw-web`**](skills/crw-web/SKILL.md): the crw tools (scrape / search / crawl / map / parse), when to use each, `crw_search` engine selection, and output limits.
- [**`camofox-browser`**](skills/camofox-browser/SKILL.md): the camofox-mcp interactive browser, the full tool reference and the "escalate only for real interactivity" rule. Requires the `camofox-mcp` server.

---

## Security

- **SSRF protection**: blocks loopback, private IPs, cloud metadata (`169.254.x.x`), IPv6-mapped addresses and non-HTTP schemes (`file://`, `data:`). The browser tiers check every outbound request, Camofox refuses pages that end on an internal address, and a per-request LLM `baseUrl` pointing at a private address is rejected.
- **Auth**: optional Bearer token with constant-time comparison. `/metrics` and `/admin/*` sit inside the same boundary.
- **CORS**: off by default. List browser origins in `server.cors_allowed_origins`.
- **Proxies**: a malformed proxy URL fails startup, or returns 400 per request, instead of sending traffic directly.
- **robots.txt**: RFC 9309 compliant with wildcard patterns.
- **Rate limiting**: token bucket, returns 429 with an `error_code`.
- **Resource limits**: 1 MB request body; per-crawl depth and page count bounded (defaults: depth 2, 100 pages).

[Full hardening guide →](docs/docs/self-hosting-hardening.md)

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
| `POST` | `/v1/change-tracking/diff` | Diff a scrape against a supplied snapshot (the [monitoring](docs/docs/monitoring.md) primitive), single or batch |
| `GET` | `/v1/capabilities` | Feature and limit discovery |
| `GET` | `/health`, `/ready`, `/openapi.json` | Liveness, readiness and schema (no auth required) |
| `GET` | `/metrics` | Prometheus metrics (behind the API-key boundary when `[auth].api_keys` is set) |
| `POST` | `/mcp` | Streamable HTTP MCP transport |

**Firecrawl v2 surface**: `scrape`, `crawl`, `map`, `search` are also served under `/v2/*` with Firecrawl v2 request and response shapes, plus v2-only `POST /v2/extract` (async structured JSON via JSON Schema; poll `GET /v2/extract/:id`), `POST /v2/batch/scrape`, `POST /v2/parse` (PDF/doc → markdown), and `GET /v2/crawl/active`. `GET /v2/crawl/:id/errors` and `GET /v2/batch/scrape/:id/errors` list each failed URL with its reason; failed pages also stay in the results as documents marked `block` and are counted in `blocked`.

Full reference in [`docs/docs/rest-api.md`](docs/docs/rest-api.md). The
Firecrawl compatibility matrix (field-by-field diff) lives in
[`COMPATIBILITY-firecrawl.md`](COMPATIBILITY-firecrawl.md).

---

## Build from source

This fork ships as the multi-arch Docker image
**`ghcr.io/adambenhassen/crw-camofox`** (`linux/amd64` + `linux/arm64`) used by
the Compose stack above. Upstream's installer, `pip`, `npm` and `cargo`
packages are **not** this fork; they default to Chrome and SearXNG. To build
the binaries yourself:

```bash
git clone https://github.com/adambenhassen/crw-camofox
cd crw-camofox
cargo build --release -p crw-server --features cdp,camofox -p crw-mcp -p crw-cli
```

---

## Contributing

Contributions are welcome, issues and PRs both.

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
is community-maintained and self-host only; it offers no managed tier or
commercial carve-out. For commercial licensing, see [upstream `crw`](https://github.com/us/crw).

---

**It is the sole responsibility of end users to respect websites' policies
when scraping.** Users are advised to adhere to applicable privacy
policies and terms of use. By default, crw-camofox respects `robots.txt`
directives.
