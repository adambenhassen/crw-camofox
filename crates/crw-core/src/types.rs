use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;
use uuid::Uuid;

/// Supported output formats.
///
/// `"extract"` and `"llm-extract"` are accepted as aliases for `Json`
/// during deserialization (Firecrawl compatibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    Markdown,
    Html,
    RawHtml,
    PlainText,
    Links,
    Images,
    Json,
    Summary,
    ChangeTracking,
}

impl OutputFormat {
    /// Parse a single format token, accepting the Firecrawl-compatible aliases
    /// (`extract`/`llm-extract` → `json`, `change-tracking` → `changeTracking`).
    ///
    /// Shared by the v1 string deserializer below and the v2 `FormatSpec`
    /// parser (`routes/v2/formats.rs`) so the accepted token set and the error
    /// wording stay byte-identical across API versions.
    pub fn parse_loose(s: &str) -> Result<Self, String> {
        match s {
            "markdown" => Ok(OutputFormat::Markdown),
            "html" => Ok(OutputFormat::Html),
            "rawHtml" => Ok(OutputFormat::RawHtml),
            "plainText" => Ok(OutputFormat::PlainText),
            "links" => Ok(OutputFormat::Links),
            "images" => Ok(OutputFormat::Images),
            "json" | "extract" | "llm-extract" => Ok(OutputFormat::Json),
            "summary" => Ok(OutputFormat::Summary),
            "changeTracking" | "change-tracking" => Ok(OutputFormat::ChangeTracking),
            other => Err(format!(
                "Unknown format '{other}'. Valid formats: markdown, html, rawHtml, plainText, links, images, json, summary, changeTracking \
                 (aliases: extract, llm-extract, change-tracking). Use formats: [\"json\"] with jsonSchema for structured extraction."
            )),
        }
    }
}

impl<'de> Deserialize<'de> for OutputFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        OutputFormat::parse_loose(&s).map_err(serde::de::Error::custom)
    }
}

/// Strategy for chunking text content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum ChunkStrategy {
    /// Split on sentence boundaries (.!?). Merges short chunks up to max_chars.
    #[serde(rename = "sentence")]
    Sentence {
        #[serde(default, alias = "maxChars")]
        max_chars: Option<usize>,
        #[serde(default, alias = "overlapChars")]
        overlap_chars: Option<usize>,
        #[serde(default)]
        dedupe: Option<bool>,
    },
    /// Split on a regex pattern.
    #[serde(rename = "regex")]
    Regex {
        pattern: String,
        #[serde(default, alias = "maxChars")]
        max_chars: Option<usize>,
        #[serde(default, alias = "overlapChars")]
        overlap_chars: Option<usize>,
        #[serde(default)]
        dedupe: Option<bool>,
    },
    /// Split on markdown headings (h1-h6).
    #[serde(rename = "topic")]
    Topic {
        #[serde(default, alias = "maxChars")]
        max_chars: Option<usize>,
        #[serde(default, alias = "overlapChars")]
        overlap_chars: Option<usize>,
        #[serde(default)]
        dedupe: Option<bool>,
    },
}

/// Filtering mode for ranked chunk retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FilterMode {
    Bm25,
    Cosine,
}

/// Per-request renderer override. Sibling to `renderJs` for finer control.
///
/// `Auto` is equivalent to omitting the field — uses the configured fallback chain.
/// Other variants hard-pin to a specific renderer with no fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestedRenderer {
    Auto,
    Lightpanda,
    Chrome,
    /// Residential-proxy Chrome tier — egresses through the DataImpulse
    /// pool. `rename_all = "lowercase"` would yield `"chromeproxy"`, so the
    /// variant is renamed explicitly to match the internal renderer name
    /// (`"chrome_proxy"`) and `RendererKind::ChromeProxy`.
    #[serde(rename = "chrome_proxy")]
    ChromeProxy,
    Playwright,
    /// Camofox (Firefox/Camoufox) heavy/stealth tier.
    Camofox,
}

impl RequestedRenderer {
    /// Returns `Some(name)` for renderers that should be hard-pinned in dispatch.
    /// `Auto` returns `None` — equivalent to omitting the field.
    pub fn pinned_name(self) -> Option<&'static str> {
        match self {
            RequestedRenderer::Auto => None,
            RequestedRenderer::Lightpanda => Some("lightpanda"),
            RequestedRenderer::Chrome => Some("chrome"),
            RequestedRenderer::ChromeProxy => Some("chrome_proxy"),
            RequestedRenderer::Playwright => Some("playwright"),
            RequestedRenderer::Camofox => Some("camofox"),
        }
    }
}

/// Firecrawl-compatible extraction options (used via `extract: { schema: {...} }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractOptions {
    #[serde(default)]
    pub schema: Option<serde_json::Value>,
}

/// POST /v1/scrape request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeRequest {
    pub url: String,
    #[serde(default = "default_formats")]
    pub formats: Vec<OutputFormat>,
    #[serde(default = "default_true", alias = "only_main_content")]
    pub only_main_content: bool,
    /// null = auto-detect, true = force JS, false = skip JS
    #[serde(alias = "render_js")]
    pub render_js: Option<bool>,
    /// Milliseconds to wait after JS rendering.
    #[serde(alias = "wait_for")]
    pub wait_for: Option<u64>,
    #[serde(default, alias = "include_tags")]
    pub include_tags: Vec<String>,
    #[serde(default, alias = "exclude_tags")]
    pub exclude_tags: Vec<String>,
    #[serde(alias = "json_schema")]
    pub json_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// CSS selector to narrow content before extraction.
    #[serde(default, alias = "css_selector")]
    pub css_selector: Option<String>,
    /// XPath expression to narrow content before extraction.
    #[serde(default)]
    pub xpath: Option<String>,
    /// Strategy for chunking the extracted markdown.
    #[serde(default, alias = "chunk_strategy")]
    pub chunk_strategy: Option<ChunkStrategy>,
    /// Query string for BM25/cosine chunk filtering.
    #[serde(default)]
    pub query: Option<String>,
    /// Filtering algorithm to rank chunks against query.
    #[serde(default, alias = "filter_mode")]
    pub filter_mode: Option<FilterMode>,
    /// Number of top chunks to return (default: 5).
    #[serde(default)]
    pub top_k: Option<usize>,
    /// Per-request proxy URL (overrides global config).
    /// Supports HTTP, HTTPS, and SOCKS5
    /// (e.g. "http://proxy:8080" or "socks5://user:pass@proxy:1080").
    #[serde(default)]
    pub proxy: Option<String>,
    /// 2-letter ISO 3166-1 alpha-2 country code (e.g. "us", "gb") for the
    /// residential-proxy chrome tier's egress. When the server has
    /// DataImpulse credentials configured, the engine composes
    /// `<base>__cr.<country>` per request and supplies it via CDP
    /// `Fetch.authRequired`. Unset / empty = server default country (or
    /// global pool when no default configured). Validated server-side;
    /// invalid values fall back to default.
    #[serde(default)]
    pub country: Option<String>,
    /// Override stealth mode for this request (None = use global config).
    #[serde(default)]
    pub stealth: Option<bool>,
    /// Unsupported Firecrawl parameter — captured to return a clear error.
    #[serde(default)]
    pub actions: Option<serde_json::Value>,
    /// Firecrawl-compatible `extract` object (e.g. `{ "schema": {...} }`).
    /// If `extract.schema` is set and `jsonSchema` is not, uses `extract.schema` as the schema.
    #[serde(default)]
    pub extract: Option<ExtractOptions>,
    /// Per-request LLM API key for structured extraction (BYOK).
    #[serde(default, alias = "llm_api_key")]
    pub llm_api_key: Option<String>,
    /// Per-request LLM provider override ("anthropic" or "openai").
    #[serde(default, alias = "llm_provider")]
    pub llm_provider: Option<String>,
    /// Per-request LLM model override.
    #[serde(default, alias = "llm_model")]
    pub llm_model: Option<String>,
    /// Per-request LLM base URL override (OpenAI-compatible providers
    /// like DeepSeek). Example: `"https://api.deepseek.com/v1"`.
    #[serde(default, alias = "base_url")]
    pub base_url: Option<String>,
    /// Optional user-supplied instructions appended to the summary system
    /// prompt (e.g. "respond in Turkish", "focus on technical details").
    /// The opencore's prompt-injection defense (UNTRUSTED delimiter,
    /// "ignore imperative content" rule) is kept intact — this only adds
    /// directives, it does not replace the safety wrapper. Capped at
    /// 500 chars server-side to bound token amplification.
    #[serde(default, alias = "summary_prompt")]
    pub summary_prompt: Option<String>,
    /// Maximum number of bytes of scraped content sent to the LLM for the
    /// `summary` format. Defaults to `[extraction.llm].max_html_bytes`
    /// (100 KB out of the box). Clamped to a 200 KB server-side ceiling
    /// regardless of value — protects against runaway provider bills.
    #[serde(default, alias = "max_content_chars")]
    pub max_content_chars: Option<usize>,
    /// Pin this request to a specific renderer. `None` or `Auto` = use the
    /// configured chain. Hard-pin: pinned renderer failures surface as errors,
    /// no silent fallback to a different renderer or HTTP. Pinning a non-Auto
    /// value implies `renderJs:true` unless `renderJs:false` is set explicitly.
    #[serde(default)]
    pub renderer: Option<RequestedRenderer>,
    /// End-to-end deadline budget in milliseconds. When unset, the configured
    /// `request.deadline_ms_default` (8000) applies. The SLO p95 metric is
    /// computed only over requests with `deadline_ms <= 8000`; longer values
    /// land in a separate slow-path histogram. Must be in `(0, 60000]`.
    #[serde(default, alias = "deadline_ms")]
    pub deadline_ms: Option<u64>,
    /// Opt-in extraction debug trace. When true, the response includes a
    /// `debugExtraction` field describing every candidate the extractor
    /// considered and why one was selected.
    #[serde(default)]
    pub debug: Option<bool>,
    /// Change-tracking options. Activated when `formats` contains
    /// `"changeTracking"`. Carries the diff modes, an optional extraction
    /// schema/prompt for json mode, and the caller-supplied `previous`
    /// snapshot to diff the current scrape against. Sibling field — mirrors
    /// the precedented `extract` / `jsonSchema` pattern (the `formats` entry
    /// is the plain string `"changeTracking"`, options ride here).
    #[serde(default, alias = "change_tracking")]
    pub change_tracking: Option<ChangeTrackingOptions>,
    /// Plain-language monitor goal used by the meaningful-change judge.
    /// Capped server-side at 2 KB. The judge only runs when both `goal` is
    /// present and `judgeEnabled` is true (and the page actually changed).
    #[serde(default)]
    pub goal: Option<String>,
    /// Whether to run the LLM meaningful-change judge on a changed page.
    /// `None` is treated as "off" at the opencore layer — the SaaS
    /// orchestration decides auto-enable semantics.
    #[serde(default, alias = "judge_enabled")]
    pub judge_enabled: Option<bool>,
    /// Firecrawl-compatible document parsers. Controls how non-HTML documents
    /// (currently only PDF) are handled when a URL returns one.
    /// - `None` (field omitted) → PDFs are auto-parsed to markdown (default,
    ///   matches Firecrawl).
    /// - `Some([])` → parsing disabled; the raw document is left unconverted.
    /// - `Some([{type:"pdf"}])` → explicitly enable PDF parsing (optionally
    ///   capped via `maxPages`).
    #[serde(default)]
    pub parsers: Option<Vec<ParserSpec>>,
}

