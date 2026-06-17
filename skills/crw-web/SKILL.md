---
name: crw-web
description: "Use when fetching a page, searching the web, crawling a site, mapping URLs, or parsing a PDF — via the crw MCP tools (crw_scrape, crw_search, crw_crawl, crw_map, crw_check_crawl_status, crw_parse_file). Start here for any read or discovery task before reaching for an interactive browser."
---

# `crw` — lightweight web suite (use first)

Static or lightly-interactive content retrieval and discovery over the crw MCP
server. No browser overhead — these tools are faster and far more token-efficient
than driving a real browser.

**Escalate only for real interactivity.** Logins, clicks, forms, SPA/lazy content,
anti-bot bypass, screenshots, or in-page JavaScript need a live browser — see the
**camofox-browser** skill (requires the `camofox-mcp` server). Try `crw` first.

## Tools

- **crw_search** — Search the web → titles, URLs, descriptions. Defaults to **Google**; can target or merge multiple engines via the `engines` field (see below). Start here for discovery.
- **crw_scrape** — Scrape one URL → markdown / HTML / links. The default way to read a page.
- **crw_parse_file** — Parse a local PDF (base64) → markdown. No OCR.
- **crw_map** — Discover all URLs on a site (sitemap + short crawl). Use to find pages before scraping.
- **crw_crawl** — Start an async multi-page crawl → returns a job ID.
- **crw_check_crawl_status** — Poll a crawl job and retrieve its pages.

**Use `crw` when:** the page is static or lightly interactive; search results, one article, a PDF,
or a sitemap are enough; speed and token economy matter.

**Blank page / loading skeleton? Try `crw` first, not the browser.** If `crw_scrape` returns empty
or a loading shell, retry with `renderJs: true` before escalating — it's a much cheaper rung than
the full interactive browser. Only escalate if `renderJs` still fails.

**Crawl economy:** run `crw_map` first to estimate a site's size before committing to a large
`crw_crawl`. Crawl jobs expire after ~1 hour, so poll `crw_check_crawl_status` regularly until done.

**Output is bounded by default.** Content fields (`crw_scrape`, `crw_check_crawl_status`,
`crw_parse_file`, and any `scrapeOptions` on `crw_search`) truncate to ~15 000 chars, and `crw_map`
returns ≤ 100 URLs. Truncated results carry a `truncated: true` marker. Pass `maxLength: 0` (or
`limit: 0` for `crw_map`) to opt out when you genuinely need the full payload.

## crw_search engines

`crw_search` runs on the Camofox backend and accepts an optional **`engines`** field — a list of
which search engines to query.

- **Omit it → Google.** No `engines` field (or an empty list) searches Google only — the default.
- **Set it to pick engine(s).** Valid values (typed; an unknown value is rejected):
  `google`, `bing`, `duckduckgo`, `wikipedia`, `youtube`, `reddit`, `amazon`, `github`.
- **List several → one merged result set.** Results from all listed engines are **deduped by URL**
  and **agreement-ranked** (a URL several engines return scores higher). Downstream rerank still
  applies on the answer/summarize path.

```json
{ "query": "rust web scraping", "engines": ["google", "bing", "duckduckgo"], "limit": 10 }
```

> **⚠️ Pitfall — `engines` serialization (resolved):** the `engines` field must be a **JSON array**.
> A past server version double-serialized it to a string (`"[\"bing\"]"`) → `invalid type: string,
> expected a sequence`. Fixed server-side (June 2026); if you see it again the server needs a
> restart/redeploy, not a client-side workaround.

**Constraints & behavior:**
- **Any of the eight engines** — no hard cap; list as many as you need.
- **Sequential** — engines run one at a time on a single warm browser tab, so N engines ≈ N× latency.
  Keep the list short when speed matters.
- **Heavy engines** (`reddit`, `amazon`) have JS-heavy SERPs; give them a generous
  `[search].timeout_ms` (and camofox `HANDLER_TIMEOUT_MS`) or they may time out.
- **Per-engine failure is non-fatal** — a failing engine is skipped and the others still return;
  only an all-engines-fail request errors.
- **`github` is special** — it uses the GitHub REST Search API (not the browser). Set a token via
  `[search].github_token` / `CRW_SEARCH__GITHUB_TOKEN` to lift its rate limit (unauthenticated
  works but throttles quickly).
- **Engine coverage note:** these eight are verified to return clean results without a login. Other
  platforms (Twitter/LinkedIn/Instagram/Facebook/TikTok/etc.) are login-walled or anti-bot and are
  intentionally not exposed — use the **camofox-browser** skill with a logged-in session for those.

**Beyond `engines`, `crw_search` takes standard filters** (schema-typed): `limit` (default 5, max
20), `tbs` time filter (`qdr:h|d|w|m|y` = past hour/day/week/month/year), `sources` to group results
by `web` / `news` / `images` instead of a flat list, and `categories` to bias toward e.g. `pdf` /
`github` / `research`. `scrapeOptions` inlines scraped content per result (bounded by `maxLength`).
