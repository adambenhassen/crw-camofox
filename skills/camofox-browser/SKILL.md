---
name: camofox-browser
description: "Use when a web task needs real browser interactivity — logins, clicks, form fills, scrolling, SPA/lazy-loaded content, anti-bot bypass, screenshots, or in-page JavaScript. Drives the camofox-mcp interactive browser; requires the camofox-mcp server. Escalate here only after crw (with renderJs) can't retrieve the content."
---

# camofox-mcp — interactive anti-detect browser (escalate only when needed)

> **Requires the `camofox-mcp` server.** These tools drive a live
> [Camofox](https://github.com/redf0x1/camofox-browser) (Firefox) anti-detect browser exposed by the
> upstream **camofox-mcp** server — a *separate* MCP server from crw's own tools. If you only have
> the `crw` tools (the **crw-web** skill), this skill does not apply — use those.

**Try `crw` first.** For plain fetching, searching, crawling, site-mapping, and PDF parsing the
**crw-web** tools are faster and far more token-efficient. Escalate to the browser **only** for real
interactivity — and only after a `crw_scrape` retry with `renderJs: true` has failed.

**Use the browser when:** the site needs login / browser state / anti-bot bypass; you must click,
type, submit forms, scroll, or run JS; content only appears after SPA hydration, modals, or lazy
loading; you need a screenshot for visual proof.

**Typical flow:** `create_tab` → `navigate` (or `navigate_and_snapshot`) → `snapshot` to read
element refs → act (`click` / `type_text`) → `camofox_close_session` when completely done.

> **Keep at least one tab alive, then close the session when done.** The browser context stays up
> only while a tab is open — closing the **last** tab mid-task tears down the context and loses all
> session state (cookies, login, history). So during a task, never close every tab: leave one open.
> **But when you are completely finished browsing, call `camofox_close_session`** to release the
> whole context — an abandoned session keeps a real browser running (high CPU/memory). Don't call it
> mid-task.

## Session/tab
- `server_status` — Check Camofox backend health/connection. Call first if unsure the browser is up.
- `create_tab` — Open a new anti-detect tab; for normal browsing pass **only a `url`** (or no args). Open one before navigating.
  - *Pitfall — `Context override conflict`:* the browser locks one fingerprint profile per identity on its first tab. **Don't pass `viewport` / `preset` / `locale` / `geo`** unless you genuinely need a specific geo/fingerprint — overrides that differ from the session's first tab cause this 409 ("close the session first to reconfigure"). If you hit it, **reuse the existing tab** (`list_tabs` → `navigate`) rather than recreating with new overrides.
- `list_tabs` — List open tabs with URLs and titles.
- `close_tab` — Close a single tab you opened and free memory. **Never close the last open tab mid-task** — that tears down the browser context and loses session state; keep at least one alive until done.
- `camofox_close_session` — **End-of-task cleanup:** closes **all** your tabs and tears down the browser context for your session, freeing CPU/memory. Call it once when you're completely finished browsing (not mid-task — it ends the session). Operates only on your own session.

## Navigation
- **navigate** — Load a URL in a tab (waits for page load).
- **navigate_and_snapshot** — navigate + wait + snapshot in one call (preferred for "go look at X").
- **go_back** / **go_forward** — Browser history back / forward.
- **refresh** — Reload the page (stale state or after changes).
- **toggle_display** — Switch a session between headless / headed / virtual; returns a VNC URL when available (use to watch or drive it live).

## Reading the page (inspection)
- **snapshot** — Accessibility-tree snapshot. The PRIMARY, token-efficient way to read a page and obtain element refs. Prefer over screenshot.
- **screenshot** — base64 PNG. Use ONLY for visual verification (CSS, layout, proof).
- **get_links** — All hyperlinks (URL + text); good for navigation discovery and mapping.
- **camofox_get_page_html** — Rendered live-DOM HTML when the snapshot misses dynamic/custom-component content.
- **camofox_query_selector** — Query a CSS selector → element text, HTML, attributes, visibility.
- **get_stats** — Session statistics: request counts, active tabs, uptime.

## Interaction
- **click** — Click an element by ref (from snapshot) or CSS selector. Snapshot first to get refs.
- **type_text** — Type into an input by ref or CSS selector.
- **type_and_submit** — Type into a field and press a key (default Enter). Best for search boxes / single-field forms.
- **fill_form** — Fill multiple form fields in one call, with optional submit.
- **batch_click** — Click several elements in sequence (continues on error).
- **camofox_hover** — Hover to trigger tooltips, dropdown menus, or hover states.
- **camofox_press_key** — Press a key: Enter to submit, Tab to move, Escape to dismiss.
- **camofox_evaluate_js** — Run JavaScript in the page context for extraction, DOM checks, or manipulation.

## Scrolling & waiting
- **scroll** — Scroll the page up/down by pixel amount (reveal below-the-fold content).
- **scroll_and_snapshot** — Scroll + snapshot in one call.
- **camofox_scroll_element** — Scroll a specific container (modal, sidebar, scrollable div) when page scroll can't reach it.
- **camofox_scroll_element_and_snapshot** — Scroll a container AND snapshot (incrementally load lazy content in modals).
- **camofox_wait_for** — Wait until the page is fully ready (DOM loaded, network idle, hydration complete).
- **camofox_wait_for_selector** — Wait for a CSS selector to appear (SPA / async workflows).
- **camofox_wait_for_text** — Wait for specific text to appear (search results / dynamic content).

## Extraction & media
- **extract_resources** — Extract images / links / media / docs from a DOM container (CSS selector or snapshot ref).
- **extract_structured** — Deterministic structured-JSON extraction via the camofox schema.
- **resolve_blobs** — Resolve `blob:` URLs to downloadable base64 (common in Telegram / WhatsApp / Discord).
- **web_search** — In-browser search across 14 engines (google, youtube, amazon, bing, duckduckgo, reddit, github, stackoverflow, wikipedia, twitter, linkedin, facebook, instagram, tiktok). Prefer `crw_search` (crw-web skill) for plain web search; use this when you then need to interact with the results in the live browser.
- **youtube_transcript** — Fetch a YouTube transcript by URL (no tab needed).
- **list_presets** — List geo presets (locale / timezone / geolocation) available for `create_tab`.

---

**Snapshot-first:** always read with `snapshot` (token-efficient) and act on element refs; fall back
to CSS selectors when no ref exists; use `screenshot` only for visual proof.
