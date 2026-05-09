<div class="page-intro">
  <div class="page-kicker">Core Endpoint</div>
  <h1>Search</h1>
  <p class="page-subtitle">Search the web first, then optionally scrape the results you care about. Works out of the box on self-hosted CRW via the bundled SearXNG sidecar — no third-party API key needed. Free, self-hostable alternative to Tavily / Serper / Brave Search.</p>
  <div class="page-capabilities">
    <div class="page-capability"><strong>Best for:</strong> unknown URLs</div>
    <div class="page-capability"><strong>Self-hosted:</strong> bundled SearXNG sidecar</div>
    <div class="page-capability"><strong>Hosted:</strong> fastcrw.com (managed)</div>
    <div class="page-capability"><strong>Start with:</strong> search only, then add scraping</div>
  </div>
  <div class="page-actions">
    <a class="page-btn primary" href="https://fastcrw.com/playground" target="_blank" rel="noopener">Try it in the Playground</a>
    <a class="page-btn secondary" href="#scraping">View Scrape</a>
  </div>
</div>

<div class="playground-panel">
  <div class="playground-kicker">Try it in the Playground</div>
  <div class="playground-title">Start with result discovery only</div>
  <div class="playground-copy">Use a small query and <code>limit: 5</code> first. Add <code>scrapeOptions</code> only when you already know you need page content from those search results.</div>
  <div class="playground-actions">
    <a class="page-btn primary" href="https://fastcrw.com/playground" target="_blank" rel="noopener">Open Playground</a>
    <a class="page-btn secondary" href="https://fastcrw.com/register" target="_blank" rel="noopener">Get API Key</a>
  </div>
</div>

:::note
**Self-hosted users**: `docker compose up` boots a SearXNG sidecar automatically. `/v1/search` is live on `http://localhost:3000` with no extra setup. To point at an existing SearXNG instance instead, set `CRW_SEARCH__SEARXNG_URL=http://your-host:8080` and remove the `searxng` service from your compose file. To disable search entirely, set `[search].enabled = false` — the route returns a clear `search_disabled` error (HTTP 503).
:::

## Searching the web with CRW

### /v1/search

```http
POST http://localhost:3000/v1/search          # self-hosted
POST https://fastcrw.com/api/v1/search        # hosted
```

Authentication:

- Self-hosted: no auth by default (add a reverse proxy / API key middleware if you expose it publicly)
- Hosted: send `Authorization: Bearer YOUR_API_KEY`

### Installation

Like the rest of the CRW API, search is HTTP-first. Use cURL or your existing HTTP client.

### Basic usage

Start with this request:

```json
{
  "query": "web scraping tools",
  "limit": 5
}
```

:::tabs
::tab{title="Python"}
```python
import requests

# Self-hosted
resp = requests.post(
    "http://localhost:3000/v1/search",
    json={"query": "web scraping tools", "limit": 5},
)

# Or hosted (with API key)
# resp = requests.post(
#     "https://fastcrw.com/api/v1/search",
#     headers={"Authorization": "Bearer YOUR_API_KEY"},
#     json={"query": "web scraping tools", "limit": 5},
# )

for item in resp.json()["data"]:
    print(item["title"], item["url"])
```
::tab{title="Node.js"}
```javascript
const resp = await fetch("http://localhost:3000/v1/search", {
  method: "POST",
  headers: { "Content-Type": "application/json" },
  body: JSON.stringify({ query: "web scraping tools", limit: 5 })
});

const body = await resp.json();
console.log(body.data);
```
::tab{title="cURL"}
```bash
# Self-hosted (no auth)
curl -X POST http://localhost:3000/v1/search \
  -H "Content-Type: application/json" \
  -d '{"query": "web scraping tools", "limit": 5}'

# Hosted
curl -X POST https://fastcrw.com/api/v1/search \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query": "web scraping tools", "limit": 5}'
```
:::

### Response

```json
{
  "success": true,
  "data": [
    {
      "url": "https://example.com/article",
      "title": "Article Title",
      "description": "A snippet from the search result...",
      "position": 1,
      "score": 9.5
    }
  ]
}
```

