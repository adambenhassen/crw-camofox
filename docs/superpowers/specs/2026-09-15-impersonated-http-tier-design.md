# Chrome-impersonated HTTP tier

Date: 2026-09-15. Status: approved design.

## Goal

Add a Chrome-impersonating HTTP tier to the render ladder, ported from
upstream `crw` (commits 8a388ed8, 7d748e58, 45865fb2). The tier is a plain
HTTP fetcher built on `wreq` that presents a real Chrome TLS/JA3/HTTP2
fingerprint. It executes no JavaScript and needs no external service.

Sites with TLS-fingerprint walls (the Amazon interstitial class) serve an
HTTP 200 interstitial to non-browser TLS stacks. Today the fork clears them
only by escalating to Camofox, which costs a browser render. The new tier
clears them in HTTP time.

## Ladder

```
HTTP → impersonated HTTP → LightPanda → Camofox → Byparr
```

The hop runs in the auto arm only (`render_js` resolved to `None`), after the
plain HTTP fetch and before the JS ladder. It fires on two triggers:

- **Wall-shaped result:** a bot-wall body, a `cf-mitigated` or
  `x-amzn-waf-action` header, or a 401/403/429/503/520–530 status.
- **Fingerprint-shaped transport error:** any HTTP-tier error except
  `Timeout`, `TargetUnreachable`, `UnsupportedContentType` and the
  "Response too large" cap.

It never fires on SPA, thin or empty shapes (impersonation cannot execute
JS), and never on a vendor wall that needs JS: Cloudflare managed challenge,
DataDome, PerimeterX, Kasada, Akamai, Imperva. That gate is the ported
`is_fingerprint_vendor_wall`.

Byparr keeps its existing challenge-hint gate. A hop that returns `None`
leaves the ladder byte-identical to today.

**Budget.** With a JS ladder configured the hop runs on half the remaining
request deadline, so a tarpitting wall host cannot starve Camofox. With no JS
tiers it gets the whole remaining budget. Below `MIN_TIER_BUDGET` it is
skipped.

**Pin.** `renderer: "impersonated-http"` is served by an early dispatch arm
ahead of the `render_js` match. A pinned result that carries a wall errors
(`RendererError`); a PDF, a 404 or a thin body comes back as the result it
is, with a hint on thin bodies, mirroring the hard browser pin.

## Accept gates

`impersonation_blocked(result)`: `cloudflare_mitigated` or `waf_challenge`
warning, hard-block status, generic bot wall, vendor block, Cloudflare
challenge body, or an antibot signal that is blocked and not
`StructuralFailure`.

`impersonation_accepted(result)`: not blocked, passes `classify_js_attempt`,
and would not itself trigger JS escalation (`needs_js_rendering` false, and
not thin-plus-executable-JS). The hop can only end a chain the ladder would
also have ended.

An accepted hop result is stamped `RenderDecision::Failover { chain: [Http,
ImpersonatedHttp], reason }` with `VendorBlock` for the wall trigger and
`NetworkError` for the transport trigger, and `credit_cost = 1`.

## Module

`crates/crw-renderer/src/impersonated.rs`, ported from upstream nearly
verbatim, whole module under `#[cfg(feature = "impersonated")]`:

- `ImpersonatedFetcher::new(timeout)` and `with_proxy(url, timeout)`, both
  fail-closed on a bad proxy.
- Pinned preset `wreq_util::Emulation::Chrome149`. No config knob.
- `safe_redirect_policy_wreq`: the wreq twin of the reqwest redirect policy,
  calling `crw_core::url_safety::validate_safe_url_blocking_resolved` with the
  same 10-hop cap.
- `fetch`: drops caller `User-Agent`, `Accept-Encoding` and `Sec-Ch-*`
  headers (the preset owns them), keeps every other header, bounds send and
  body read by the deadline, stamps the challenge header, and hands the bytes
  to the shared tail.

`http_only.rs` gains `pub(crate) fn build_http_fetch_result(url, status,
content_type_header, challenge, final_url, bytes, elapsed_ms, tier_name)`,
extracted from `HttpFetcher::fetch`: size cap, `%PDF-` sniff and relabel,
binary-body rejection, charset-aware decode, `FetchResult` assembly. The
egress-latch block stays in `HttpFetcher::fetch` because it needs the egress
provenance. `MAX_RESPONSE_BYTES` and `HTTP_CONNECT_TIMEOUT` become
`pub(crate)`.

## Renderer wiring

`FallbackRenderer` gains, under the feature:

- `impersonated: Option<Arc<dyn PageFetcher>>`, built in `new()` when
  `config.impersonated_in_chain()`, in every mode including `none` (the tier
  is an HTTP strategy, not a JS renderer). Build failure is a hard error.
- `impersonated_timeout_ms: u64`.
- `try_impersonated_hop(url, headers, deadline, trigger) -> Option<FetchResult>`.
- `impersonation_blocked`, `impersonation_accepted`, `has_impersonated_tier`.
- `available_renderer_names()`: `impersonated-http` when present, then
  `js_renderer_names()`.