/// A document parser directive (Firecrawl `parsers` entry). Accepts either the
/// bare string form (`"pdf"`) or the object form (`{ "type": "pdf",
/// "mode": "auto", "maxPages": 10 }`) on the wire; always serializes to the
/// object form. Matches Firecrawl v2's `parsers` shape exactly.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ParserSpec {
    /// Parser type. Only `"pdf"` is supported today.
    #[serde(rename = "type")]
    pub parser_type: String,
    /// Parsing strategy (Firecrawl: `auto` | `fast` | `ocr`). fastCRW has no
    /// OCR, so `ocr` degrades to text extraction with a warning, and `auto`
    /// (text-first + OCR fallback in Firecrawl) is text-only here. Accepted for
    /// wire-compatibility regardless. `None` ≈ `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Optional cap on the number of pages to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_pages: Option<usize>,
}

impl ParserSpec {
    /// Convenience constructor for the common PDF directive.
    pub fn pdf() -> Self {
        Self {
            parser_type: "pdf".to_string(),
            mode: None,
            max_pages: None,
        }
    }
}

impl<'de> serde::Deserialize<'de> for ParserSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Obj {
                #[serde(rename = "type")]
                parser_type: String,
                #[serde(default)]
                mode: Option<String>,
                #[serde(default, rename = "maxPages", alias = "max_pages")]
                max_pages: Option<usize>,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Str(parser_type) => ParserSpec {
                parser_type,
                mode: None,
                max_pages: None,
            },
            Raw::Obj {
                parser_type,
                mode,
                max_pages,
            } => ParserSpec {
                parser_type,
                mode,
                max_pages,
            },
        })
    }
}

fn default_formats() -> Vec<OutputFormat> {
    vec![OutputFormat::Markdown]
}

impl Default for ScrapeRequest {
    /// Matches the serde defaults exactly (`formats: ["markdown"]`,
    /// `only_main_content: true`, everything else empty/None). Hand-written
    /// rather than derived because `#[derive(Default)]` would give
    /// `formats: vec![]` / `only_main_content: false`, contradicting the wire
    /// defaults — the v2 adapters build `ScrapeRequest { .., ..Default::default() }`
    /// and rely on these matching.
    fn default() -> Self {
        Self {
            url: String::new(),
            formats: default_formats(),
            only_main_content: true,
            render_js: None,
            wait_for: None,
            include_tags: Vec::new(),
            exclude_tags: Vec::new(),
            json_schema: None,
            headers: HashMap::new(),
            css_selector: None,
            xpath: None,
            chunk_strategy: None,
            query: None,
            filter_mode: None,
            top_k: None,
            proxy: None,
            country: None,
            stealth: None,
            actions: None,
            extract: None,
            llm_api_key: None,
            llm_provider: None,
            llm_model: None,
            base_url: None,
            summary_prompt: None,
            max_content_chars: None,
            renderer: None,
            deadline_ms: None,
            debug: None,
            change_tracking: None,
            goal: None,
            judge_enabled: None,
            parsers: None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Metadata about a scraped page.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct PageMetadata {
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub og_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub og_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub og_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_url: Option<String>,
    #[serde(rename = "sourceURL")]
    pub source_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered_with: Option<String>,
    pub elapsed_ms: u64,
    /// Number of pages, for paginated documents (PDF). `None` for web pages.
    /// Drives per-page billing on document scrapes / uploads. Serialized as
    /// `numPages` to match Firecrawl's metadata field name.
    #[serde(default, rename = "numPages", skip_serializing_if = "Option::is_none")]
    pub page_count: Option<usize>,
    /// Original filename for documents uploaded via `/v2/parse`. `None` for
    /// URL-sourced pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_filename: Option<String>,
}

/// Token-usage and best-effort cost for one LLM call.
///
/// `estimated_cost_usd` is informational only — provider pricing drifts
/// and this value MUST NOT be used for customer billing.
///
/// `cache_hit_input_tokens` / `cache_miss_input_tokens` surface the
/// provider's prompt-cache breakdown (Anthropic `cache_read_input_tokens`,
/// OpenAI `prompt_tokens_details.cached_tokens`, DeepSeek
/// `prompt_cache_hit_tokens`). `None` means the provider did not report a
/// breakdown for this call. `truncated` flags requests whose markdown
/// input was clipped before the LLM call. `calls` aggregates the number
/// of underlying provider calls when usage is summed across multiple
/// invocations (default 1 for a single call).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    pub model: String,
    pub provider: String,

    // ── Wave 2 additions (additive, backward-compatible via serde defaults) ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_miss_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(default = "one_u32", skip_serializing_if = "is_one_u32")]
    pub calls: u32,

    // ── Wave 4 (R1) additions: SaaS billing correlation across legs ──
    //
    // The SaaS-side managed pricing path needs to know exactly how many
    // summary calls executed AND whether the answer leg ran. The 5-branch
    // fail-closed dispatch keys off these counters:
    //   - executedSummaries > 0 OR answerExecuted ⇒ engine did work
    //   - inputTokens == 0 AND outputTokens == 0 ⇒ no upstream cost
    // Without the counters the SaaS cannot disambiguate "no work" from
    // "work but missing telemetry" and would refund or charge wrong.
    //
    // Always serialized (no skip_serializing_if) so the always-present
    // R1 invariant holds: when /v1/search returns llmUsage, both fields
    // are explicitly visible.
    #[serde(default)]
    pub executed_summaries: u32,
    #[serde(default)]
    pub answer_executed: bool,
}

fn one_u32() -> u32 {
    1
}
fn is_one_u32(n: &u32) -> bool {
    *n == 1
}

/// A single chunk with optional relevance score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkResult {
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    pub index: usize,
}

/// A single image discovered on a scraped page, returned when `formats`
/// includes `images`. `url` is resolved to an absolute URL (or kept verbatim for
/// `data:`/`blob:`); `alt` is the `<img alt>` text when available (most non-img
/// sources — meta, icons, poster, background — carry no alt).
///
/// The native `/v1` surface serializes these objects. The Firecrawl-compat
/// `/v2` surface flattens them to a plain `Vec<String>` of URLs in
/// `routes/v2/adapters.rs::to_v2_document` (Firecrawl's `images` is `string[]`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ScrapedImage {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt: Option<String>,
}

/// Data returned for a single scraped page.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ScrapeData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plain_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    /// Images discovered on the page; populated when `formats` includes
    /// `images`. Native `/v1` shape; v2 flattens to `Vec<String>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ScrapedImage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
    /// LLM-generated summary; populated when `formats` includes `summary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Token usage + best-effort cost for any LLM call this request triggered
    /// (summary, structured JSON, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_usage: Option<LlmUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<Vec<ChunkResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Soft-failure / informational warnings collected through the render
    /// chain. Empty vec serializes as missing for backward compatibility.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    /// Routing decision metadata (renderer chosen, failover chain).
    /// Surfaced for debug + UI; `None` for legacy paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub render_decision: Option<RenderDecision>,
    /// Credit cost attributed to this page (0 = not yet priced).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub credit_cost: u32,
    pub metadata: PageMetadata,
    /// Extraction debug trace; populated only when the request opts in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_extraction: Option<DebugExtraction>,
    /// MIME content type of the fetched resource (from `FetchResult`).
    /// Surfaced so change-tracking can hash binary/non-text content (PDF,
    /// images) by bytes rather than attempting a markdown/json diff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Change-tracking result; populated only when `formats` includes
    /// `"changeTracking"`. Carries per-page status + diff (+ judgment when
    /// the orchestration layer ran the judge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_tracking: Option<ChangeTrackingResult>,
    /// Why this document has no body: an anti-bot verdict stamped at the scrape
    /// choke (`single::scrape_url`), or — on the crawl and batch paths, which
    /// return documents rather than one envelope — `HTTP_ERROR_VENDOR` for an
    /// origin error page. `Some` means the caller did not get the page they
    /// asked for, so v1/v2 turn it into `success:false`.
    /// `None` (skipped when serializing) = a real page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block: Option<BlockOutcome>,
    /// The renderer snapshotted a partial DOM because the navigation budget
    /// elapsed (`FetchResult.truncated`). The content is usable but incomplete,
    /// and a caller cannot otherwise tell it apart from a page that genuinely
    /// has little content — which is what makes a shrinking scrape budget a
    /// silent recall regression.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Above this many bytes of body, a `>= 400` response is treated as the real page
/// rather than the origin's error page.
///
/// Measured, not guessed. Across one prod day (43 responses graded `success` while
/// carrying `metadata.statusCode >= 400`) the largest error page — a CentOS Apache
/// test page — rendered to 2,356 bytes of markdown, and the next-largest 4xx body
/// was 3,340. So the bar sits in that gap. Above it live real pages served under an
/// error status, which the renderer deliberately keeps (`crw_renderer` accept gate,
/// and `crw_crawl::single`: "the status is a soft signal, not a content gate"):
/// stackoverflow.com under 403 renders 39,499 chars, a YouTube watch page under 401
/// renders 12,256, a Medium article under 403 renders 10,826.
///
/// Deliberately conservative. It leaves large branded 404s (GitHub's is 3,340)
/// passing as successes; raising it is a recall decision that needs its own
/// benchmark run, not a nudge.
const ERROR_PAGE_MAX_TEXT: usize = 2_500;

impl ScrapeData {
    /// Whether any page-content field survived. A document without a body is a
    /// placeholder for a URL that produced no page (a cleared wall, or a scrape
    /// error), as opposed to an origin error page that was kept readable.
    pub fn has_body(&self) -> bool {
        self.markdown.is_some()
            || self.html.is_some()
            || self.raw_html.is_some()
            || self.plain_text.is_some()
            || self.links.is_some()
            || self.images.is_some()
            || self.json.is_some()
            || self.summary.is_some()
            || self.chunks.is_some()
    }

    /// True when the body the caller asked for is no bigger than an error page
    /// (see [`ERROR_PAGE_MAX_TEXT`]). `false` when there is nothing to measure.
    pub fn is_error_page_sized(&self) -> bool {
        self.rendered_text_len()
            .is_some_and(|n| n < ERROR_PAGE_MAX_TEXT)
    }

    /// Size of the body the caller actually asked for, in bytes.
    ///
    /// Text formats first: the previous gate took `.max()` across all four body
    /// fields, so an error page's raw HTML — always large — kept the gate from
    /// ever firing whenever `formats` included `html` or `rawHtml`.
    ///
    /// But measuring only text would be worse than the bug it fixes. A request
    /// for `formats:["rawHtml"]` populates neither text field, so a text-only
    /// measurement reads 0 and fails **every** `>= 400` response, including the
    /// large real pages that soft-block statuses are known to carry. So when the
    /// caller asked for no text at all, fall back to the markup we do hold. It
    /// discriminates less well (an error page's HTML is bulkier than its text),
    /// which is the right direction to be wrong in.
    /// `None` when the document holds no body at all — `formats:["screenshot"]`,
    /// `["links"]` and a summary-only request all populate none of these fields,
    /// and "nothing to measure" must not be read as "measured nothing".
    /// A present-but-empty field is a real measurement of 0: the caller asked for
    /// that format and the page yielded none of it.
    fn rendered_text_len(&self) -> Option<usize> {
        let text = [self.markdown.as_deref(), self.plain_text.as_deref()]
            .into_iter()
            .flatten()
            .map(str::len)
            .max();
        text.or_else(|| {
            [self.html.as_deref(), self.raw_html.as_deref()]
                .into_iter()
                .flatten()
                .map(str::len)
                .max()
        })
    }

