# Changelog

All notable changes to this fork (`crw-camofox`) are documented here. This is a
camofox-first, anti-detection variant of crw; entries below cover the fork's own
`v1.x` line. The format follows [Keep a Changelog](https://keepachangelog.com/),
and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Renderer:** a Chrome-impersonated HTTP tier (`[renderer.impersonated]`,
  on by default in the Docker image). It presents a real Chrome TLS/JA3/HTTP2
  fingerprint with no browser and no JavaScript, runs between the plain HTTP
  fetch and the browser ladder on wall-shaped results or fingerprint-shaped
  transport errors, and never on pages that need JavaScript or on vendor
  walls that do. Pin it with `renderer: "impersonated-http"`; `renderJs:
  true` alongside the pin is rejected. Per-request `proxy` and `stealth`
  overrides bypass the hop. The tier is on by default in the Docker image and
  adds its 15 s budget to the auto-extended request deadline; set `enabled =
  false` to opt out.

## [1.4.0] - 2026-09-15

A hardening release: the Camofox tier now waits out Cloudflare challenges and
reuses the clearance it earns, a Byparr solver tier handles Turnstile
checkboxes, and scrapes stop shipping error pages, walls and parking pages as
content. Crawl and batch keep failed pages as marked documents, and the
metrics and admin routes sit behind the API-key boundary.

### Added

- **Renderer:** the Camofox tier now waits, on the open tab, for a Cloudflare
  "Just a moment" challenge to clear (`renderer.camofox.challenge_wait_ms`,
  default 20 s, always inside the request deadline) instead of snapshotting
  the interstitial and reporting the page as blocked.
- **Renderer:** after a Camofox render that earned a `cf_clearance` cookie,
  the tab's cookies and user agent are cached per host and sent with the
  HTTP-tier fetch on later scrapes of that host, so they skip the browser.
  A repeat challenge drops the entry. Off with
  `renderer.camofox.clearance_reuse = false`; never used through a proxy.
  The cache is used only when its `cf_clearance` cookie applies to the host
  being fetched.
- **Crawl:** `CrawlRequest.headers` (and v2 `scrapeOptions.headers`) reach
  every page fetch. The nested `scrapeOptions` shape from the spec is accepted.
- **Crawl / batch:** a page that could not be read (transport failure, CDN
  origin error, wall, PDF or extraction failure) is returned as a document
  marked through `block` and counted in a new `blocked` field, instead of
  being dropped. Blocked v2 documents cost zero credits.
- **Crawl:** a crawl whose every fetch failed ends `failed` with the last
  fetch failure's reason instead of completing with zero pages.
- **v2:** `GET /v2/{batch/scrape,crawl}/{id}/errors` lists each failed URL with
  its reason. `/v2/scrape` honours `renderJs` and turns `location.languages`
  into an `Accept-Language` header. v2 documents carry `llmUsage` when an LLM
  ran, as `/v1` does.
- **Map:** `/v1/map` responses include a `sitemaps` array of the sitemap documents that
  answered with parseable content (kept out of `links`).
- **Search:** `scrapeOptions.timeout` (1–60000 ms) sets the per-result scrape
  budget; the default is 15 s instead of the full renderer ladder. Results
  carry `truncated`.
- **Docker:** `CRW_HOST_PORT` and `CRW_BIND_ADDRESS` set the published port
  and host interface.
- **Renderer:** a Byparr challenge-solver tier (`[renderer.byparr]`). The
  ladder calls it last, and only when the HTTP tier or an earlier browser tier
  returned an anti-bot challenge or wall. It clicks the Cloudflare Turnstile
  checkbox, and the `cf_clearance` it earns feeds the same per-host cache as
  Camofox (`renderer.byparr.clearance_reuse`). Pages that end on an internal
  address are refused. Docker Compose runs Byparr by default.

### Fixed

- **Renderer:** a caller-supplied `User-Agent` no longer travels with the
  stealth mode's Chrome `Sec-Ch-Ua*` client hints. A blank `User-Agent` is
  treated as absent.
- **Renderer:** the escalation after LightPanda targeted a `chrome` tier this
  fork never builds, so it always failed and Camofox was never reached. It now
  escalates to Camofox. Custom request headers now reach LightPanda renders
  too; Camofox renders still ignore them.
- **Renderer:** Camofox renders report the page's real HTTP status instead of
  200 for every page, including 404 and 403.
- **Renderer:** when every browser tier rejects a page as an anti-bot wall, the
  scrape fails instead of returning the wall, unless the page is larger than
  an error page (so cleared Cloudflare pages still succeed).
- **Renderer:** a forced-JS scrape whose browser tiers fail falls back to the
  HTTP body it already fetched, with a `js_escalation_failed` warning.
- **Renderer:** timeouts report the requested budget instead of the near-zero
  overrun. A blackholed origin is reported as unreachable, not a timeout. An
  unanswered proxy auth challenge is reported as a proxy authentication
  failure. A request no longer returns to a proxy that already failed it.
- **Renderer:** a dead Camofox proxy (Firefox `proxyConnectFailure`) now trips
  the renderer breaker instead of counting as the site's fault. Anti-bot walls
  no longer advance the breaker when a recovery tier exists.
- **Renderer:** binary bodies (a NUL byte in the first 1 KB) fail with 422
  instead of being decoded as HTML; a mislabelled PDF is detected by its
  header.
- **Scrape:** Cloudflare 520–527 origin-error pages, origin error pages
  (>= 400 with little text), registrar parking pages, and Reddit, Cloudflare
  and Vercel block pages served as 200 fail instead of shipping as content.
  Cleared Cloudflare pages and thin but real pages are no longer reported as
  blocks. A scrape whose requested formats all come back empty fails as
  `no_usable_content`. Crawl and batch apply the same verdicts.
- **Crawl:** a Camofox escalation that adds content below the LightPanda retry
  threshold is kept. `truncated` follows the render that was kept.
- **Crawl:** a complete non-HTML body (a JSON API response, say) that scores
  low on the quality check no longer buys a browser render that was then
  discarded.
- **Extract:** `onlyMainContent` no longer deletes article bodies whose
  wrappers are named after nearby layout (sidebar, nav, footer), keeps
  `<header>`/`<aside>`/`<footer>` inside `<main>` or `<article>`, keeps
  Elementor page content, and removes navigation menus. Non-HTML bodies are
  left as they are.
- **Extract:** `maxChars` and the LLM prompt cap count Unicode scalars, not
  bytes. An LLM request whose connection closed before it reached the provider
  is retried once. An extract URL whose page is a wall or error page fails
  with that reason, and a job whose every URL failed charges no credits.
- **Search:** Google result links resolve to their real URLs instead of Google
  `/goto` redirects; consent and rate-limit redirects keep the original link.
  `lang` is validated at the API boundary. Enrichment DNS checks run in
  parallel.
- **Research:** arXiv is a paper source again, and OpenAlex queries no longer
  fail on `?` or `*`.
- **Security:** the SSRF guard's DNS lookup is bounded at 8 s and no longer
  blocks whole public /16 ranges. The CDP render tiers check every outbound
  request (child frames included) against the SSRF rules, and LightPanda starts
  with `--block-private-networks`. Error strings no longer contain internal
  URLs or proxy credentials. PDF parsing moves to lopdf 0.42
  (RUSTSEC-2026-0187).
- **Security:** a per-request LLM `baseUrl` (BYOK) that points at a private,
  loopback or link-local address is refused with 400 on scrape, v2 batch start
  and search. Before, the server POSTed page content and the caller's key to
  it. LLM calls no longer follow redirects to such addresses. The operator's
  `[extraction.llm].base_url` is not restricted.

### Changed

- **Breaking:** `/metrics`, `/metrics/renderer-breakers`, and
  `/admin/breakers/reset` now sit behind the API-key auth boundary. When
  `[auth].api_keys` is set, Prometheus scrapes must send a Bearer token.
- The Camofox tier refuses to return a page whose final URL (after redirects
  and client-side navigation) is a private or internal address. This guards
  what crw returns only: requests the browser makes on the way (a redirect hop,
  subresources) still reach the network. camofox-browser's own private-network
  check covers the URL it is asked to open, not redirects — verified live.
- A Camofox fetch that lands on Firefox's own error page ("Problem loading
  page") now fails as a navigation failure instead of returning that page.
- CORS is no longer permissive. Browser callers need their origin listed in
  `server.cors_allowed_origins`; the default sends no CORS headers. Entries
  are matched as origins, so a trailing slash or capitals no longer disable
  CORS silently; `*` is ignored with a warning.
- **Breaking:** a malformed proxy URL is refused instead of ignored. A bad
  `crawler.proxy` (or CLI `--proxy`) fails startup, and a bad per-request
  `proxy` returns 400. Before, the value was logged and dropped, and traffic
  went out directly from the server's own address.
- An unreachable search backend now answers 502 instead of 422.

## [1.3.0] - 2026-09-13

A robustness release for the Camofox tier: search and render recover from the
browser server's transient failures instead of wedging or stalling, pinned
renders no longer die on the HTTP probe, and errors from the browser server
finally say what went wrong.

### Added

- **Scrape:** an `images` output format returns the page's discovered images as
  structured data (`[{url, alt}]`, flattened to `string[]` on v2) with
  WHATWG-correct `srcset` parsing, instead of callers re-parsing HTML.

### Fixed

- **Search:** a search that timed out on the reused warm tab left every later
  search stalled behind the same dead tab until the process restarted. A
  timeout on a reused tab now recreates the tab and retries once; a timeout
  on a fresh tab swaps it out for the next search. The abandoned tab is
  closed only after its replacement exists, so the browser context never
  drops to zero tabs and hits Camofox's eager teardown.
- **Search:** a failed `/evaluate` (Camofox answers a dead tab with a JSON
  error, not an empty result) is treated as an upstream error so the stale-tab
  recovery runs, instead of being reported as a clean empty result page.
- **Search:** the response no longer blames SearXNG in error messages; the
  backend is Camofox.
- **Renderer:** tabs are created blank under a creation lock and navigated
  with a separate call. Concurrent creates that navigated inside the create
  raced for Camofox's initial blank page on a freshly relaunched context and
  aborted each other, and each failure counted toward Camofox's
  consecutive-failure breaker, which then closed the context. A create that
  lands in Camofox's context relaunch window (`window is null`) is retried
  with a growing pause inside the request deadline instead of failing the
  render.
- **Renderer:** when JS rendering is requested (which a pinned renderer
  implies), an HTTP-tier failure now escalates to the JS tier the way auto
  mode does. A Camofox-pinned scrape of an origin slower than the HTTP timeout
  used to return `502` without ever reaching Camofox.
- **Renderer:** pages whose HTML exceeds Camofox's 1 MiB single-result cap
  are retrieved in slices. Previously the Camofox tier returned the server's
  truncation placeholder as the document, so large pages (Wikipedia-sized
  articles) came back empty and flagged as a loading placeholder.
- **Renderer:** a navigate that Camofox reports as failed only because its
  post-navigation ARIA snapshot timed out (large documents) no longer fails
  the render; the page has loaded by then and the snapshot is unused.
- **Renderer:** the Camofox tier is registered independently of the `cdp`
  feature, and `renderer.mode = "camofox"` without a configured endpoint (or
  in a binary built without the feature) is a startup configuration error
  rather than a silent fall back to HTTP-only.
- **Diagnostics:** Camofox's own error message (for example a persistent
  profile pinned to an older Camoufox build) is carried into search and
  renderer errors instead of a bare status; non-JSON bodies such as proxy
  error pages are logged rather than surfaced to API clients. Rejected or
  failed closes of abandoned tabs, and engine failures that the response
  strips to a short reason, are now logged in full.
- **MCP:** inline scrape content in search results is capped across results
  to protect agent context.
- **Release:** the internal crate version pins are synchronized with the
  workspace version, so the CI version guard passes again.

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

[1.4.0]: https://github.com/adambenhassen/crw-camofox/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/adambenhassen/crw-camofox/compare/v1.2.0...v1.3.0
[1.2.0]: https://github.com/adambenhassen/crw-camofox/compare/v1.1.2...v1.2.0
[1.1.2]: https://github.com/adambenhassen/crw-camofox/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/adambenhassen/crw-camofox/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/adambenhassen/crw-camofox/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/adambenhassen/crw-camofox/releases/tag/v1.0.0
