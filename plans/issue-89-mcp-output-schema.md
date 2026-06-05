# Issue #89 — crw MCP outputSchema mismatch: implementation plan

## Problem

The crw MCP server declares an `outputSchema` for exactly one tool (`crw_search`, `crates/crw-mcp-proto/src/lib.rs:248-273`) but the shared choke point `tool_result_response` (`lib.rs:334-355`) wraps *every* tool result into a plain text content block (`content:[{type:"text", text:"<json>"}]`) and never emits `structuredContent`. Strict MCP clients (Hermes-class) that validate `result.structuredContent` against the declared `outputSchema` find nothing to validate and reject `crw_search`. There are two independent defects: (1) `structuredContent` is never emitted, and (2) the declared schema is **factually wrong** — it claims `data` is a flat array, but `call_tool`'s `crw_search` branch serializes `SearchResponse = ApiResponse<SearchResponseData>` (`crw-core/src/types.rs:1156`), which produces `{ success, data: { results: <array | {web,news,images}>, answer?, citations?, llmUsage?, warnings? } }`, where `data.results` is an untagged enum (`types.rs:1112-1117`) that is an array when `sources` is unset and an object when set. Fixing either defect alone leaves the bug live.

## Decision summary

- **Option B, done strictly.** Emit real `structuredContent` *and* rewrite the schema to match crw's actual `/v1/search` serializer. Both are required; neither suffices alone.
- **Bump `PROTOCOL_VERSION` from `"2024-11-05"` to `"2025-06-18"`** (`lib.rs:8`). `outputSchema`/`structuredContent` for tools were introduced in MCP revision 2025-06-18 (PR #371); there is no per-feature capability flag (crw declares `capabilities.tools:{}`). The only spec-legal way to emit them is to advertise the revision that defines them. Emitting them under `2024-11-05` is the rejected worst-of-both-worlds half-measure.
- **Single source of truth.** Add `tool_output_schema(tool_name) -> Option<Value>` derived from `tool_definitions`, and gate emission on it — no hardcoded `"crw_search"` literal, so a future tool that declares a schema lights up automatically.
- **Tolerate, never reject** the new mandatory `MCP-Protocol-Version` HTTP header (PR #548). Read for observability; do not reject on presence/absence/mismatch. JSON-RPC batching (removed in 2025-06-18) was never implemented — verified no-op.
- **Scope is surgical: `crw_search` only.** crw-browse is a separate rmcp-SDK server that pins its own protocol version (`crw-browse/src/server.rs:378`), does **not** consume `crw-mcp-proto::PROTOCOL_VERSION` (verified: 0 matches), and stays on 2024-11-05.

## Changes

### 1. `crates/crw-mcp-proto/src/lib.rs:8` — bump protocol version
```rust
/// MCP spec revision. Bumped to 2025-06-18 (from 2024-11-05) to legitimize
/// tool `outputSchema` + result `structuredContent`, introduced in that
/// revision (PR #371). Propagates to the `initialize` handshake at lib.rs:310.
/// NOTE: crw-browse is a separate rmcp-based MCP server pinning its own
/// ProtocolVersion (crw-browse/src/server.rs:378); it does NOT consume this
/// constant and intentionally stays on 2024-11-05.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
```

### 2. `crates/crw-mcp-proto/src/lib.rs:252-274` — rewrite the `crw_search` `outputSchema` to the real nested shape
Replace the flat-array block with the envelope the server actually serializes. `data.results` uses `oneOf(array, grouped-object)`. **No `additionalProperties:false` anywhere** (`SearchResult` conditionally emits `publishedDate/markdown/html/rawHtml/links/metadata/summary`; the envelope carries `answer/citations/llmUsage/warnings/error/errorCode/warning`). Text-result items (flat `results`, grouped `web`, grouped `news`) all `$ref` a single `searchResultItem` `$def`; grouped `images` are **deliberately left unconstrained** (they are `ImageResult` — `imageUrl`, no `snippet`).

```jsonc
"outputSchema": {
  "type": "object",
  "$defs": {
    "searchResultItem": {
      "type": "object",
      "properties": {
        "url": {"type":"string"}, "title": {"type":"string"},
        "description": {"type":"string"}, "snippet": {"type":"string"},
        "position": {"type":"integer"}, "score": {"type":"number"},
        "category": {"type":"string"}
      },
      "required": ["url","title","description","snippet","position"]
    }
  },
  "properties": {
    "success": { "type": "boolean" },
    "data": {
      "type": "object",
      "properties": {
        "results": {
          "oneOf": [
            { "type": "array", "items": { "$ref": "#/$defs/searchResultItem" } },
            { "type": "object",
              "properties": {
                "web":  { "type": "array", "items": { "$ref": "#/$defs/searchResultItem" } },
                "news": { "type": "array", "items": { "$ref": "#/$defs/searchResultItem" } },
                // IMAGES ARE A DIFFERENT SHAPE — DO NOT $ref searchResultItem.
                // Grouped images deserialize to crw_core::types::ImageResult
                // (types.rs:1082): { url, title, description, imageUrl, position,
                // thumbnailUrl?, imageFormat?, resolution? } — NO `snippet`,
                // carries `imageUrl`. $ref-ing searchResultItem would FALSELY
                // reject every real grouped image (required `snippet` absent).
                "images": { "type": "array" }
              }
              // No `required`: GroupedSearchData fields are all
              // Option + skip_serializing_if (types.rs:1100-1105), so an empty
              // grouped response serializes to `{}` and MUST validate.
            }
          ]
        },
        "answer":   { "type": "string" },
        "citations":{ "type": "array" },
        "llmUsage": { "type": "object" },
        "warnings": { "type": "array", "items": { "type": "string" } }
      },
      "required": ["results"]
    },
    "error":     { "type": "string" },
    "errorCode": { "type": "string" },
    "warning":   { "type": "string" }
  },
  "required": ["success", "data"]
}
```
Required item fields are exactly those `SearchResult` guarantees (verified `types.rs:1046-1055`): `url/title/description/position` are non-`Option`; `snippet` is `#[serde(default)]` so always serialized. `score`/`category` are `Option + skip_serializing_if` → stay **optional**. `data` is required because `ApiResponse::ok` always sets `data: Some(..)` (`types.rs:522-526`), and the Ok branch of `tool_result_response` is only reached for successful calls (errors return `Err(String)` and never serialize `data`).
> **`$ref` fallback:** if the dev-side `jsonschema` validator (change 10) cannot resolve local `$ref`, inline the identical `searchResultItem` object at the flat-array and `web`/`news` sites (NOT at `images`). The contract — same 5-field required list for text results everywhere, images unconstrained — is what matters; `$defs` is just the DRY mechanism.

### 3. `crates/crw-mcp-proto/src/lib.rs:196` — fix the description prose
Change the inline `Return shape: { "success": true, "data": [{...}] }` to the real nested shape `{ "success": true, "data": { "results": [{ "url","title","description","snippet","position","score" }, ...] } }`. Keep the existing `snippet`-is-an-alias note.

### 4. `crates/crw-mcp-proto/src/lib.rs` — add single-source-of-truth helper
Insert after `tool_definitions` (before `handle_protocol_request`):
```rust
/// Returns the declared `outputSchema` for a tool, if any. Single source of
/// truth: structuredContent emission is derived from the declaration, so the
/// two cannot drift. Recomputes tool_definitions per call (tools/call is not
/// hot); memoize behind OnceLock only if profiling ever demands it.
pub fn tool_output_schema(tool_name: &str) -> Option<Value> {
    tool_definitions(false)["tools"]
        .as_array()?
        .iter()
        .find(|t| t["name"] == tool_name)
        .and_then(|t| t.get("outputSchema").cloned())
}
```

### 5. `crates/crw-mcp-proto/src/lib.rs:334-355` — make `tool_result_response` schema-aware (dual-emit)
New signature (arity change — compiler enforces all three call sites):
```rust
pub fn tool_result_response(id: Value, tool_name: &str, result: Result<Value, String>) -> JsonRpcResponse
```
- **Ok branch (single-source invariant):** build the structured `value` once. Serialize that *same* binding into the text block (`content:[{type:"text", text: to_string_pretty(&value)}]`) **and**, iff `tool_output_schema(tool_name).is_some() && value.is_object()`, attach top-level `"structuredContent": value.clone()`. Both representations derive from one binding, so `from_str(content[0].text) == structuredContent` holds by construction. The text block is unconditional (backward compat); `structuredContent` is additive.
  - **The `&& value.is_object()` guard is load-bearing — DO NOT DROP.** Spec requires `structuredContent` to be a JSON object. In proxy mode a tool that declares a schema may yield a non-object Ok value (upstream HTML error string, plain string, or legacy top-level flat array); the guard degrades gracefully to text-only instead of shipping `structuredContent: "<html>"` to a strict client. Locked by test T2b. Add the comment: *"Proxy version-skew safety valve — locked by T2b. Do NOT remove the is_object() guard."*
- **Err branch:** unchanged — `{content:[{text}], isError:true}`, **no** `structuredContent`. `isError` signals failure; strict clients must not validate `outputSchema` against error results. Keeps existing error tests green.

### 6-8. Update all three call sites (arity change is compiler-enforced)
- `crates/crw-server/src/routes/mcp.rs:136`: `Some(tool_result_response(id, tool_name, result))` (`tool_name` bound at 128).
- `crates/crw-mcp/src/main.rs:115`: `Some(tool_result_response(id, tool_name, result))` (`tool_name` bound at 107). Covers embedded **and** proxy backends.
- `crates/crw-cli/src/commands/mcp.rs:87`: `Some(tool_result_response(id, tool_name, result))` (`tool_name` bound at 79). **The site the issue summary missed.**

### 9. `crates/crw-server/src/routes/mcp.rs` `mcp_handler` (155-) — tolerate the `MCP-Protocol-Version` header
`headers: HeaderMap` is already a parameter (`mcp.rs:157`). After the existing content-type check (161-), add:
```rust
// MCP 2025-06-18 (PR #548) requires clients to send MCP-Protocol-Version on
// every post-initialize request. We TOLERATE it: read for observability, never
// reject on presence, absence, or mismatch. Hard validation deferred until
// client adoption is confirmed. Do NOT add a reject branch without updating T10.
let _client_protocol = headers
    .get("mcp-protocol-version")
    .and_then(|v| v.to_str().ok());
```
Leave the content-type validation untouched.

### 10. Add `jsonschema` dev-dependency to two crates
- `crates/crw-mcp-proto/Cargo.toml` `[dev-dependencies]` — for T4 (hand-built fixture, fast crate-local guard).
- `crates/crw-server/Cargo.toml:46` `[dev-dependencies]` — for T12 (real-serializer validation). crw-server already depends on `crw-core` (`Cargo.toml:22`), which re-exports `crw-mcp-proto` via `crw_core::mcp::*`, so T12 reaches both the real `SearchResponse` type and `tool_output_schema`/`tool_result_response` from one crate.

### 11. Sync docs + generated mirror (the bump turns published docs into a lie)
- `docs/docs/mcp.md:285` (client proposal in Verify-Installation request): `2024-11-05` → `2025-06-18`.
- `docs/docs/mcp.md:296` (expected `protocolVersion`): `2024-11-05` → `2025-06-18`.
- `docs/docs/mcp.md:325` (`Protocol version:`): `2024-11-05` → `2025-06-18`.
- `docs/docs/mcp.md` crw_search section: add one line that crw_search now emits MCP-2025-06-18 `structuredContent` mirroring `{ success, data: { results: [...] } }`, alongside the legacy text block.
- `docs/mcp/index.html:731, 739, 756` (generated mirror): update the three `2024-11-05` literals; regenerate if docs build from source.

### 12. `COMPATIBILITY-firecrawl.md` §7 — one-line note
crw_search now additionally emits MCP-2025-06-18 `structuredContent` shaped to crw's own `/v1/search` envelope (`data.results`), explicitly **not** Firecrawl's `data.web` shape.

## Test matrix

All emission paths funnel through the single `tool_result_response` choke point, so the unit tests in `crw-mcp-proto` cover all three dispatch paths by construction; the arity change makes any bypassing call site a compile error.

| ID | File | Assertion |
|---|---|---|
| **T1** | `crw-mcp-proto/src/lib.rs` `#[cfg(test)]` | `tool_result_response(id, "crw_search", Ok(repr))`: `content[0].text` present; `structuredContent` present; **`from_str(content[0].text) == structuredContent`** (single-source invariant); `structuredContent == repr` (not mutated). |
| **T2** | same | `tool_name="crw_scrape"`, `Ok(obj)`: `content[0].text` present, `structuredContent` **absent** (schema-gate). |
| **T2b** | same | Proxy-skew valve: `Ok(json!("upstream error string"))` and `Ok(json!([{"url":"x"}]))` through `"crw_search"` → text block carries the body, `structuredContent` **absent**. Locks the `is_object()` guard against refactor. |
| **T3** | same | `Err("boom")`: `isError:true`, `content[0].text=="boom"`, no `structuredContent`. |
| **T4** | same | jsonschema (hand-built): validate emitted `structuredContent` for flat `repr` **and** a grouped value (`web`/`news` items via shared `search_result_item(idx)` builder, `images` present) against `tool_output_schema("crw_search")`. |
| **T5** | same | `tool_output_schema("crw_search").is_some()`; its `required == ["success","data"]`; `data.required` contains `results`; `data` is `type:object`; `tool_output_schema("crw_scrape").is_none()`. |
| **T6** | same | Existing schema tests + `schemas_do_not_set_additional_properties_false` (`lib.rs:465`) stay green; confirm that guard is scoped to `inputSchema` and is **not** extended to the new `outputSchema`. |
| **T7** | `crw-server/tests/mcp.rs:40` | Tighten `is_string()` → `assert_eq!(result["protocolVersion"], "2025-06-18")`. |
| **T8** | same | tools/list: exactly 5 tools, same names; `crw_search` carries the nested-shape `outputSchema`. |
| **T9** | same | tools/call `crw_search` under empty test config → deterministic `search_disabled` Err → `isError:true`, `content[0].text` set, no `structuredContent`. |
| **T10** | same | `MCP-Protocol-Version` tolerance: POST `/mcp` with (a) `2025-06-18` → ok; (b) no header → ok; (c) `2024-11-05` and `not-a-version` → **still ok**. |
| **T11** | same | Existing error tests unchanged: unknown-tool `isError` (`mcp.rs:157`), crawl renderer-unavailable text (`mcp.rs:235`), `-32601`/`-32700`. |
| **T12** | `crw-server/tests/mcp.rs` | **Real-serializer gate.** Build real `crw_core::types::SearchResponse` via `ApiResponse::ok(SearchResponseData{..})`, `serde_json::to_value`, run through the real emit path, validate emitted `structuredContent` against `tool_output_schema("crw_search")` with `jsonschema`. Cases: **A** flat populated (array); **B** flat empty (`[]`); **C** grouped web+news+images (proves `ImageResult` stays unconstrained); **D** grouped empty → assert `data.results == {}` and schema accepts it. |

Shared fixtures: `representative_search_value()` (asserts `snippet == description` and `snippet.is_string()` for every item) and `search_result_item(idx)` (all five required fields, `snippet == description`), used by both flat and grouped fixtures.

**Route-level success path:** covered-by-construction (single choke point + compiler-enforced arity + T1/T4/T12), not by an executing route round-trip — standing up a SearXNG stub for one route is disproportionate. Belt-and-suspenders stub deferred unless requested.

**Out of scope:** `crw-conformance/` (Python /v2 REST harness, zero MCP coverage — verified). Do not add MCP cases there; confirm `run.sh` stays green.

**Workspace gate:** `cargo test --workspace`, `cargo fmt`, `cargo clippy` all green.

## Backward compatibility & Firecrawl parity

- **Lenient/older clients: zero breakage.** `structuredContent` is purely additive; `content[0].text` is retained verbatim and is the serialization of the same value (T1 locks this).
- **Strict Hermes-class clients: now pass.** They find `result.structuredContent` and validate it against the corrected nested schema, which the real serde value satisfies on every branch (T12: flat, empty-flat, grouped, empty-grouped). Both original failure axes are eliminated.
- **Hard-gate honesty.** The rewritten schema becomes a real gate the moment `structuredContent` is emitted. Required text-item fields are minimal and all `SearchResult`-guaranteed; `score`/`category` optional; grouped `images` (`ImageResult`) unconstrained. No previously-silent pass converts to a hard failure (T12 proves the real serializer validates on every branch).
- **Schema mirrors crw's OWN serializer, not Firecrawl's.** crw nests results under `data.results` (array in the default no-sources case; object with `web`/`news`/`images` in the grouped case); Firecrawl nests under `data.web` (verified `crw-conformance/fixtures/firecrawl_v2/search_basic.json`). This plan does **not** claim Firecrawl-MCP-shape parity — it is correct relative to crw's `/v1/search` body, the contract crw clients already consume.
- **No Firecrawl-parity regression.** `COMPATIBILITY-firecrawl.md` §7 is capability-level only and makes no response-shape claim. firecrawl-mcp-server uses plain text blocks; crw retains its text block, so lenient-text-convention parity is preserved. §7 gets the one-line note (change 12).
- **Protocol bump is client-visible.** A client that doesn't support `2025-06-18` SHOULD disconnect. Verified: no in-workspace consumer of `crw-mcp-proto::PROTOCOL_VERSION` hard-pins `2024-11-05` (the only constant is `lib.rs:8`); crw clients negotiate by echoing the server's chosen version. crw-browse (separate rmcp server) is unaffected.

## Rollout & release

- **Commit (single PR, conventional):** `feat(mcp): emit structuredContent for crw_search; bump protocol to 2025-06-18`. release-please opens/updates the Release PR → MINOR bump **0.11.0 → 0.12.0**. Never bump version or edit CHANGELOG manually.
- **Pre-commit (via `/commit`):** `cargo fmt`, `cargo clippy`, `cargo test --workspace` green, **plus** the repo-wide stale-version gate, which must return **zero** lines:
  ```bash
  grep -rn "2024-11-05" --include="*.rs" --include="*.md" --include="*.html" . | grep -v "crates/crw-browse/"
  ```
  (crw-browse allowlisted as the unrelated rmcp server.)
- **CHANGELOG note** (release-please-owned): "crw_search MCP tool now emits spec-compliant `structuredContent` (MCP 2025-06-18); `initialize` protocol version bumped from 2024-11-05 to 2025-06-18. structuredContent matches crw's `/v1/search` envelope (`{success, data:{results}}`), not Firecrawl's `data.web` shape. In proxy mode, structuredContent is sourced from the upstream `/v1/search` body — use matching minor versions across a proxy boundary. The separate crw-browse MCP server is unaffected and remains on 2024-11-05."
- **Release-checklist item:** before merging the Release PR, confirm any out-of-workspace `crw-saas`/`crw-client.ts` consumer's `initialize` handshake does not hard-pin `protocolVersion: "2024-11-05"`. No code fallback added absent an identified pinned client.
- No data migration, no config change. Stdio and HTTP both updated via the shared choke point.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Protocol bump disconnects a client that hard-pins 2024-11-05 | Verified no in-workspace pinned consumer; crw clients echo the server's version. Release-checklist confirms out-of-workspace clients. |
| Proxy version-skew: remote `/v1/search` body predates the 0.12.0 nested shape | Structural `is_object()` guard (locked by T2b) blocks non-object bodies. Residual: an object-envelope whose `data` is a legacy flat array passes the gate but fails strict validation — stated support boundary; documented in code comment + CHANGELOG ("use matching minor versions or run embedded"). |
| Hand-built test fixtures drift from the real serializer (the original #89 root cause) | T12 validates the **real** `SearchResponse` serializer output (untagged enum, camelCase, every `skip_serializing_if`) against the declared schema; T4 is the cheap crate-local mirror. |
| `jsonschema` validator can't resolve local `$ref`/`$defs` | Documented inline fallback: inline the identical `searchResultItem` at the flat/`web`/`news` sites (not `images`). |
| Stale `2024-11-05` left in a doc/mirror | Repo-wide grep gate in pre-commit must return zero lines. |
| Forgetting one of three call sites | Arity change on `tool_result_response` makes any missed site a compile error. |

## Open questions

None. All forced choices are made: `results` polymorphism kept via `oneOf` with images unconstrained; other four tools out of scope (the helper makes `crw_map` a one-line follow-up); header tolerated not validated; `jsonschema` added to both crates; `is_object()` guard kept and tested; crw-browse stays on 2024-11-05; route-level success path covered-by-construction.

Plan file location for the implementing engineer: this plan is self-contained above; no separate file was written. Load-bearing source references: `crates/crw-mcp-proto/src/lib.rs` (lines 8, 196, 252-274, 334-355), `crates/crw-core/src/types.rs` (lines 507-519, 1043-1117, 1153-1156), `crates/crw-server/src/routes/mcp.rs` (lines 24, 112, 127-136, 155-161), `crates/crw-mcp/src/main.rs:115`, `crates/crw-cli/src/commands/mcp.rs:87`, `crates/crw-server/tests/mcp.rs:40`.