    /// `Some(message)` when the origin answered `>= 400` and what we are holding is
    /// its error page rather than the page that was asked for.
    ///
    /// One helper for every surface: v1, v2, crawl and batch all have to agree, or
    /// the same URL is refunded on one endpoint and billed on another.
    ///
    /// A block verdict outranks the origin's status, and that ordering lives here
    /// rather than at the call sites. Cloudflare answers its challenge with 403,
    /// so without this the wall was classified `http_error` and the "Just a
    /// moment..." shell shipped as the page's markdown — `clear_body()` runs on
    /// the block path, which the status gate returned before ever reaching.
    /// Measured on prod 2026-08-24 and on 109 real customer requests across six
    /// days of traces.
    ///
    /// Three callers already worked around this by hand (`crw-crawl::crawl`,
    /// `crw-server::state`'s batch path, `crw-crawl::single`'s `unusable`), and
    /// the batch one's comment says it clears the shell "exactly as the single
    /// scrape route does" — which was not true, because the single scrape route
    /// asked this helper first. Owning the rule here makes that comment true and
    /// leaves those guards redundant but harmless.
    ///
    /// `structural_failure` is the one verdict that does NOT outrank the status,
    /// and the exclusion is deliberate: `structural_integrity_check` never reads
    /// the HTTP status, so a terse origin error page earns that verdict on its
    /// shape alone. Letting it short-circuit here would turn every small 404 into
    /// `no_usable_content` with its body cleared, when today the caller can read
    /// the error page under `http_error`.
    ///
    /// It is also exactly the vendor `crw-crawl::crawl` filters out before it
    /// asks this question, for the same stated reason — so excluding it here, and
    /// only it, is what makes the two surfaces agree. `parked_domain` is NOT
    /// excluded, precisely because crawl does not exclude it either: a parked
    /// page is a real verdict about the destination on every surface, and
    /// carving it out here would have created the cross-surface split this change
    /// exists to close.
    pub fn http_error(&self) -> Option<String> {
        if self
            .block
            .as_ref()
            .is_some_and(|b| b.vendor != STRUCTURAL_FAILURE_VENDOR)
        {
            return None;
        }
        let status = self.metadata.status_code;
        if status < 400 || self.rendered_text_len()? >= ERROR_PAGE_MAX_TEXT {
            return None;
        }
        Some(
            self.warning
                .clone()
                .unwrap_or_else(|| format!("Target returned HTTP {status}")),
        )
    }

    /// True when none of the formats the caller actually asked for produced
    /// anything.
    ///
    /// This is the last gap that let a scrape charge for a response with no
    /// content in it. Two shapes measured on prod, both `success:true`, both
    /// billed:
    ///
    /// ```text
    /// {"markdown":"", "warning":"pdf_too_large: document decompresses beyond
    ///   the allowed size (possible decompression bomb)",
    ///   "metadata":{"statusCode":200,"renderedWith":"pdf","numPages":0}}
    /// {"markdown":"", "warning":null, "warnings":null,
    ///   "metadata":{"statusCode":200,"renderedWith":"http","elapsedMs":110}}
    /// ```
    ///
    /// The second carries no warning at all, so keying on warning strings — or
    /// on a list of terminal `PdfError` codes — would miss it and would need
    /// extending every time a new extraction failure is added.
    ///
    /// Takes `formats` instead of reading `Option::is_some()` off `self`. For
    /// most fields the two agree, but `summary` collapses "requested and failed"
    /// into the same `None` as "never asked for": `crw_crawl::single` turns a
    /// `summarize()` error into a warning and leaves `summary: None`. That is
    /// exactly the empty-success case this exists to catch, and it is not
    /// visible without knowing what was asked.
    ///
    /// `ChangeTracking` is excluded on purpose: "nothing changed since
    /// `previous`" is a real answer, the same way a confirmed zero-result search
    /// is a real search. `chunks` is excluded too — it is driven by
    /// `chunk_strategy` rather than a format, and is a derived view of markdown
    /// rather than an independent ask.
    pub fn has_no_content(&self, formats: &[OutputFormat]) -> bool {
        // An explicitly empty `formats` array asked for nothing, so nothing is
        // missing. `serde`'s default only fills in `[Markdown]` when the field is
        // absent — `"formats": []` reaches here as an empty slice, and `.any()`
        // over it is vacuously false, which would fail every such scrape. `/v2`
        // rejects an empty list up front (`v2/formats.rs`); `/v1` accepts it, so
        // the guard belongs here rather than at one route.
        if formats.is_empty() {
            return false;
        }
        !formats.iter().any(|f| self.format_delivered(*f))
    }

    /// Whether the one field that carries `format` came back with something in it.
    fn format_delivered(&self, format: OutputFormat) -> bool {
        fn filled(s: Option<&str>) -> bool {
            s.is_some_and(|s| !s.trim().is_empty())
        }
        match format {
            OutputFormat::Markdown => filled(self.markdown.as_deref()),
            OutputFormat::Html => filled(self.html.as_deref()),
            OutputFormat::RawHtml => filled(self.raw_html.as_deref()),
            OutputFormat::PlainText => filled(self.plain_text.as_deref()),
            OutputFormat::Summary => filled(self.summary.as_deref()),
            // A collection that came back present-but-empty is a real answer:
            // "this page has no outbound links" is a complete result, unlike an
            // empty markdown body, which means the page never rendered. Presence
            // is the measurement; emptiness is a legitimate value of it.
            OutputFormat::Links => self.links.is_some(),
            OutputFormat::Images => self.images.is_some(),
            // An extraction that returns `{}` or `[]` found none of the schema's
            // fields; a bare number or bool is still a real answer.
            OutputFormat::Json => self.json.as_ref().is_some_and(|v| match v {
                serde_json::Value::Null => false,
                serde_json::Value::Object(m) => !m.is_empty(),
                serde_json::Value::Array(a) => !a.is_empty(),
                serde_json::Value::String(s) => !s.trim().is_empty(),
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) => true,
            }),
            // "nothing changed since `previous`" is a real answer, so the value
            // is never inspected — but its presence is, so a change-tracking run
            // that produced nothing at all is still caught.
            OutputFormat::ChangeTracking => self.change_tracking.is_some(),
        }
    }

    /// Clear the page-content fields (markdown, HTML, text, links, and any
    /// LLM-derived outputs), keeping `metadata`, `block`, and warnings. Used
    /// on the block-response path so a detected anti-bot interstitial returns
    /// a clean block (success:false + error + metadata) instead of the
    /// challenge shell text as content.
    pub fn clear_body(&mut self) {
        self.markdown = None;
        self.html = None;
        self.raw_html = None;
        self.plain_text = None;
        self.links = None;
        self.images = None;
        self.json = None;
        self.summary = None;
        self.chunks = None;
    }
}

/// Typed anti-bot block verdict. `vendor` is the antibot `class_name`
/// (cloudflare|datadome|perimeterx|generic_block|structural_failure|…);
/// `reason` is the detector's human-readable explanation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockOutcome {
    pub vendor: String,
    pub reason: String,
}

/// `AntibotSignal::StructuralFailure`'s `class_name()`. Not a vendor: it is the
/// "we got a page but there is nothing usable in it" verdict, which the classifier
/// keeps inside `AntibotSignal` because `is_blocked()` drives renderer escalation.
/// See `message()` for why the customer-facing wording splits here.
pub const STRUCTURAL_FAILURE_VENDOR: &str = "structural_failure";

/// `BlockOutcome::vendor` for an HTTP-level failure to get the page: the origin
/// answered with an error status and this is its error page, its CDN answered
/// that it could not reach the origin, or, on the crawl path, the request did
/// not complete at all and `metadata.statusCode` is `0`. Not an anti-bot
/// verdict, but the same consequence for the caller and for billing, and the
/// crawl/batch surfaces have nowhere else to say it: they return an array of
/// documents, not an envelope with an error code.
pub const HTTP_ERROR_VENDOR: &str = "http_error";

/// `BlockOutcome::vendor` for a registrar parking page, a domain-marketplace listing
/// or a default web-server vhost. Not an anti-bot verdict, but nothing the caller
/// asked for was delivered. Kept distinct from `STRUCTURAL_FAILURE_VENDOR` because
/// these pages are not thin or broken — they are just not the site.
pub const PARKED_DOMAIN_VENDOR: &str = "parked_domain";

impl BlockOutcome {
    /// Standard anti-bot block error string shared by the v1 and v2 handlers so
    /// the two API surfaces label the same block identically.
    ///
    /// `structural_failure` is deliberately worded differently. It is not a
    /// vendor wall — it fires on a thin or empty document (`antibot.rs` structural
    /// arms), which is most often a broken TLS page, an error stub, or a JS shell
    /// we could not hydrate. Reporting that as "Blocked by anti-bot" sent
    /// customers to buy proxies and stealth for what was a certificate problem
    /// (`wrong.host.badssl.com`: 21 visible characters, reported as a block).
    ///
    /// Wording only. The verdict stays inside `AntibotSignal`, `is_blocked()` is
    /// untouched, and the escalation ladder behaves identically — a thin page must
    /// still escalate to the next renderer tier, which is what `is_blocked()`
    /// drives.
    pub fn message(&self) -> String {
        // `parked_domain` rides the same wording: telling a customer their target
        // was "blocked by anti-bot" when the domain is simply for sale is what sends
        // them off to buy proxies for a site that does not exist.
        if self.vendor == STRUCTURAL_FAILURE_VENDOR || self.vendor == PARKED_DOMAIN_VENDOR {
            format!("No usable content could be extracted ({})", self.reason)
        } else {
            format!("Blocked by anti-bot ({}): {}", self.vendor, self.reason)
        }
    }
}

/// Per-request extraction debug trace. One entry per extract() call
/// (multi-attempt JS escalation produces multiple attempts).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DebugExtraction {
    pub attempts: Vec<DebugAttempt>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugAttempt {
    pub renderer: String,
    pub extracted_via: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_features: Option<serde_json::Value>,
    pub candidates: Vec<DebugCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugCandidate {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_chars: Option<usize>,
    pub score: f64,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

/// Generic API response wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T: Serialize> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl<T: Serialize> ApiResponse<T> {
    pub fn ok(data: T) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            error_code: None,
            warning: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
            error_code: None,
            warning: None,
        }
    }

    pub fn err_with_code(msg: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(msg.into()),
            error_code: Some(code.into()),
            warning: None,
        }
    }
}

// ── Crawl types ──

/// POST /v1/crawl request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrawlRequest {
    pub url: String,
    pub max_depth: Option<u32>,
    #[serde(alias = "limit", alias = "max_pages")]
    pub max_pages: Option<u32>,
    #[serde(default = "default_formats")]
    pub formats: Vec<OutputFormat>,
    #[serde(default = "default_true")]
    pub only_main_content: bool,
    #[serde(default, alias = "json_schema")]
    pub json_schema: Option<serde_json::Value>,
    /// null = auto-detect (use global default), true = force JS, false = skip JS.
    /// Applies to every page fetched during the crawl.
    #[serde(default, alias = "render_js")]
    pub render_js: Option<bool>,
    /// Milliseconds to wait after JS rendering on each page.
    #[serde(default, alias = "wait_for")]
    pub wait_for: Option<u64>,
    /// Pin every page in this crawl to a specific renderer. See `ScrapeRequest::renderer`.
    #[serde(default)]
    pub renderer: Option<RequestedRenderer>,
    /// 2-letter ISO 3166-1 alpha-2 country code (e.g. "us", "gb") applied to
    /// every page fetched in this crawl. See `ScrapeRequest::country`.
    #[serde(default)]
    pub country: Option<String>,
    /// Extra request headers applied to every page this crawl fetches, with the
    /// same semantics as [`ScrapeRequest::headers`], including its warning: on
    /// a browser render `Network.setExtraHTTPHeaders` decorates every request
    /// the page makes, subresources included, so cross-origin-sensitive
    /// credentials do not belong here.
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

/// Resolve the effective `render_js` decision from a per-request value and the
/// global default. Per-request always wins when set; otherwise fall back to the
/// default. `None` at both ends means "auto-detect".
///
/// Precedence table:
///
/// | request       | default       | effective    |
/// |---------------|---------------|--------------|
/// | `Some(true)`  | any           | `Some(true)` |
/// | `Some(false)` | any           | `Some(false)`|
/// | `None`        | `Some(true)`  | `Some(true)` |
/// | `None`        | `Some(false)` | `Some(false)`|
/// | `None`        | `None`        | `None`       |
pub fn resolve_render_js(request: Option<bool>, default: Option<bool>) -> Option<bool> {
    request.or(default)
}

