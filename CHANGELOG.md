# Changelog

All notable changes to this fork (`crw-camofox`) are documented here. This is a
camofox-first, anti-detection variant of crw; entries below cover the fork's own
`v1.x` line. The format follows [Keep a Changelog](https://keepachangelog.com/),
and the project uses [Semantic Versioning](https://semver.org/).

## [1.2.0] - 2026-07-18

A hardening release: anti-bot detection accuracy, renderer recovery under blocked
or dead egress, and map/search robustness — plus the camofox tier now respects
the request deadline and search no longer fails silently.

### Added

- **Map:** URL discovery on hard sites, with a configurable discovery limit.
  SPA shells and sitemap-less sites now yield a real URL inventory instead of a
  seed-only result.
- **Map:** anti-bot-gated sitemaps (Cloudflare / JS challenge) are escalated
  through the JS renderer, recovering the real sitemap XML instead of dropping it.
- **Search:** per-request `country` is threaded into result-page scraping, so the
  exit country of each scraped result follows the caller's geo.

### Fixed

- **Anti-bot detection:** challenge/interstitial pages are now reported as blocked
  (`success: false`) instead of returning the challenge shell as content. Covers
  modern Cloudflare Turnstile 200 interstitials, large (100–300 KB) managed
  challenge pages whose marker sits deep in the body, full-HTML marker scanning
  (previously capped at 80 KB), and HTTP-200 datacenter block shells. A detected
  block clears the page body so callers get a clean verdict.
- **Renderer recovery:** blocked-egress and connection-reset scrapes retry through
  the fallback proxy; connect-timeout blackholes are recovered the same way with a
  tighter connect timeout; a near-exhausted request budget no longer invokes a JS
  tier that can only fabricate a timeout, and an unreachable origin now surfaces as
  `422` rather than a generic `500`.
- **Renderer:** CDP WebSocket host is resolved to an IP for the Chromium 148+
  rebinding guard; thin pages that ship no executable JS are no longer sent to a
  browser (the render would reveal nothing); a multibyte-safe slice guard prevents
  a panic when scanning large pages.
- **Search:** charset-aware decoding — Latin-1 / Windows-1252 pages are decoded
  correctly instead of turning high bytes into replacement characters; each
  per-result scrape carries its own error; answer-synthesis warnings are neutral.
- **Search:** failed, hung, or zero-row engines are surfaced as response warnings
  (via `unresponsive_engines`) instead of an invisible empty success.
- **Camofox tier:** every REST operation is bounded by the remaining request
  deadline instead of the fixed 30 s client timeout, so a stalled render can no
  longer overrun the caller's deadline; tab cleanup keeps a small grace budget so
  tabs are reaped rather than leaked.
- **Security:** every sitemap URL (not just the seed) is resolve-validated against
  the private-range blocklist before fetching, closing an SSRF path where a crafted
  sitemap index could point a child at an internal address.

### Performance

- HTML extraction runs off the async reactor, keeping the runtime responsive under
  extraction-heavy load.

## [1.1.2] - 2026-07-04

### Fixed

- **Renderer:** body text is counted past the detector window and the scan
  survives a multibyte character straddling the window edge.

## [1.1.1] - 2026-07-02

### Fixed

- **Renderer:** the CDP target is reaped when a lightpanda fetch is cancelled;
  the 429 proxy-retry is disabled when the proxy URL is malformed; the relaxed-TLS
  fallback fires only on certificate errors.
- **Search:** `fetch_expanded` short-circuits when the original fetch fails, and
  C1 query-expansion overlap is skipped when `sources` is set (no double-scrape).
- **Research API:** correct title recovery when merging duplicate paper hits,
  arXiv-id extraction from string leaves, category folding into the search term,
  URL-encoded date filters, Semantic-Scholar legs skipped without an S2 key, and a
  concurrency permit released during backoff.

### Changed

- **Stealth:** single source of truth for the Chrome major UA version.
- **Search:** BYOK LLM config is built as a fail-closed allowlist.

## [1.1.0] - 2026-06-27

### Added

- **Search:** overlapping query-expansion scrape with the original query (C1).
- **Search:** Firecrawl-compatible research API engine layer.
- **Renderer:** relaxed-TLS fallback and 429 proxy-retry for the HTTP fetch tier.
- **Extract:** optional `reasoning_effort` config field.

### Fixed

- **Map:** SPA shells are rendered during URL discovery (#166).
- **Proxy:** an empty `CRW_CRAWLER__PROXY` normalizes to none (#154).
- **Stealth:** UA and `Sec-Ch-Ua` bumped from Chrome 131 to 150; the "Mozilla"
  UA prefix is stripped for lightpanda; a modern UA is sent on the CDP path.
- **PDF:** sandbox child address space is bounded to prevent a false
  `pdf_too_large`.
- **Research API:** arXiv inspection resolved via Semantic Scholar.

### Performance

- Research concurrency raised 4→8 with a tighter cache cap.

## [1.0.0] - 2026-06-17

First tagged release of the fork — a camofox-first, anti-detection variant of crw,
distributed solely as a multi-arch Docker image. This release establishes the
fork's identity: the camofox (Camoufox / Firefox) browser is the heavy renderer
and the sole search backend, and the upstream Chrome/Playwright and SearXNG
machinery is removed.

### Added

- **Camofox renderer tier:** the camofox-browser REST server (Camoufox / Firefox
  anti-detect browser) is wired in as the heavy/stealth JS tier, taking Chrome's
  slot in the failover ladder.
- **Camofox-backed search:** `/v1/search` drives the camofox browser directly —
  Google via the built-in search macro; Bing, DuckDuckGo, Wikipedia, YouTube,
  Reddit, and Amazon by navigating their result pages; GitHub via the REST Search
  API — behind a per-engine extractor registry with a generic fallback.
- **Multi-engine search:** a `SearchEngine` enum and an `engines` request field
  fan a query out across up to four engines and merge/dedupe the results, exposed
  across MCP, OpenAPI, the SDKs, and the CLI (stringified-array args accepted).
- **crw-browse-camofox:** an MCP server over the camofox REST API for interactive
  browser control (later switched to the upstream camofox-mcp).
- A single warm camofox tab is reused across searches to end the context-teardown
  race that produced empty or 5xx results.

### Changed

- Camofox is the **default heavy renderer** and the **only** `/v1/search` backend.
- The Docker Compose stack defaults to camofox; distribution is Docker-only
  (multi-arch image on GHCR).

### Removed

- **BREAKING:** the Chrome and Playwright renderer tiers and `crw-browse` — the
  ladder is now lightpanda (CDP) plus the camofox / camoufox REST tiers only.
- **BREAKING:** the SearXNG search backend, replaced entirely by camofox.
- Upstream's crates.io / npm / PyPI / SDK packaging and install paths.

### Fixed

- Structured-extraction (Anthropic) chat URL no longer doubles `/v1`.
- Engine cap and camofox-mcp configuration corrected against a live Docker stack.

[1.2.0]: https://github.com/adambenhassen/crw-camofox/compare/v1.1.2...v1.2.0
[1.1.2]: https://github.com/adambenhassen/crw-camofox/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/adambenhassen/crw-camofox/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/adambenhassen/crw-camofox/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/adambenhassen/crw-camofox/releases/tag/v1.0.0