That is the flat response shape used when `sources` is not set.

## Parameters

| Field | Type | Default | Description |
|---|---|---|---|
| `query` | string | required | Search query (1–2000 chars) |
| `limit` | number | `5` | Maximum results per source (max `20`) |
| `lang` | string | -- | Result language hint such as `"en"` or `"tr"` |
| `tbs` | string | -- | Recency filter: `qdr:h`, `qdr:d`, `qdr:w`, `qdr:m`, `qdr:y` |
| `sources` | string[] | -- | Result groups such as `"web"`, `"news"`, `"images"` |
| `categories` | string[] | -- | Filters such as `"github"`, `"research"`, `"pdf"` |
| `scrapeOptions` | object | -- | Scrape each result URL after search |

`scrapeOptions`:

| Field | Type | Default | Description |
|---|---|---|---|
| `formats` | string[] | `["markdown"]` | Allowed: `markdown`, `html`, `rawHtml`, `links`. `plainText` and `json` (extract) are not supported on `/v1/search` — use `/v1/scrape` for those |
| `onlyMainContent` | boolean | `true` | Keep content focused on the main body |

## Search result types

Without `sources`, CRW returns a flat list:

```json
{
  "success": true,
  "data": [
    {
      "url": "https://example.com/article",
      "title": "Article Title",
      "description": "Search snippet...",
      "position": 1,
      "score": 9.5
    }
  ]
}
```

With `sources`, CRW returns grouped results:

```json
{
  "success": true,
  "data": {
    "web": [{ "url": "...", "title": "...", "description": "..." }],
    "news": [{ "url": "...", "title": "...", "publishedDate": "2026-04-02T14:00:00" }],
    "images": [{ "url": "...", "imageUrl": "...", "thumbnailUrl": "..." }]
  }
}
```

## Search with content scraping

When you need more than result snippets, add `scrapeOptions`:

```json
{
  "query": "web scraping tools",
  "limit": 3,
  "scrapeOptions": {
    "formats": ["markdown"],
    "onlyMainContent": true
  }
}
```

That enriches eligible results with scraped page content. It is powerful, but it is also the moment search becomes more expensive, so keep it off until you need it.

## Freshness, sources, and categories

- Use `tbs` when freshness matters more than broad recall.
- Use `sources` when you want different result groups such as `web`, `news`, or `images`.
- Use `categories` to narrow the query domain without rewriting the query itself.

Good default: add one narrowing control at a time so you can see which one actually improved the results.

## Self-hosting the SearXNG sidecar

The default `docker-compose.yml` ships a hardened SearXNG container:

- Read-only root filesystem with sized tmpfs scratch
- All Linux capabilities dropped, `no-new-privileges`
- `mem_limit`, `pids_limit` set
- Pinned upstream image tag (we never run `:latest`)
- Config mounted read-only from `config/searxng/settings.yml`

It is mere-aggregation under AGPL — you are running an unmodified upstream SearXNG image with config mounted at runtime, so no §13 corresponding-source obligations attach to the image itself. If you redistribute your CRW deployment publicly, AGPL §13 still requires you to offer the corresponding source of CRW (which is already on GitHub) to your users.

## Common production patterns

- Start with search only, then add `scrapeOptions` after you verify result quality.
- Use `sources: ["news"]` or `tbs` when freshness matters more than broad recall.
- Use `categories: ["github"]` or `["research"]` to narrow noisy queries.
- Keep `limit` low on the first pass so the result quality is easy to inspect.

## Common mistakes

- Adding `scrapeOptions` to every search before you know you need page content
- Confusing `sources` with `categories`
- Treating `qdr:h` as truly hourly precision; SearXNG collapses it to `day`
- Sending `plainText` or `json` in `scrapeOptions.formats` — use `/v1/scrape` for those

## When to use something else

- Use [Scrape](#scraping) when you already know the exact URL
- Use [Map](#map) for site-specific discovery
- Use [Crawl](#crawling) when you need a bounded multi-page job on one site