/// Resolve the effective pinned renderer name from a per-request value.
///
/// Returns the renderer name (e.g. `"chrome"`) when a non-`Auto` renderer is pinned.
/// `None` and `Some(Auto)` both return `None` — meaning "use the configured chain".
pub fn resolve_pinned_renderer(req: Option<RequestedRenderer>) -> Option<&'static str> {
    req.and_then(|r| r.pinned_name())
}

/// Is this declared content type actually HTML (or HTML-like markup)?
///
/// Shared by the renderer (deciding whether a browser render can add
/// anything to a body) and the extractor (deciding whether it is safe to run
/// an HTML parser / HTML-to-markdown converter over the body at all). Absent
/// or unrecognised types stay eligible: a server that omits `Content-Type`
/// is still overwhelmingly likely to be serving HTML.
pub fn is_html_like_content_type(content_type: Option<&str>) -> bool {
    match content_type {
        None => true,
        Some(ct) => {
            let ct = ct.trim().to_ascii_lowercase();
            ct.is_empty()
                || ct == "text/html"
                || ct == "application/xhtml+xml"
                || ct == "application/xml"
                || ct == "text/xml"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully populated document, so each test can vary just the field it is about.
    fn blocked_fixture() -> ScrapeData {
        ScrapeData {
            markdown: Some("Just a moment... Humans only".into()),
            html: Some("<html>challenge</html>".into()),
            raw_html: Some("<html>challenge</html>".into()),
            plain_text: Some("challenge text".into()),
            links: Some(vec!["https://help.example.com".into()]),
            images: Some(vec![ScrapedImage {
                url: "https://help.example.com/logo.png".into(),
                alt: Some("logo".into()),
            }]),
            json: Some(serde_json::json!({"junk": true})),
            summary: Some("a summary of junk".into()),
            llm_usage: None,
            chunks: None,
            warning: Some("blocked".into()),
            warnings: vec!["blocked".into()],
            render_decision: None,
            credit_cost: 0,
            truncated: false,
            metadata: PageMetadata {
                title: None,
                description: None,
                og_title: None,
                og_description: None,
                og_image: None,
                canonical_url: None,
                source_url: "https://www.glassdoor.com/Reviews/x.htm".into(),
                language: None,
                status_code: 200,
                rendered_with: None,
                elapsed_ms: 0,
                page_count: None,
                source_filename: None,
            },
            debug_extraction: None,
            content_type: Some("text/html".into()),
            change_tracking: None,
            block: Some(BlockOutcome {
                vendor: "cloudflare".into(),
                reason: "cloudflare challenge interstitial".into(),
            }),
        }
    }

    /// An ordinary origin error page: the status is the caller's, and there is no
    /// anti-bot verdict. `block` must be cleared explicitly — the fixture this
    /// builds on is a *walled* page, and `http_error` now returns `None` for
    /// anything carrying a block, so leaving it set would make every assertion
    /// here measure the block gate instead of the status gate.
    fn page(status: u16, markdown_len: usize) -> ScrapeData {
        let mut d = blocked_fixture();
        d.block = None;
        d.metadata.status_code = status;
        d.markdown = Some("x".repeat(markdown_len));
        d.plain_text = None;
        d.html = None;
        d.raw_html = None;
        d.warning = None;
        d
    }

    #[test]
    fn has_body_separates_a_placeholder_from_a_readable_error_page() {
        let mut d = ScrapeData::default();
        assert!(!d.has_body(), "a failed_page or cleared wall has no body");
        d.markdown = Some("404 Not Found".into());
        assert!(
            d.has_body(),
            "an origin error page kept readable has a body"
        );
        d.markdown = None;
        d.links = Some(vec!["https://example.com/a".into()]);
        assert!(d.has_body(), "a links-only format still delivered a page");
    }

    #[test]
    fn error_page_sized_follows_the_error_page_bar() {
        assert!(page(200, 120).is_error_page_sized());
        assert!(!page(200, ERROR_PAGE_MAX_TEXT).is_error_page_sized());
        let mut nothing = page(200, 0);
        nothing.markdown = None;
        assert!(
            !nothing.is_error_page_sized(),
            "no body is not a measurement"
        );
    }

    #[test]
    fn http_error_fires_on_an_error_page_and_spares_a_real_one() {
        // americastire.com: 403 with 650 bytes of CloudFront prose, billed as a
        // success in prod for months because the old bar was 200 bytes.
        assert!(page(403, 650).http_error().is_some());
        // stackoverflow.com/questions/tagged/rust answers 403 and serves the real
        // page (39,499 chars). The renderer keeps it on purpose; so must this.
        assert!(page(403, 39_499).http_error().is_none());
        // A thin page that the origin says is fine stays a success.
        assert!(page(200, 2).http_error().is_none());
    }

    #[test]
    fn a_block_verdict_outranks_the_origin_status() {
        // Cloudflare answers its challenge with 403. Reporting that as
        // `http_error` returned the "Just a moment..." shell as the page's
        // markdown, because `clear_body()` only runs on the block path.
        let mut walled = page(403, 650);
        walled.block = Some(BlockOutcome {
            vendor: "cloudflare".into(),
            reason: "cloudflare challenge interstitial".into(),
        });
        assert!(walled.http_error().is_none());
        // The same page without a verdict is still an ordinary origin error.
        walled.block = None;
        assert!(walled.http_error().is_some());
    }

    #[test]
    fn has_no_content_catches_the_shapes_that_were_billed_for_nothing() {
        // pdf_too_large: the decompression-bomb guard refused the document, so
        // markdown is empty and only a warning records why.
        let mut d = page(200, 0);
        d.warning = Some("pdf_too_large: document decompresses beyond the allowed size".into());
        assert!(d.has_no_content(&[OutputFormat::Markdown]));
        // aliexpress.com, 2026-08-17: same empty markdown, no warning at all —
        // which is why this keys on the outcome and not on warning text.
        d.warning = None;
        assert!(d.has_no_content(&[OutputFormat::Markdown]));
        // A legitimately thin page delivered what was asked for.
        assert!(!page(200, 1).has_no_content(&[OutputFormat::Markdown]));
    }

    #[test]
    fn has_no_content_judges_only_the_formats_that_were_requested() {
        let mut d = page(200, 0); // markdown present but empty
        d.links = Some(vec!["https://example.com/a".into()]);
        // Asking for links and getting links is a delivered scrape, even though
        // the markdown field happens to be empty.
        assert!(!d.has_no_content(&[OutputFormat::Links]));
        // Partial delivery across a multi-format request still counts.
        assert!(!d.has_no_content(&[OutputFormat::Markdown, OutputFormat::Links]));
    }

    #[test]
    fn has_no_content_accepts_a_page_that_genuinely_has_no_links() {
        // `formats:["links"]` over a page with no outbound links returns
        // `Some([])`. That is the complete, correct answer to what was asked —
        // failing it would bill-refund a scrape that worked, and it is the one
        // shape where "empty" and "missing" must not be conflated.
        let mut d = page(200, 0);
        d.markdown = None;
        d.images = None;
        d.links = Some(Vec::new());
        assert!(!d.has_no_content(&[OutputFormat::Links]));
        // Isolated from `links`, so an arm that read the wrong field would fail
        // here instead of riding on the assertion above.
        d.links = None;
        d.images = Some(Vec::new());
        assert!(!d.has_no_content(&[OutputFormat::Images]));
        assert!(d.has_no_content(&[OutputFormat::Links]));
        // Not requested at all is still nothing delivered.
        d.images = None;
        assert!(d.has_no_content(&[OutputFormat::Images]));
    }

    #[test]
    fn a_thin_error_page_keeps_its_status_classification() {
        // `structural_integrity_check` never reads the HTTP status, so a terse
        // 404 earns a `structural_failure` verdict on shape alone. If that
        // short-circuited `http_error`, every small error page would come back
        // `no_usable_content` with its body cleared instead of readable under
        // `http_error` — and would disagree with `crawl.rs`, which filters this
        // vendor out for the same reason.
        let mut d = page(404, 300);
        d.block = Some(BlockOutcome {
            vendor: STRUCTURAL_FAILURE_VENDOR.into(),
            reason: "Structural: minimal_text, no_content_elements".into(),
        });
        assert!(d.http_error().is_some());
        // `parked_domain` is NOT carved out: crawl.rs does not filter it either,
        // and a parked page is a real verdict about the destination on every
        // surface. Carving it out here is what would split them.
        d.block = Some(BlockOutcome {
            vendor: PARKED_DOMAIN_VENDOR.into(),
            reason: "parked domain".into(),
        });
        assert!(d.http_error().is_none());
        // A real vendor wall still outranks the status.
        d.block = Some(BlockOutcome {
            vendor: "datadome".into(),
            reason: "datadome interstitial".into(),
        });
        assert!(d.http_error().is_none());
    }

    #[test]
    fn has_no_content_catches_a_format_that_silently_failed() {
        // `summarize()` failures become a warning and leave the field `None`,
        // which is indistinguishable from "not requested" without the formats
        // list — this is the whole reason `has_no_content` takes one.
        let mut d = page(200, 400);
        d.summary = None;
        assert!(d.has_no_content(&[OutputFormat::Summary]));
        // Delivered, so not empty.
        d.summary = Some("a summary".into());
        assert!(!d.has_no_content(&[OutputFormat::Summary]));
    }

    #[test]
    fn has_no_content_treats_an_unchanged_page_as_an_answer() {
        // "nothing changed since `previous`" is a real result, the same
        // precedent as a confirmed zero-result search being a real search.
        let mut d = page(200, 0);
        d.markdown = None;
        d.change_tracking = Some(ChangeTrackingResult {
            status: ChangeStatus::Same,
            first_observation: false,
            content_hash: "sha256:same".into(),
            snapshot: None,
            diff: None,
            judgment: None,
            tag: None,
            truncated: false,
        });
        assert!(!d.has_no_content(&[OutputFormat::ChangeTracking]));
        // Requested but never produced is still nothing delivered.
        d.change_tracking = None;
        assert!(d.has_no_content(&[OutputFormat::ChangeTracking]));
        // An empty extraction found none of the schema's fields.
        d.json = Some(serde_json::json!({}));
        assert!(d.has_no_content(&[OutputFormat::Json]));
        d.json = Some(serde_json::json!({"title": "x"}));
        assert!(!d.has_no_content(&[OutputFormat::Json]));
    }

    #[test]
    fn has_no_content_does_not_fail_a_request_that_asked_for_nothing() {
        // `"formats": []` survives serde (the default only fills in when the
        // field is ABSENT), and `.any()` over an empty slice is vacuously false —
        // without the guard, every such scrape would hard-fail as
        // `no_usable_content` no matter what was actually fetched.
        let d = page(200, 400);
        assert!(!d.has_no_content(&[]));
        let empty = page(200, 0);
        assert!(!empty.has_no_content(&[]));
    }

    #[test]
    fn http_error_is_silent_when_there_is_no_body_to_judge() {
        // `formats:["screenshot"]` / `["links"]` populate none of the four body
        // fields. Nothing to measure is not a measurement of nothing.
        let mut d = page(403, 0);
        d.markdown = None;
        d.plain_text = None;
        d.html = None;
        d.raw_html = None;
        assert!(d.http_error().is_none());
    }

    #[test]
    fn http_error_measures_markup_when_no_text_was_requested() {
        // `formats:["rawHtml"]` populates neither text field. Measuring text only
        // would read 0 and fail every >= 400 response, including the large real
        // pages that soft-block statuses are known to carry.
        let mut d = page(403, 0);
        d.markdown = None;
        d.raw_html = Some("<html>".repeat(2_000));
        assert!(d.http_error().is_none());
        d.raw_html = Some("<html>403</html>".into());
        assert!(d.http_error().is_some());
    }

    #[test]
    fn http_error_ignores_raw_html_length() {
        // The old gate took `.max()` across markdown/text/html/rawHtml, so asking
        // for `rawHtml` made the bar unreachable and the gate never fired.
        let mut d = page(404, 250);
        d.raw_html = Some("<html>".repeat(10_000));
        assert!(d.http_error().is_some());
    }

    #[test]
    fn clear_body_drops_content_keeps_metadata_and_block() {
        let mut data = ScrapeData {
            markdown: Some("Just a moment... Humans only".into()),
            html: Some("<html>challenge</html>".into()),
            raw_html: Some("<html>challenge</html>".into()),
            plain_text: Some("challenge text".into()),
            links: Some(vec!["https://help.example.com".into()]),
            images: Some(vec![ScrapedImage {
                url: "https://help.example.com/logo.png".into(),
                alt: Some("logo".into()),
            }]),
            json: Some(serde_json::json!({"junk": true})),
            summary: Some("a summary of junk".into()),
            llm_usage: None,
            chunks: None,
            warning: Some("blocked".into()),
            warnings: vec!["blocked".into()],
            render_decision: None,
            credit_cost: 0,
            metadata: PageMetadata {
                title: None,
                description: None,
                og_title: None,
                og_description: None,
                og_image: None,
                canonical_url: None,
                source_url: "https://www.glassdoor.com/Reviews/x.htm".into(),
                language: None,
                status_code: 200,
                rendered_with: None,
                elapsed_ms: 0,
                page_count: None,
                source_filename: None,
            },
            debug_extraction: None,
            content_type: Some("text/html".into()),
            change_tracking: None,
            block: Some(BlockOutcome {
                vendor: "cloudflare".into(),
                reason: "cloudflare challenge interstitial".into(),
            }),
            truncated: false,
        };
        data.clear_body();
        // content-shell + LLM outputs cleared
        assert!(data.markdown.is_none());
        assert!(data.html.is_none());
        assert!(data.raw_html.is_none());
        assert!(data.plain_text.is_none());
        assert!(data.links.is_none());
        assert!(data.images.is_none());
        assert!(data.json.is_none());
        assert!(data.summary.is_none());
        // block verdict, metadata, warnings kept for the caller
        assert!(data.block.is_some());
        assert_eq!(data.metadata.status_code, 200);
        assert_eq!(data.warnings, vec!["blocked".to_string()]);
    }

    #[test]
    fn resolve_render_js_request_wins_true() {
        assert_eq!(resolve_render_js(Some(true), Some(false)), Some(true));
    }

    #[test]
    fn resolve_render_js_request_wins_false() {
        assert_eq!(resolve_render_js(Some(false), Some(true)), Some(false));
    }

    #[test]
    fn resolve_render_js_falls_back_to_default() {
        assert_eq!(resolve_render_js(None, Some(true)), Some(true));
        assert_eq!(resolve_render_js(None, Some(false)), Some(false));
    }

    #[test]
    fn resolve_render_js_both_none() {
        assert_eq!(resolve_render_js(None, None), None);
    }

    #[test]
    fn crawl_request_accepts_render_js_camel_case() {
        let json = serde_json::json!({
            "url": "https://example.com",
            "renderJs": true,
            "waitFor": 2000
        });
        let req: CrawlRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.render_js, Some(true));
        assert_eq!(req.wait_for, Some(2000));
    }

    #[test]
    fn crawl_request_accepts_render_js_snake_case() {
        let json = serde_json::json!({
            "url": "https://example.com",
            "render_js": false,
            "wait_for": 1500
        });
        let req: CrawlRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.render_js, Some(false));
        assert_eq!(req.wait_for, Some(1500));
    }

    #[test]
    fn crawl_request_headers_round_trip_and_default_empty() {
        let req: CrawlRequest = serde_json::from_value(serde_json::json!({
            "url": "https://example.com",
            "headers": { "X-Custom": "1", "User-Agent": "test" }
        }))
        .unwrap();
        assert_eq!(req.headers.get("X-Custom"), Some(&"1".to_string()));
        assert_eq!(req.headers.get("User-Agent"), Some(&"test".to_string()));

        // Absent `headers` must stay an empty map, not a deserialize error, so
        // every crawl body written before this field existed still parses.
        let bare: CrawlRequest =
            serde_json::from_value(serde_json::json!({ "url": "https://example.com" })).unwrap();
        assert!(bare.headers.is_empty());
    }

    #[test]
    fn crawl_request_render_fields_optional() {
        let json = serde_json::json!({ "url": "https://example.com" });
        let req: CrawlRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.render_js, None);
        assert_eq!(req.wait_for, None);
    }

    #[test]
    fn requested_renderer_deserializes_lowercase() {
        for (s, expected) in [
            ("\"auto\"", RequestedRenderer::Auto),
            ("\"lightpanda\"", RequestedRenderer::Lightpanda),
            ("\"chrome\"", RequestedRenderer::Chrome),
            ("\"playwright\"", RequestedRenderer::Playwright),
        ] {
            let parsed: RequestedRenderer = serde_json::from_str(s).unwrap();
            assert_eq!(parsed, expected, "input {s} should parse to {expected:?}");
        }
    }

    #[test]
    fn requested_renderer_chrome_proxy_round_trip() {
        let parsed: RequestedRenderer = serde_json::from_str("\"chrome_proxy\"").unwrap();
        assert_eq!(parsed, RequestedRenderer::ChromeProxy);
        let json = serde_json::to_string(&RequestedRenderer::ChromeProxy).unwrap();
        assert_eq!(json, "\"chrome_proxy\"");
        assert_eq!(
            resolve_pinned_renderer(Some(RequestedRenderer::ChromeProxy)),
            Some("chrome_proxy")
        );
    }

    #[test]
    fn requested_renderer_rejects_unknown() {
        let result: Result<RequestedRenderer, _> = serde_json::from_str("\"firefox\"");
        assert!(
            result.is_err(),
            "unknown renderer should fail to deserialize"
        );
    }

    #[test]
    fn scrape_request_accepts_renderer_field() {
        let json = serde_json::json!({
            "url": "https://example.com",
            "renderer": "chrome"
        });
        let req: ScrapeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.renderer, Some(RequestedRenderer::Chrome));
    }

    #[test]
    fn scrape_request_renderer_explicit_null() {
        let json = serde_json::json!({
            "url": "https://example.com",
            "renderer": null
        });
        let req: ScrapeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.renderer, None);
    }

    #[test]
    fn scrape_request_renderer_omitted() {
        let json = serde_json::json!({ "url": "https://example.com" });
        let req: ScrapeRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.renderer, None);
    }

    #[test]
    fn crawl_request_accepts_renderer_field() {
        let json = serde_json::json!({
            "url": "https://example.com",
            "renderer": "lightpanda"
        });
        let req: CrawlRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.renderer, Some(RequestedRenderer::Lightpanda));
    }

    #[test]
    fn resolve_pinned_renderer_auto_returns_none() {
        assert_eq!(resolve_pinned_renderer(Some(RequestedRenderer::Auto)), None);
        assert_eq!(resolve_pinned_renderer(None), None);
    }

    #[test]
    fn resolve_pinned_renderer_chrome_returns_name() {
        assert_eq!(
            resolve_pinned_renderer(Some(RequestedRenderer::Chrome)),
            Some("chrome")
        );
        assert_eq!(
            resolve_pinned_renderer(Some(RequestedRenderer::Lightpanda)),
            Some("lightpanda")
        );
        assert_eq!(
            resolve_pinned_renderer(Some(RequestedRenderer::Playwright)),
            Some("playwright")
        );
    }

    #[test]
    fn chrome_proxy_serializes_with_underscore() {
        let json = serde_json::to_string(&RendererKind::ChromeProxy).unwrap();
        assert_eq!(json, "\"chrome_proxy\"");
    }

    #[test]
    fn chrome_proxy_deserializes_from_underscore() {
        let k: RendererKind = serde_json::from_str("\"chrome_proxy\"").unwrap();
        assert_eq!(k, RendererKind::ChromeProxy);
    }

    #[test]
    fn chrome_proxy_as_str() {
        assert_eq!(RendererKind::ChromeProxy.as_str(), "chrome_proxy");
    }

    #[test]
    fn block_message_keeps_anti_bot_wording_for_a_real_vendor() {
        let b = BlockOutcome {
            vendor: "cloudflare".into(),
            reason: "CF challenge".into(),
        };
        assert_eq!(
            b.message(),
            "Blocked by anti-bot (cloudflare): CF challenge"
        );
    }

    /// A thin document is not a vendor wall. Reporting it as one sent customers
    /// to buy proxies for what was a broken certificate — `wrong.host.badssl.com`
    /// renders 21 visible characters and was labelled "Blocked by anti-bot".
    #[test]
    fn block_message_does_not_call_a_thin_page_a_block() {
        let b = BlockOutcome {
            // The LITERAL, not the constant. Building the fixture from
            // `STRUCTURAL_FAILURE_VENDOR` would make this test pass even if the
            // constant were a typo, since both sides would then be wrong
            // together and the branch would silently never fire in production.
            // The constant is pinned to its real producer separately, by
            // `structural_failure_vendor_matches_classifier` in crw-extract.
            vendor: "structural_failure".into(),
            reason: "Structural: minimal_text on small page (500 bytes, 21 chars visible)".into(),
        };
        let m = b.message();
        assert!(
            !m.contains("Blocked by anti-bot"),
            "structural failure must not read as a vendor block: {m}"
        );
        assert!(m.starts_with("No usable content could be extracted"), "{m}");
        // The diagnostic detail still reaches the caller.
        assert!(m.contains("21 chars visible"), "{m}");
    }

    /// Every other vendor string must keep the historical wording verbatim: it is
    /// documented customer contract and is shared byte-for-byte with the
    /// Firecrawl-compat surface.
    #[test]
    fn block_message_split_is_scoped_to_structural_failure_only() {
        for vendor in [
            "cloudflare",
            "datadome",
            "perimeterx",
            "akamai",
            "imperva",
            "sucuri",
            "kasada",
            "vercel",
            "network_security",
            "rate_limited",
            "generic_block",
        ] {
            let b = BlockOutcome {
                vendor: vendor.into(),
                reason: "r".into(),
            };
            assert_eq!(b.message(), format!("Blocked by anti-bot ({vendor}): r"));
        }
    }
}

