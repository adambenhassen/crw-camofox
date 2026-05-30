<div class="page-intro">
  <div class="page-kicker">More APIs</div>
  <h1>Monitoring</h1>
  <p class="page-subtitle">Schedule recurring scrapes or crawls, detect when a page actually changes, and notify your agent by signed webhook or email — with a structured diff and an optional LLM judge that filters out noise. A self-hostable, Firecrawl-compatible alternative to <code>/monitor</code>.</p>
  <div class="page-capabilities">
    <div class="page-capability"><strong>Best for:</strong> change detection on pages you rely on</div>
    <div class="page-capability"><strong>Hosted:</strong> fastcrw.com (full scheduler + notifications)</div>
    <div class="page-capability"><strong>Self-hosted:</strong> changeTracking primitive + optional <code>monitor</code> mode</div>
    <div class="page-capability"><strong>Start with:</strong> one scrape target, daily schedule</div>
  </div>
  <div class="page-actions">
    <a class="page-btn primary" href="https://fastcrw.com/register" target="_blank" rel="noopener">Get API Key</a>
    <a class="page-btn secondary" href="#crawling">View Crawl</a>
  </div>
</div>

## What this is for

Use monitoring when you need to know the moment a page changes and only care about the changes that matter — competitor pricing, product catalogs, job listings, docs, changelogs, research papers, or government filings. A monitor runs scheduled scrapes or crawls, diffs each result against the last retained snapshot, classifies every page (`same`, `new`, `changed`, `removed`, or `error`), and delivers a structured diff. The output is just the change, so your agent ingests far fewer tokens than re-scraping everything.

Reach for `monitoring` instead of polling `/v1/scrape` yourself when you want the schedule, snapshot storage, diffing, retries, and noise filtering handled for you.