Shared helpers added to `lib.rs`: `is_hard_block_status`,
`is_fingerprint_vendor_wall`, `antibot_result`, `ImpersonatedTrigger`.

The tier is not in `js_renderers`, the breaker registry, `tier_timeouts` or
`has_recovery_tier`: it changes the fingerprint, not the egress IP.

The clearance cache is not consulted for the hop: a cached `cf_clearance` is
bound to the Camofox user agent, which the preset replaces.

## Types and config

- `RendererKind::ImpersonatedHttp`, serde `impersonated-http`,
  `as_str()` the same. `renderer_kind_for("impersonated-http")` maps to it.
  `credit_for` returns 1.
- `RequestedRenderer::ImpersonatedHttp`, serde `impersonated-http`,
  `pinned_name()` returns `"impersonated-http"`. New `implies_js()`: false
  for `ImpersonatedHttp` and `Auto`, true otherwise. Both pin choke points
  (crawl `single.rs`, server `state.rs`) use it for the "pinned implies JS"
  coercion.
- `RendererConfig.impersonated: ImpersonatedConfig { enabled: bool = true,
  timeout_ms: Option<u64> }`, loaded from `[renderer.impersonated]`. Absent
  section means on with defaults.
- `impersonated_in_chain()` = `cfg!(feature = "impersonated") &&
  self.impersonated.enabled`. `impersonated_timeout()` falls back to
  `http_timeout()`. `min_deadline_for_full_ladder_ms` adds the timeout when
  in chain.

## Server and crawl

- `validate_renderer_pin` and the `single.rs` pin check validate against
  `available_renderer_names()`. An `impersonated-http` pin is validated
  regardless of the resolved `renderJs` (it is a transport choice).
- `renderJs: true` together with the pin is rejected with 400 on scrape and
  crawl: "renderer 'impersonated-http' never executes JS; remove
  renderJs:true (or omit it)". The fork has no screenshot format, so no
  screenshot guard.
- The per-request proxy and per-request stealth path in `single.rs` builds a
  temporary `HttpFetcher` and bypasses the shared renderer. It does not hop.
  Known gap, documented in `configuration.md`.
- MCP tool schemas (`crw_scrape`, `crw_crawl`) add `"impersonated-http"` to
  the `renderer` enum with a one-line description. The schema size guard test
  is raised if it trips.
- OpenAPI: add the value to both `renderer` enums with the upstream
  description.

## Build

- Workspace: `wreq = { version = "0.16", features = ["gzip", "brotli",
  "zstd", "deflate", "charset", "socks"] }`, `wreq-util = "0.2"`.
- Features: `crw-core` `impersonated = []`; `crw-renderer` `impersonated =
  ["crw-core/impersonated", "dep:wreq", "dep:wreq-util"]`; `crw-server`
  `impersonated = ["crw-renderer/impersonated"]`; `crw-cli` `impersonated =
  ["crw-renderer/impersonated", "crw-server?/impersonated"]`.
- Dockerfile: install `cmake clang libclang-dev` in the builder; add
  `CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc` and
  `CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++`; build with
  `--features cdp,camofox,impersonated`.
- CI: the feature-matrix step adds `crw-renderer/impersonated` and
  `crw-server/impersonated`. The runner needs cmake and clang (ubuntu-latest
  ships both).
- Default `cargo build` and `make check` stay BoringSSL-free.

## Tests

- Module unit tests (wiremock): clean 200 accepted with `rendered_with =
  "impersonated-http"`; interstitial body returned verbatim; SSRF redirect
  blocked; safe redirect followed with `final_url`; challenge header stamped;
  fingerprint headers dropped, custom headers kept; bad proxy fails closed.
- Ladder tests in `lib.rs`: default config constructs the tier; `enabled =
  false` leaves none; wall result hops before JS; clean site never consults
  the hop; still-blocked hop continues to the ladder; SPA shell skips the hop;
  hop body that needs JS continues to the ladder; pinned thin body warns;
  pinned 404 returns; pinned hard-block status errors; pinned wall body
  errors; pin without the tier errors.
- Config tests: `[renderer.impersonated]` parses, defaults on, deadline sum
  includes the timeout.
- Server tests: pin accepted with the feature, 400 without it; `renderJs:
  true` plus pin rejected on scrape and crawl.
- `#[ignore]` live tests: Amazon.it product page via the pin and via the
  auto chain; clean sites stay on the plain tier.

## Docs

- `config.default.toml`: `[renderer.impersonated]` block, ladder comment.
- `docs/docs/configuration.md` and `docs/docs/js-rendering.md`: the tier,
  the pin, the known per-request-proxy gap.
- `CHANGELOG.md` Unreleased: Added entry.
- README: ladder rows and the Mermaid diagram.

## Out of scope

- A preset config knob. Only Chrome149 is verified against live walls.
- Hopping on the per-request proxy path.
- Replacing the plain reqwest tier.