/// Status of an async crawl job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CrawlStatus {
    #[serde(rename = "scraping")]
    InProgress,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
}

/// GET /v1/crawl/:id response body.
/// Field names match Firecrawl API: status, total, completed, data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlState {
    #[serde(skip_serializing)]
    pub id: Uuid,
    pub success: bool,
    pub status: CrawlStatus,
    pub total: u32,
    pub completed: u32,
    /// How many of `completed` came back a block or an origin error page rather
    /// than the requested page. Counted here because a caller billing per page
    /// reads this envelope, not the paginated `data` array — and a walled page
    /// must not be charged. Additive: `#[serde(default)]` keeps an older client
    /// and an older engine interoperable in both directions.
    #[serde(default)]
    pub blocked: u32,
    pub data: Vec<ScrapeData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// POST /v1/crawl start response.
/// Matches Firecrawl format: { success: true, id: "..." }
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlStartResponse {
    pub success: bool,
    pub id: String,
}

/// POST /v1/map request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapRequest {
    pub url: String,
    pub max_depth: Option<u32>,
    #[serde(default = "default_true")]
    pub use_sitemap: bool,
    /// When true (default), fall back to a short-budget BFS crawl after the
    /// sitemap phase to fill gaps. Set to false for sitemap-only mode — faster
    /// for sites with rich sitemaps, but may miss pages a sitemap omits.
    #[serde(default = "default_true")]
    pub crawl_fallback: bool,
    /// Custom timeout in seconds (default: 120).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Tier B — strip tracking params. `Some(_)` overrides TOML.
    #[serde(default)]
    pub strip_tracking_params: Option<bool>,
    /// Tier A — drop action URLs. `Some(_)` overrides TOML.
    #[serde(default)]
    pub drop_action_urls: Option<bool>,
    /// Firecrawl-compatible coarse alias. `Some(true)`: strip every
    /// non-preserved param. `Some(false)`: switch the whole filter off
    /// (raw URLs — the explicit escape hatch).
    #[serde(default)]
    pub ignore_query_parameters: Option<bool>,
    /// Additive on top of `DEFAULT_TRACKING_PARAMS`. Keys are normalized to
    /// canonical form (lowercase, `-` folded to `_`), so `add-to-cart` and
    /// `add_to_cart` are equivalent. Max 64 keys; over-cap → 422.
    #[serde(default)]
    pub extra_tracking_params: Option<Vec<String>>,
    /// Additive on top of `DEFAULT_ACTION_PARAMS`. Keys are normalized to
    /// canonical form (lowercase, `-` folded to `_`). Max 64 keys; over-cap → 422.
    #[serde(default)]
    pub extra_action_params: Option<Vec<String>>,
    /// Additive on top of `ALWAYS_PRESERVE` + TOML preserves. Keys are
    /// normalized to canonical form (lowercase, `-` folded to `_`).
    /// Max 64 keys; over-cap → 422.
    #[serde(default)]
    pub preserve_params: Option<Vec<String>>,
    /// Max URLs to discover. Firecrawl-compatible. Defaults to
    /// `DEFAULT_MAX_DISCOVERED_URLS`; the engine clamps to its hard ceiling.
    /// Raise it to dump large/nested sitemaps (e.g. songsterr's ~4.3M URLs).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// POST /v1/map response data — the discovered links plus filter stats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapData {
    pub links: Vec<String>,
    /// Number of URLs the /map filter dropped entirely (Tier A action-URL
    /// matches). `0` when the filter is disabled.
    #[serde(default)]
    pub dropped_action_count: usize,
    /// Number of URLs that had at least one query param stripped by Tier B.
    /// `0` when the filter is disabled.
    #[serde(default)]
    pub stripped_tracking_count: usize,
    /// Sitemap documents discovered and parsed while mapping the site, e.g.
    /// `/sitemap.xml`, `/sitemap_index.xml`, `/product-sitemap.xml`. Kept out
    /// of `links` because a sitemap file is not a page. Empty when
    /// `useSitemap` is false or the site exposes no reachable sitemap.
    #[serde(default)]
    pub sitemaps: Vec<String>,
}