:::note
**Self-hosted users**: the full scheduler + notification control plane is part of the hosted product. The open-core engine ships the **stateless `changeTracking` primitive** (diff one scrape against a snapshot you supply) plus an optional, feature-gated **`monitor` mode** (SQLite scheduler, default OFF). See [self-hosting monitoring](#monitoring) below.
:::

## Endpoints

All monitor endpoints require a Bearer API key on the hosted API.

```http
POST   /v1/monitor                          # create
GET    /v1/monitor                          # list
GET    /v1/monitor/{id}                      # get
PATCH  /v1/monitor/{id}                      # update
DELETE /v1/monitor/{id}                      # delete
POST   /v1/monitor/{id}/run                  # run now (409 if a check is in flight)
GET    /v1/monitor/{id}/checks               # list checks
GET    /v1/monitor/{id}/checks/{checkId}     # get one check + its pages
```

Base URL: `https://fastcrw.com/api` (hosted).

## Create a monitor

Describe what to watch and how often. A `goal` enables the LLM judge so you are only alerted on meaningful changes.

:::tabs
```bash
curl -s -X POST "https://fastcrw.com/api/v1/monitor" \
  -H "Authorization: Bearer $CRW_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Pricing monitor",
    "schedule": { "text": "every 30 minutes", "timezone": "UTC" },
    "goal": "Alert when a pricing tier, price, or headline feature changes.",
    "targets": [
      { "type": "scrape", "urls": ["https://example.com/pricing"] }
    ],
    "notification": {
      "email": { "enabled": true, "recipients": ["alerts@example.com"], "includeDiffs": true }
    }
  }'
```

```javascript
const res = await fetch("https://fastcrw.com/api/v1/monitor", {
  method: "POST",
  headers: {
    Authorization: `Bearer ${process.env.CRW_API_KEY}`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({
    name: "Pricing monitor",
    schedule: { text: "every 30 minutes", timezone: "UTC" },
    goal: "Alert when a pricing tier, price, or headline feature changes.",
    targets: [{ type: "scrape", urls: ["https://example.com/pricing"] }],
    notification: {
      email: { enabled: true, recipients: ["alerts@example.com"], includeDiffs: true },
    },
  }),
});
const { data } = await res.json();
console.log(data.id, data.nextRunAt);
```

```python
import os, requests

res = requests.post(
    "https://fastcrw.com/api/v1/monitor",
    headers={"Authorization": f"Bearer {os.environ['CRW_API_KEY']}"},
    json={
        "name": "Pricing monitor",
        "schedule": {"text": "every 30 minutes", "timezone": "UTC"},
        "goal": "Alert when a pricing tier, price, or headline feature changes.",
        "targets": [{"type": "scrape", "urls": ["https://example.com/pricing"]}],
        "notification": {
            "email": {"enabled": True, "recipients": ["alerts@example.com"], "includeDiffs": True}
        },
    },
)
print(res.json()["data"]["id"])
```
:::

The response returns the monitor with its normalized cron, computed `nextRunAt`, and `estimatedCreditsPerMonth` (an upper bound when judging is enabled). When the monitor has a webhook, the signing secret is returned **once** here as `webhookSecret`.

```json
{
  "success": true,
  "data": {
    "id": "019df960-06e7-7383-9d89-82c0113dc31a",
    "name": "Pricing monitor",
    "status": "active",
    "schedule": { "cron": "*/30 * * * *", "timezone": "UTC", "text": "every 30 minutes" },
    "nextRunAt": "2026-05-30T16:00:00.000Z",
    "lastRunAt": null,
    "currentCheckId": null,
    "goal": "Alert when a pricing tier, price, or headline feature changes.",
    "judgeEnabled": true,
    "targets": [
      { "type": "scrape", "urls": ["https://example.com/pricing"], "changeMode": "markdown" }
    ],
    "webhook": null,
    "notification": { "emails": ["alerts@example.com"], "includeDiffs": true },
    "retentionDays": 30,
    "estimatedCreditsPerMonth": 2880,
    "lastCheckSummary": null,
    "createdAt": "2026-05-30T15:30:00.000Z",
    "updatedAt": "2026-05-30T15:30:00.000Z"
  }
}
```

## Schedules

Provide a schedule as cron **or** as natural-language `text`. The minimum interval is 15 minutes; responses always return the normalized cron. `timezone` is an IANA zone (DST-correct — "daily at 9am" tracks 9:00 wall-clock across transitions). Text schedules are spread by monitor id so many monitors don't all fire at the same instant.

Supported natural-language forms: `every 30 minutes`, `every 15 minutes starting at :07`, `hourly`, `every 2 hours`, `daily`, `daily at 9:00`, `daily at 9am`, `daily at 5:30 PM`, `weekly`.

## Targets

Each monitor takes 1–50 targets:

- **`scrape`** — runs one scrape per URL in `urls` (≤50 distinct URLs across all targets).
- **`crawl`** — runs a full crawl for `url` on each check, then diffs every discovered page. Use `maxPages` to bound cost.

`scrapeOptions` / `crawlOptions` pass through to the underlying jobs. Monitor scrapes always fetch fresh.

## Goals and judging

Add a plain-language `goal` to be alerted only on meaningful changes. When `goal` is set and `judgeEnabled` is omitted, judging is enabled automatically. The judge runs only on **changed** pages and returns a judgment with `meaningful`, `confidence` (`low` / `medium` / `high`), `reason`, and `meaningfulChanges`. Set `judgeEnabled: false` to store a goal without judging.

## Change tracking modes

By default each page's markdown is diffed and reported as `same` / `changed` / `new` / `removed` / `error`. To track specific structured fields, set a `changeMode` on the target:

- **`markdown`** (default) — a unified text diff plus a parse-diff-style AST.
- **`json`** — supply a `jsonSchema`; CRW extracts those fields each check and emits a per-field diff keyed by JSON path (`plans[0].price → {previous, current}`) plus a full `snapshot`.
- **`mixed`** — both surfaces; a page is `changed` if **either** the markdown or a tracked field changed.

## Notifications

### Webhooks

Configure a `webhook` to receive two events:

- **`monitor.page`** — sent as each monitored page finishes; includes `isMeaningful` + `judgment` when judging ran.
- **`monitor.check.completed`** — sent after the full check reconciles, with summary counts.

Deliveries are signed (`X-CRW-Signature: t=<unix>,v1=<hmac-sha256>` over `<t>.<body>`), support custom headers + metadata and per-event subscription, retry with backoff, and dead-letter after repeated failures (with a one-time failure email). Webhook URLs are SSRF-guarded (https-only, private/loopback/metadata ranges blocked).

```json
{
  "webhook": {
    "url": "https://example.com/webhooks/crw",
    "events": ["monitor.page", "monitor.check.completed"],
    "headers": { "Authorization": "Bearer your-secret" },
    "metadata": { "environment": "production" }
  }
}
```

### Email

Email summaries are sent only when a check has `changed` / `new` / `removed` / `error` pages. With a goal + judging, noise-only checks are suppressed. New recipients receive a confirmation link (double opt-in) before any alert; up to 25 confirmed recipients per monitor. Set `includeDiffs: true` to embed the diff in the message.

## Check results

List checks with `GET /v1/monitor/{id}/checks` (filter by `status`: `queued`, `running`, `completed`, `failed`, `partial`, `skipped_overlap`) and inspect one with `GET /v1/monitor/{id}/checks/{checkId}`. Both auto-paginate via an opaque `next` cursor. A check detail returns `estimatedCredits`, `actualCredits`, summary counts, and a paginated `pages[]` array; each changed page carries inline `diff` data and (json mode) a `snapshot`.

```bash
curl "https://fastcrw.com/api/v1/monitor/$MONITOR_ID/checks/$CHECK_ID?status=changed" \
  -H "Authorization: Bearer $CRW_API_KEY"
```

## Parameters

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | Human-readable monitor name |
| `schedule.cron` | string | -- | Cron expression (provide this or `schedule.text`) |
| `schedule.text` | string | -- | Natural-language schedule (e.g. `"every 30 minutes"`) |
| `schedule.timezone` | string | `"UTC"` | IANA timezone for text/cron evaluation |
| `goal` | string | -- | Plain-language alert intent; enables the judge (≤2 KB) |
| `judgeEnabled` | boolean | auto | Force judging on/off; auto-on when `goal` is set |
| `targets` | object[] | required | 1–50 targets (`scrape` or `crawl`) |
| `targets[].type` | string | required | `"scrape"` or `"crawl"` |
| `targets[].urls` | string[] | -- | Scrape target URLs (≤50 distinct across targets) |
| `targets[].url` | string | -- | Crawl target root URL |
| `targets[].changeMode` | string | `"markdown"` | `markdown`, `json`, or `mixed` |
| `targets[].jsonSchema` | object | -- | Fields to track in `json` / `mixed` mode |
| `targets[].maxPages` | number | `1000` | Crawl page cap |
| `webhook` | object | -- | Signed webhook config (see Notifications) |
| `notification.email` | object | -- | `{ enabled, recipients[], includeDiffs }` |
| `retentionDays` | number | `30` | Snapshot/check retention (1–365) |

## Pricing

Monitors have no per-monitor fee. Each check pays for the scrapes or crawl it performs, plus an optional judge credit per changed page.

| Component | Credits |
| --- | --- |
| Scrape monitor | 1 credit per URL per check |
| Crawl monitor | 1 credit per discovered page per check |
| Meaningful-change judging | +1 credit per changed page the judge validates |

Checks with no changed pages use no judge credits. When a monitor runs out of credits its checks pause (`paused_no_credits`) and resume automatically once the balance recovers.

## Self-hosting monitoring

The open-core engine gives self-hosters the building blocks:

- **`changeTracking` scrape format** — add it to `/v1/scrape` `formats` with the diff `modes` and a `previous` snapshot you persist between checks. opencore is stateless: it returns the diff + the new snapshot for you to store.
- **`POST /v1/change-tracking/diff`** — diff a page (or a batch) against a supplied `previous` snapshot. The workhorse for crawl-based monitoring.
- **Optional `monitor` mode** — build the engine with the `monitor` Cargo feature (default OFF) for a SQLite-backed scheduler, set-level `new`/`removed`, an LLM judge, and signed local webhooks, with no external database.

```bash
# diff the current scrape against your stored snapshot
curl -s -X POST "http://localhost:3000/v1/change-tracking/diff" \
  -H "Content-Type: application/json" \
  -d '{
    "modes": ["gitDiff"],
    "previous": { "markdown": "Starter $19", "contentHash": "abc" },
    "current":  { "markdown": "Starter $24" }
  }'
```

:::note
The `monitor` feature pulls in SQLite/HMAC dependencies only when enabled — the default engine build stays dependency-light. Self-host monitoring uses UTC schedules and your own LLM key (BYOK) for judging.
:::

## Common mistakes

- **Passing `changeTracking` as an object in `formats`** — on the engine it is the plain string `"changeTracking"`; the options ride on the sibling `changeTracking` field.
- **Expecting `removed` from a scrape target** — `new` / `removed` are set-level states for **crawl** targets; a fixed `urls[]` entry that fails is `error`, never `removed`.
- **Intervals under 15 minutes** — rejected. Use `every 15 minutes` or longer.
- **Forgetting the webhook secret** — it is shown once on create; store it to verify the `X-CRW-Signature` header.

## What to read next

- [Scrape](#scraping) — the single-page primitive monitors run under the hood.
- [Crawl](#crawling) — multi-page discovery for crawl targets.
- [Credit Costs](#credit-costs) — how checks are metered.
- [Self-Hosting](#self-hosting) — run the engine + optional `monitor` mode yourself.