/// POST /v1/map response body.
/// Standard envelope: { success: true, data: { links: [...] } }
pub type MapResponse = ApiResponse<MapData>;

// ── Search types ──

/// Top-level "source" buckets exposed in the `/v1/search` API. Maps to
/// SearXNG's `categories` query parameter (web → general, news → news,
/// images → images).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchSource {
    Web,
    News,
    Images,
}

impl SearchSource {
    /// SearXNG category name for this source.
    pub fn searxng_category(self) -> &'static str {
        match self {
            SearchSource::Web => "general",
            SearchSource::News => "news",
            SearchSource::Images => "images",
        }
    }
}

/// A search engine selectable for the Camofox backend. Serialized lowercase so
/// the public API / MCP schema reads `"google"`, `"bing"`, … Unknown values are
/// rejected at deserialize. The SearXNG backend ignores this enum (it has its
/// own engine routing).
///
/// The set is deliberately small: these are the engines verified to return
/// clean, structured results through `camofox-browser` — general web
/// (Google via its search macro; Bing/DuckDuckGo by navigating their search URL
/// directly), GitHub (via its REST Search API), and the Wikipedia/YouTube
/// verticals (by search URL). How each is driven and scraped lives in
/// `crw-search`'s camofox client, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchEngine {
    #[default]
    Google,
    Bing,
    #[serde(rename = "duckduckgo")]
    DuckDuckGo,
    Github,
    Wikipedia,
    Youtube,
    Reddit,
    Amazon,
}

impl SearchEngine {
    /// Short label used to tag results with their originating engine,
    /// e.g. `"google"`, `"duckduckgo"`.
    pub fn label(self) -> &'static str {
        match self {
            SearchEngine::Google => "google",
            SearchEngine::Bing => "bing",
            SearchEngine::DuckDuckGo => "duckduckgo",
            SearchEngine::Github => "github",
            SearchEngine::Wikipedia => "wikipedia",
            SearchEngine::Youtube => "youtube",
            SearchEngine::Reddit => "reddit",
            SearchEngine::Amazon => "amazon",
        }
    }
}

/// User-facing category modifiers.
///
/// Three values carry curated, Firecrawl-compatible behavior:
/// - `Github` / `Research` switch to topical SearXNG *engines* (configurable
///   via `[search].github_engines` / `[search].research_engines`).
/// - `Pdf` appends `filetype:pdf` to the query.
///
/// Any other string is passed straight through to SearXNG's native
/// `categories` query parameter (e.g. `science`, `it`, `news`, `files`,
/// `images`), so SearXNG's own engine→category routing applies without any
/// crw code or config changes. This makes the surface a strict superset of
/// Firecrawl's `github`/`research`/`pdf` — existing callers are unaffected.
///
/// See <https://docs.searxng.org/user/configured_engines.html> for the
/// categories a given SearXNG instance exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchCategory {
    Github,
    Research,
    Pdf,
    /// Unknown value — forwarded verbatim to SearXNG's `categories` param.
    Other(String),
}

impl SearchCategory {
    /// Wire/string representation. The three curated variants round-trip to
    /// their lowercase names; `Other` returns the verbatim passthrough value.
    pub fn as_str(&self) -> &str {
        match self {
            SearchCategory::Github => "github",
            SearchCategory::Research => "research",
            SearchCategory::Pdf => "pdf",
            SearchCategory::Other(s) => s.as_str(),
        }
    }
}

impl From<String> for SearchCategory {
    fn from(s: String) -> Self {
        match s.as_str() {
            "github" => SearchCategory::Github,
            "research" => SearchCategory::Research,
            "pdf" => SearchCategory::Pdf,
            _ => SearchCategory::Other(s),
        }
    }
}

impl Serialize for SearchCategory {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SearchCategory {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(SearchCategory::from(String::deserialize(deserializer)?))
    }
}

/// Time-window filter, mirrors Google's `tbs=qdr:*` syntax used by Firecrawl.
/// SearXNG's `time_range` only supports day/week/month/year; `Hour` is mapped
/// to `Day` for parity with the SaaS implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchTimeFilter {
    #[serde(rename = "qdr:h")]
    Hour,
    #[serde(rename = "qdr:d")]
    Day,
    #[serde(rename = "qdr:w")]
    Week,
    #[serde(rename = "qdr:m")]
    Month,
    #[serde(rename = "qdr:y")]
    Year,
}

impl SearchTimeFilter {
    /// SearXNG `time_range` string. SearXNG has no hour granularity, so
    /// `Hour` is reported as `day` (lossy; matches SaaS behavior).
    pub fn searxng_time_range(self) -> &'static str {
        match self {
            SearchTimeFilter::Hour | SearchTimeFilter::Day => "day",
            SearchTimeFilter::Week => "week",
            SearchTimeFilter::Month => "month",
            SearchTimeFilter::Year => "year",
        }
    }
}

/// `scrapeOptions` sub-object — a narrow projection of `ScrapeRequest` that
/// we accept on every result from a search. Only the fields the SaaS exposes.
///
/// `formats` defaults to `["markdown"]` so Firecrawl-compatible callers that
/// pass `scrapeOptions: {}` (toggle enrichment without specifying formats)
/// get a sensible default instead of a deserialization error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchScrapeOptions {
    #[serde(default = "default_formats")]
    pub formats: Vec<OutputFormat>,
    #[serde(default = "default_true")]
    pub only_main_content: bool,
    /// Residential-proxy exit country (ISO 3166-1 alpha-2) for the per-result page scrape.
    /// Populated by the SaaS layer from the caller's IP (geo-aware proxy). `None` = engine default.
    #[serde(default)]
    pub country: Option<String>,
    /// Per-result scrape budget (ms). `None` = the search-enrichment default,
    /// NOT the implicit full-ladder deadline a single `/v1/scrape` gets: search
    /// waits for every result, so one straggler walking the whole renderer
    /// ladder would stall the entire response. Must be in `(0, 60000]`.
    #[serde(default)]
    pub timeout: Option<u64>,
}

/// Deserialize an optional `Vec<T>` that may arrive either as a real JSON array
/// or as a *string* encoding one. Some MCP/LLM clients JSON-encode array
/// arguments, sending the string `"[\"bing\"]"` (or a bare `"google,bing"`)
/// instead of the array `["bing"]`. A real array deserializes normally; a
/// string is parsed as JSON when it looks like an array, otherwise split on
/// commas, with each token deserialized as `T`. Empty/`null` ⇒ `None`.
fn string_or_seq_opt<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    use serde::de::{self, SeqAccess, Visitor};
    use std::fmt;
    use std::marker::PhantomData;

    struct V<T>(PhantomData<T>);

    impl<'de, T: serde::de::DeserializeOwned> Visitor<'de> for V<T> {
        type Value = Option<Vec<T>>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array, or a string encoding one (JSON or comma-separated)")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element::<T>()? {
                out.push(v);
            }
            Ok(Some(out))
        }

        fn visit_str<E: de::Error>(self, s: &str) -> Result<Self::Value, E> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            let parsed: Vec<T> = if s.starts_with('[') {
                serde_json::from_str(s).map_err(de::Error::custom)?
            } else {
                s.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| serde_json::from_value(serde_json::Value::String(t.to_string())))
                    .collect::<Result<Vec<T>, _>>()
                    .map_err(de::Error::custom)?
            };
            Ok(Some(parsed))
        }
    }

    deserializer.deserialize_any(V(PhantomData))
}

/// POST /v1/search request body. Mirrors the zod schema in
/// `crw-saas/src/lib/search-schema.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    /// Number of results per source (or total when `sources` is unset).
    /// Defaults to `[search].default_limit` when omitted; clamped to
    /// `[search].max_limit` server-side.
    #[serde(default)]
    pub limit: Option<u32>,
    /// SearXNG `language` parameter (e.g. `"en"`, `"de"`, `"auto"`).
    #[serde(default)]
    pub lang: Option<String>,
    /// Google-style time filter (`qdr:h|d|w|m|y`).
    #[serde(default)]
    pub tbs: Option<SearchTimeFilter>,
    /// When set, results are grouped under `web`/`news`/`images` keys.
    /// When unset, a flat array is returned.
    #[serde(default, deserialize_with = "string_or_seq_opt")]
    pub sources: Option<Vec<SearchSource>>,
    /// User-facing category modifiers. Max 5 entries (matches SaaS).
    #[serde(default, deserialize_with = "string_or_seq_opt")]
    pub categories: Option<Vec<SearchCategory>>,
    /// Engines to search via the Camofox backend. `None`/empty ⇒ `[google]`.
    /// Multiple engines are fanned out and merged. Capped server-side. Ignored
    /// by the opt-in SearXNG backend.
    #[serde(default, deserialize_with = "string_or_seq_opt")]
    pub engines: Option<Vec<SearchEngine>>,
    /// When set, every `web` result is enriched in-process via the scrape
    /// pipeline (parallel, bounded by `[crawler].max_concurrency`).
    #[serde(default)]
    pub scrape_options: Option<SearchScrapeOptions>,
    /// When true, every scraped result also gets an LLM summary attached to
    /// `SearchResult.summary`. Requires `scrape_options` to be set (so the
    /// markdown exists to summarize). LLM fan-out is bounded by
    /// `[extraction.llm].max_concurrency`.
    #[serde(default, alias = "summarize_results")]
    pub summarize_results: Option<bool>,
    /// When true, a single synthesized answer is generated from the top-N
    /// scraped results. Requires `scrape_options` to be set.
    #[serde(default)]
    pub answer: Option<bool>,
    /// Number of top results to include in answer synthesis (default 5,
    /// capped at 10).
    #[serde(default, alias = "answer_top_n")]
    pub answer_top_n: Option<u32>,
    /// Per-source character cap for the answer prompt (default 8192,
    /// hard-capped at 32768 server-side).
    #[serde(default, alias = "max_chars_per_source")]
    pub max_chars_per_source: Option<usize>,
    /// BYOK fields (mirror `ScrapeRequest`).
    #[serde(default, alias = "llm_api_key")]
    pub llm_api_key: Option<String>,
    #[serde(default, alias = "llm_provider")]
    pub llm_provider: Option<String>,
    #[serde(default, alias = "llm_model")]
    pub llm_model: Option<String>,
    #[serde(default, alias = "base_url")]
    pub base_url: Option<String>,
    /// Optional user-supplied instructions appended to the per-result
    /// summary system prompt. See `ScrapeRequest.summary_prompt`. Capped
    /// at 500 chars server-side.
    #[serde(default, alias = "summary_prompt")]
    pub summary_prompt: Option<String>,
    /// Optional user-supplied instructions appended to the answer-synthesis
    /// system prompt (e.g. "respond in Turkish", "be concise"). The
    /// "answer using ONLY the provided sources" rule and citation discipline
    /// stay intact. Capped at 500 chars server-side.
    #[serde(default, alias = "answer_prompt")]
    pub answer_prompt: Option<String>,
    /// Sampling temperature for the answer-synthesis LLM call. Omitted (None)
    /// keeps the provider default (prod behavior). The benchmark/eval harness
    /// sets `0` (with a fixed seed) to make answers deterministic, so a real
    /// accuracy lever is distinguishable from sampling noise.
    #[serde(default, alias = "answer_temperature")]
    pub answer_temperature: Option<f32>,
    /// Per-request override for `[search].query_expand_variants` — the number
    /// of diverse query rewrites fetched + unioned when query expansion is on.
    /// None uses the server config. The benchmark/eval harness sets this to A/B
    /// recall (e.g. 1 vs 3) at a fixed answer temperature.
    #[serde(default, alias = "query_expand_variants")]
    pub query_expand_variants: Option<usize>,
    /// Per-request override for `[search].multi_round` — the adaptive
    /// evidence-scout round that fires when the round-1 answer abstains. None
    /// uses the server config. The eval harness sets this to A/B the lever.
    #[serde(default, alias = "multi_round")]
    pub multi_round: Option<bool>,
    /// Per-request override for `[search].answer_list_format` — when the query
    /// has list intent ("best/top X in Y", "recommend …"), render the answer as
    /// a ranked list of named options instead of prose. None uses the server
    /// config; Some(false) forces prose, Some(true) forces the list path (still
    /// only fires on list-intent queries).
    #[serde(default, alias = "answer_list_format")]
    pub answer_list_format: Option<bool>,
    /// Maximum number of bytes of each per-result markdown sent to the LLM
    /// when `summarize_results` is enabled. Defaults to
    /// `[extraction.llm].max_html_bytes` (100 KB). Clamped to a 200 KB
    /// server-side ceiling. Independent from `max_chars_per_source`, which
    /// caps the answer-synthesis path, not the per-result summary path.
    #[serde(default, alias = "max_content_chars")]
    pub max_content_chars: Option<usize>,
}

/// A single search result (web or news). Mirrors `SearchResult` in
/// `crw-saas/src/lib/search-transform.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub description: String,
    /// Alias of `description`. Always populated. Emitted so downstream LLM
    /// pipelines that ask for "snippet" (Firecrawl convention) don't need a
    /// rename step. `#[serde(default)]` keeps deserialization permissive for
    /// callers that don't supply it.
    #[serde(default)]
    pub snippet: String,
    pub position: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    // Populated when scrapeOptions is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<PageMetadata>,
    /// LLM-generated summary; populated when `summarizeResults: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Set when this result's enrichment scrape failed (P3-4), so a partial
    /// success is observable: distinguishes "scrape failed" from "page simply
    /// had no markdown". Absent on success — backward-compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set when the enrichment scrape returned a partial-DOM snapshot because
    /// its budget elapsed (`ScrapeData.truncated`). Sibling of `error`: `error`
    /// marks a total failure, this marks an incomplete success — without it a
    /// budget-shortened render is indistinguishable from a thin page. Absent
    /// (not `false`) when the scrape completed — backward-compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

/// A single image result. Mirrors `ImageResult` in
/// `crw-saas/src/lib/search-transform.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageResult {
    pub url: String,
    pub title: String,
    pub description: String,
    pub image_url: String,
    pub position: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
}

/// Grouped result envelope when `sources` is set on the request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedSearchData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web: Option<Vec<SearchResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub news: Option<Vec<SearchResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageResult>>,
}

/// `data` payload of the `/v1/search` response. Either a flat list of
/// results or a grouped object — chosen by whether the request specified
/// `sources`. Untagged: serializes as either an array or an object with
/// `web`/`news`/`images` keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchData {
    Flat(Vec<SearchResult>),
    Grouped(GroupedSearchData),
}

/// A citation reference in an LLM-synthesized search answer. `position`
/// is clamped to `[0, sources.len())` server-side so fabricated indices
/// can't escape the input source list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Citation {
    pub url: String,
    pub title: String,
    pub position: u32,
}

/// Wrapper data envelope for `/v1/search` responses. Carries the existing
/// `SearchData` (flat or grouped) alongside optional LLM-generated
/// `answer` + `citations`. Adding sibling fields directly to `SearchData`
/// is impossible because that enum is `#[serde(untagged)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponseData {
    pub results: SearchData,
    /// LLM-synthesized answer over the top-N results; `None` unless
    /// `answer: true` was set on the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
    /// Source citations for the answer. Order is meaningful: citation #0
    /// is `sources[0]`. Capped at 20 entries.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub citations: Vec<Citation>,
    /// Token usage + best-effort cost from the answer synthesis call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_usage: Option<LlmUsage>,
    /// Soft-failure / partial-result notices (e.g. "answer call rate-limited;
    /// summaries returned without answer").
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
}

/// POST /v1/search response body.
pub type SearchResponse = ApiResponse<SearchResponseData>;

// ── Render result ──

/// Closed enum of renderer kinds used in routing decisions and metrics.
/// Distinct from `RequestedRenderer` (user-facing input) — this is the
/// internal vocabulary for what actually executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RendererKind {
    Http,
    Lightpanda,
    Chrome,
    #[serde(rename = "chrome_proxy")]
    ChromeProxy,
    Camofox,
    Byparr,
}

impl RendererKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RendererKind::Http => "http",
            RendererKind::Lightpanda => "lightpanda",
            RendererKind::Chrome => "chrome",
            RendererKind::ChromeProxy => "chrome_proxy",
            RendererKind::Camofox => "camofox",
            RendererKind::Byparr => "byparr",
        }
    }
}

/// Why and how a renderer was chosen for a given request. Surfaced in
/// `FetchResult.render_decision` and exposed to API callers behind a debug gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum RenderDecision {
    /// User pinned a specific renderer; auto-mode learning is bypassed.
    UserPinned { renderer: RendererKind },
    /// Auto mode used the configured default chain (no host preference yet).
    AutoDefault { chosen: RendererKind },
    /// Auto mode promoted a heavy renderer based on host preference.
    AutoPromoted {
        chosen: RendererKind,
        from: RendererKind,
        reason: String,
    },
    /// Auto mode skipped a renderer because its circuit breaker was open.
    BreakerSkipped {
        skipped: RendererKind,
        chosen: RendererKind,
    },
    /// Failover triggered after the initial choice failed.
    Failover {
        chain: Vec<RendererKind>,
        reason: FailoverErrorKind,
    },
}

/// Closed taxonomy of failure reasons that drive failover and host learning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailoverErrorKind {
    /// LightPanda hydration / runtime exception (counts toward promotion).
    NextJsClientError,
    /// LightPanda returned an empty Next.js root shell.
    EmptyNextRoot,
    /// LightPanda timed out.
    LightpandaTimeout,
    /// LightPanda crashed or connection died.
    LightpandaCrash,
    /// Cloudflare challenge detected (combination markers).
    CloudflareChallenge,
    /// Generic placeholder / too-short content.
    PlaceholderContent,
    /// Vendor-specific anti-bot block (Akamai, PerimeterX, DataDome, etc.).
    /// Vendor name is recorded via `crw_vendor_block_total{vendor}` metric
    /// and the renderer warning — not carried in the enum variant to keep
    /// the type `Copy`-friendly.
    VendorBlock,
    /// JS renderer returned a 4xx/5xx HTTP status (e.g. 403, 429) — same
    /// status set the HTTP tier escalates on. Caught in the JS tier so a
    /// "200 with bot HTML" or "403 with content" can't masquerade as success.
    StatusBlocked,
    /// The `crw_extract::antibot` classifier flagged a block the lighter
    /// `detector` heuristics missed (e.g. a "blocked by network security"
    /// WAF page served with HTTP 200). Drives escalation toward the
    /// residential `chrome_proxy` tier; counts toward host promotion.
    AntibotBlock,
    /// Network error during render.
    NetworkError,
    /// Other / unclassified failure (does NOT count for promotion).
    Other,
}

impl FailoverErrorKind {
    /// Strict failure predicate: only LightPanda-specific failures should
    /// drive host preference promotion. CF challenges and network errors
    /// are not LightPanda's fault.
    pub fn counts_for_promotion(&self) -> bool {
        matches!(
            self,
            FailoverErrorKind::NextJsClientError
                | FailoverErrorKind::EmptyNextRoot
                | FailoverErrorKind::LightpandaTimeout
                | FailoverErrorKind::LightpandaCrash
                | FailoverErrorKind::PlaceholderContent
                | FailoverErrorKind::AntibotBlock
        )
    }

    /// Stable camelCase identifier matching the JSON `serde` rendering.
    /// Used in user-facing warnings so the string a client sees in a
    /// `warnings[]` entry matches the `renderDecision.reason` field.
    pub fn as_str(&self) -> &'static str {
        match self {
            FailoverErrorKind::NextJsClientError => "nextJsClientError",
            FailoverErrorKind::EmptyNextRoot => "emptyNextRoot",
            FailoverErrorKind::LightpandaTimeout => "lightpandaTimeout",
            FailoverErrorKind::LightpandaCrash => "lightpandaCrash",
            FailoverErrorKind::CloudflareChallenge => "cloudflareChallenge",
            FailoverErrorKind::PlaceholderContent => "placeholderContent",
            FailoverErrorKind::VendorBlock => "vendorBlock",
            FailoverErrorKind::StatusBlocked => "statusBlocked",
            FailoverErrorKind::AntibotBlock => "antibotBlock",
            FailoverErrorKind::NetworkError => "networkError",
            FailoverErrorKind::Other => "other",
        }
    }
}

/// Result of fetching + optionally rendering a page.
#[derive(Debug, Clone)]
pub struct FetchResult {
    pub url: String,
    /// Final URL after redirects, populated only when it differs from the
    /// requested `url`. None means no redirect or scheme/path was identical.
    pub final_url: Option<String>,
    pub status_code: u16,
    pub html: String,
    pub content_type: Option<String>,
    pub raw_bytes: Option<Vec<u8>>,
    pub rendered_with: Option<String>,
    pub elapsed_ms: u64,
    pub warning: Option<String>,
    /// Routing decision metadata. `None` for legacy / non-instrumented paths.
    pub render_decision: Option<RenderDecision>,
    /// Credit cost for this request (set by routing layer; 0 = not yet priced).
    pub credit_cost: u32,
    /// Soft-failure / informational warnings to surface to the caller.
    pub warnings: Vec<String>,
    /// The anti-bot wall a JS tier recognized on THIS body, set only when the
    /// ladder rejected the body for it and returned it anyway because no tier
    /// did better. The page-level classifier cannot always re-derive it: a
    /// vendor wall with enough prose passes its markdown guard.
    pub wall: Option<BlockOutcome>,
    /// Set by chrome renderer when the navigation budget elapsed before
    /// `loadEventFired` and we snapshotted the partial DOM. Mid-load HTML may
    /// still extract usefully (`single.rs` decides success on md length).
    pub truncated: bool,
    /// Set when `Deadline::remaining() == 0` was observed at result-build time.
    /// Stricter than `truncated` — caller's whole budget is spent.
    pub deadline_exceeded: bool,
    /// XHR/fetch responses captured during navigation. Empty unless the
    /// renderer ran with network capture enabled. Used by extraction as a
    /// fallback content source when DOM-based extraction is low quality.
    pub captured_responses: Vec<CapturedNetworkResponse>,
}

/// A single XHR/fetch response captured via CDP Network domain.
#[derive(Debug, Clone)]
pub struct CapturedNetworkResponse {
    pub url: String,
    pub request_id: String,
    pub status: u16,
    pub mime_type: Option<String>,
    pub body: Option<String>,
    pub body_size_bytes: usize,
}

// ===========================================================================
// Change tracking (monitor) types
//
// These types are the stateless primitives the SaaS / self-host monitor
// control plane builds on. `crw-diff` consumes `ChangeTrackingOptions` and
// produces a `ChangeTrackingResult`; the LLM judge (`crw-extract`) populates
// `ChangeJudgment`. Wire shapes mirror Firecrawl's `/monitor` check payloads.
// ===========================================================================

/// Change-tracking diff mode. Wire: `"gitDiff"` or `"json"`.
///
/// Deserialization also accepts `"git-diff"` for ergonomics; serialization
/// always emits the canonical `"gitDiff"` / `"json"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeTrackingMode {
    GitDiff,
    Json,
}

impl<'de> Deserialize<'de> for ChangeTrackingMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "gitDiff" | "git-diff" => Ok(ChangeTrackingMode::GitDiff),
            "json" => Ok(ChangeTrackingMode::Json),
            other => Err(serde::de::Error::custom(format!(
                "Unknown changeTracking mode '{other}'. Valid modes: gitDiff, json (alias: git-diff)."
            ))),
        }
    }
}

/// A snapshot of a scrape, used as the baseline to diff against. The caller
/// (SaaS / self-host monitor) persists this between checks and supplies the
/// prior one as `previous`; opencore is stateless and stores nothing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeTrackingSnapshot {
    /// Normalized markdown content (present for gitDiff / mixed mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<String>,
    /// Extracted structured JSON (present for json / mixed mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
    /// Mode-aware content hash (markdown hash for gitDiff/mixed; tracked-field
    /// hash for json mode). The SaaS short-circuit keys off this.
    #[serde(default)]
    pub content_hash: String,
    /// Caller-stamped capture time; echoed back untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
}

/// Change-tracking options. Sibling field on `ScrapeRequest` (activated by the
/// `"changeTracking"` format string) and the body of `POST /v1/change-tracking/diff`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeTrackingOptions {
    /// Diff surfaces to compute. `["gitDiff"]` = markdown unified diff + AST;
    /// `["json"]` = per-field diff; `["json","gitDiff"]` = mixed (both).
    #[serde(default)]
    pub modes: Vec<ChangeTrackingMode>,
    /// JSON schema describing the fields to track (json / mixed mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
    /// Natural-language extraction prompt (alternative to `schema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The previous snapshot to diff against. `None` => first observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<ChangeTrackingSnapshot>,
    /// Opaque caller tag echoed back on the result (e.g. a target id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// MIME content type of the current page (binary/non-text → byte hash, no diff).
    #[serde(
        default,
        alias = "content_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_type: Option<String>,
}

/// Per-page change status emitted by opencore. Set-level `new` / `removed`
/// are computed by the caller's reconciler, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeStatus {
    Same,
    Changed,
}

/// Judge confidence level. Matches Firecrawl's `"low" | "medium" | "high"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeConfidence {
    Low,
    Medium,
    High,
}

/// A single meaningful change called out by the judge. Mirrors Firecrawl's
/// `meaningfulChanges[]` entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeaningfulChange {
    /// `"added" | "removed" | "changed"`.
    #[serde(rename = "type")]
    pub change_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    pub reason: String,
}

/// LLM meaningful-change judgment. Public wire shape is exactly
/// `{meaningful, confidence, reason, meaningfulChanges}` (Firecrawl parity);
/// `llm_usage` is internal-only and never serialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeJudgment {
    pub meaningful: bool,
    pub confidence: ChangeConfidence,
    pub reason: String,
    #[serde(default)]
    pub meaningful_changes: Vec<MeaningfulChange>,
    /// Token usage for the judge call. Internal-only — `skip` keeps it out of
    /// the public judgment wire shape; the orchestration layer reads it for
    /// billing/observability.
    #[serde(skip)]
    pub llm_usage: Option<LlmUsage>,
}

/// One change line within a diff chunk (parse-diff-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffChange {
    /// `"add" | "del" | "normal"`.
    #[serde(rename = "type")]
    pub change_type: String,
    pub content: String,
    /// New-file line number (add / normal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ln: Option<usize>,
    /// Old-file line number (normal only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ln1: Option<usize>,
    /// New-file line number (normal only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ln2: Option<usize>,
}

/// A hunk within a diff file (parse-diff-compatible).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffChunk {
    /// The `@@ -a,b +c,d @@` header line.
    pub content: String,
    pub changes: Vec<DiffChange>,
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
}

/// A single file's diff (parse-diff-compatible). For a single-page change
/// track there is always exactly one synthetic file (`previous` → `current`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub from: String,
    pub to: String,
    pub additions: usize,
    pub deletions: usize,
    pub chunks: Vec<DiffChunk>,
}

/// The git-diff AST (parse-diff style). Serialized into `diff.json` for
/// gitDiff-only mode; in mixed mode the per-field json diff takes `diff.json`
/// instead and this AST is not surfaced.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DiffAst {
    pub files: Vec<DiffFile>,
    pub additions: usize,
    pub deletions: usize,
    /// True when the AST was capped at `max_diff_changes` (full snapshot still
    /// retained, so the change is recoverable).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// The `diff` envelope: `{ text?, json? }`. `text` is the unified markdown
/// diff (gitDiff / mixed). `json` is mode-polymorphic — the parse-diff AST in
/// gitDiff-only mode, or the per-field path map (`{ "<path>": {previous,current} }`)
/// in json / mixed mode. Modeled as `Value` to carry either shape, exactly
/// matching Firecrawl's wire payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeDiff {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json: Option<serde_json::Value>,
}

/// Result of a change-tracking computation for one page. Surfaced on
/// `ScrapeData.change_tracking` and returned by `POST /v1/change-tracking/diff`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeTrackingResult {
    pub status: ChangeStatus,
    /// True when no `previous` was supplied — the caller maps this to `new`.
    #[serde(default)]
    pub first_observation: bool,
    /// Mode-aware hash of the current content (see `ChangeTrackingSnapshot`).
    pub content_hash: String,
    /// The current snapshot — persist this as the next check's `previous`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<ChangeTrackingSnapshot>,
    /// The diff surfaces; `None` when `status == Same` or for binary content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ChangeDiff>,
    /// Meaningful-change judgment; populated by the orchestration layer only
    /// when the page changed, a goal is set, and judging is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgment: Option<ChangeJudgment>,
    /// Echoed caller tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// True when the diff AST was truncated (mirrors `DiffAst.truncated`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[cfg(test)]
mod search_engine_tests {
    use super::*;

    #[test]
    fn search_engine_serde_is_lowercase() {
        let e: SearchEngine = serde_json::from_str("\"duckduckgo\"").unwrap();
        assert_eq!(e, SearchEngine::DuckDuckGo);
        assert_eq!(
            serde_json::to_string(&SearchEngine::Bing).unwrap(),
            "\"bing\""
        );
    }

    #[test]
    fn search_engine_label() {
        assert_eq!(SearchEngine::Google.label(), "google");
        assert_eq!(SearchEngine::DuckDuckGo.label(), "duckduckgo");
        assert_eq!(SearchEngine::Github.label(), "github");
    }

    #[test]
    fn search_engine_rejects_unsupported() {
        // Engines that don't return clean results (CAPTCHA / anti-bot / login)
        // aren't in the enum and must fail to deserialize rather than pass.
        assert!(serde_json::from_str::<SearchEngine>("\"stackoverflow\"").is_err());
        assert!(serde_json::from_str::<SearchEngine>("\"yelp\"").is_err());
    }

    #[test]
    fn search_engine_reddit_and_amazon_parse() {
        assert_eq!(
            serde_json::from_str::<SearchEngine>("\"reddit\"").unwrap(),
            SearchEngine::Reddit
        );
        assert_eq!(
            serde_json::from_str::<SearchEngine>("\"amazon\"").unwrap(),
            SearchEngine::Amazon
        );
    }

    #[test]
    fn search_engine_default_is_google() {
        assert_eq!(SearchEngine::default(), SearchEngine::Google);
    }

    #[test]
    fn search_request_engines_defaults_to_none_and_omitted() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":"rust"}"#).unwrap();
        assert!(r.engines.is_none());
    }

    #[test]
    fn search_request_parses_engines_list() {
        let r: SearchRequest =
            serde_json::from_str(r#"{"query":"rust","engines":["google","bing"]}"#).unwrap();
        assert_eq!(
            r.engines.unwrap(),
            vec![SearchEngine::Google, SearchEngine::Bing]
        );
    }

    #[test]
    fn search_request_accepts_stringified_engines() {
        // Some MCP/LLM clients JSON-encode array args, sending the *string*
        // `"[\"bing\"]"` instead of the array `["bing"]`.
        let r: SearchRequest =
            serde_json::from_str(r#"{"query":"rust","engines":"[\"google\",\"bing\"]"}"#).unwrap();
        assert_eq!(
            r.engines.unwrap(),
            vec![SearchEngine::Google, SearchEngine::Bing]
        );
    }

    #[test]
    fn search_request_accepts_comma_separated_engines_string() {
        let r: SearchRequest =
            serde_json::from_str(r#"{"query":"rust","engines":"google, bing"}"#).unwrap();
        assert_eq!(
            r.engines.unwrap(),
            vec![SearchEngine::Google, SearchEngine::Bing]
        );
    }

    #[test]
    fn search_request_accepts_stringified_sources_and_categories() {
        let r: SearchRequest = serde_json::from_str(
            r#"{"query":"rust","sources":"[\"web\",\"news\"]","categories":"github,pdf"}"#,
        )
        .unwrap();
        assert_eq!(
            r.sources.unwrap(),
            vec![SearchSource::Web, SearchSource::News]
        );
        assert_eq!(
            r.categories.unwrap(),
            vec![SearchCategory::Github, SearchCategory::Pdf]
        );
    }

    #[test]
    fn search_request_empty_engines_string_is_none() {
        let r: SearchRequest = serde_json::from_str(r#"{"query":"rust","engines":""}"#).unwrap();
        assert!(r.engines.is_none());
    }
}
