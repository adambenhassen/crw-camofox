//! `CamofoxBrowse` — an rmcp server that bridges MCP clients to the
//! camofox-browser REST API, giving agents interactive control of a Camoufox
//! (Firefox) browser: navigate → snapshot → click/type → evaluate/screenshot.
//!
//! Each `#[tool]` is a thin forward to [`crate::camofox::CamofoxClient`]; the
//! camofox server owns the page, refs, and stealth — this layer only translates
//! MCP tool calls into REST calls.

use std::sync::Arc;

use base64::Engine;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::camofox::{CamofoxClient, Target};

#[derive(Clone)]
pub struct CamofoxBrowse {
    client: Arc<CamofoxClient>,
    default_wait_ms: u64,
    #[allow(dead_code)] // read by the #[tool_handler] generated glue
    tool_router: ToolRouter<Self>,
}

fn ok_json(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(value.to_string())])
}

fn err(msg: impl Into<String>) -> CallToolResult {
    let body = serde_json::json!({ "ok": false, "error": msg.into() });
    let mut result = CallToolResult::success(vec![Content::text(body.to_string())]);
    result.is_error = Some(true);
    result
}

/// Resolve a `ref`/`selector` pair into a [`Target`], rejecting "neither" and
/// "both" so the agent gets a clear error instead of ambiguous behaviour.
fn resolve_target<'a>(
    ref_: &'a Option<String>,
    selector: &'a Option<String>,
) -> Result<Target<'a>, CallToolResult> {
    match (ref_.as_deref(), selector.as_deref()) {
        (Some(r), None) => Ok(Target::Ref(r)),
        (None, Some(s)) => Ok(Target::Selector(s)),
        (Some(_), Some(_)) => Err(err("pass exactly one of `ref` or `selector`, not both")),
        (None, None) => Err(err("one of `ref` or `selector` is required")),
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NavigateInput {
    /// Absolute URL to navigate to (http/https).
    pub url: String,
    /// Readiness wait budget in ms (default 15000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EmptyInput {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TargetInput {
    /// `eN` ref from a prior `snapshot`.
    #[serde(default, rename = "ref")]
    pub ref_: Option<String>,
    /// CSS selector (alternative to `ref`).
    #[serde(default)]
    pub selector: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TypeInput {
    #[serde(default, rename = "ref")]
    pub ref_: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    /// Text to type into the targeted element.
    pub text: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EvaluateInput {
    /// JavaScript expression to evaluate in the page.
    pub expression: String,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ScreenshotInput {
    /// Capture the full scrollable page instead of just the viewport.
    #[serde(default)]
    pub full_page: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PressInput {
    /// Keyboard key to press, e.g. `Enter`, `Tab`, `Escape`, `ArrowDown`.
    pub key: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ScrollInput {
    /// Scroll direction: `up`, `down`, `left`, or `right`.
    pub direction: String,
    /// Pixels to scroll (default 500).
    #[serde(default)]
    pub pixels: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct WaitInput {
    /// Wait budget in milliseconds (default 5000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ConsoleInput {
    /// Also include uncaught JavaScript errors alongside console messages.
    #[serde(default)]
    pub include_errors: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CookiesInput {
    /// `get` returns the current cookies; `set` imports `cookies`.
    pub action: String,
    /// Cookies to import (Playwright format) — required for `action: set`.
    #[serde(default)]
    pub cookies: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct LinksInput {
    /// Max links to return (default 50).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Pagination offset (default 0).
    #[serde(default)]
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct EvaluateExtendedInput {
    /// Long-running JavaScript expression to evaluate.
    pub expression: String,
    /// Timeout in ms (default 60000, server cap 300000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ExtractStructuredInput {
    /// Camofox extract schema, e.g.
    /// `{"kind":"object","fields":{"title":{"kind":"text","selector":"h1"}}}`.
    /// Field `kind` ∈ text|html|attr|url|number.
    pub schema: serde_json::Value,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ActInput {
    /// Action kind: `click`, `type`, `press`, `scroll`, `hover`, `wait`,
    /// `extractStructured`, `close`, ...
    pub kind: String,
    /// The action's remaining fields (e.g. `{ "ref": "e5" }` or
    /// `{ "text": "hello" }`), merged into the request.
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DisplayInput {
    /// `headless` (default), `headed`, or `virtual` (Xvfb + noVNC viewer).
    pub mode: String,
}

#[tool_router]
impl CamofoxBrowse {
    pub fn new(client: Arc<CamofoxClient>, default_wait_ms: u64) -> Self {
        Self {
            client,
            default_wait_ms,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Navigate the browser to a URL (http/https) and wait for it to load. \
                       Creates the session's tab on first call, reuses it after. Returns the \
                       resolved URL. Call `snapshot` next to see the page."
    )]
    pub async fn navigate(
        &self,
        Parameters(input): Parameters<NavigateInput>,
    ) -> Result<CallToolResult, McpError> {
        let wait = input.timeout_ms.unwrap_or(self.default_wait_ms);
        Ok(match self.client.navigate(&input.url, wait).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Snapshot the current page as an accessibility tree. Interactive elements \
                       carry `[eN]` ref tokens that `click` and `type` accept. Requires a prior \
                       `navigate`."
    )]
    pub async fn snapshot(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.snapshot().await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Click an element. Pass exactly one of `ref` (an `eN` token from `snapshot`) \
                       or `selector` (CSS). Returns the page URL after the click (may change on \
                       navigation)."
    )]
    pub async fn click(
        &self,
        Parameters(input): Parameters<TargetInput>,
    ) -> Result<CallToolResult, McpError> {
        let target = match resolve_target(&input.ref_, &input.selector) {
            Ok(t) => t,
            Err(e) => return Ok(e),
        };
        Ok(match self.client.click(target).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Type text into an element. Pass exactly one of `ref` or `selector`, plus \
                       `text`. The element is focused and filled."
    )]
    pub async fn type_text(
        &self,
        Parameters(input): Parameters<TypeInput>,
    ) -> Result<CallToolResult, McpError> {
        let target = match resolve_target(&input.ref_, &input.selector) {
            Ok(t) => t,
            Err(e) => return Ok(e),
        };
        Ok(match self.client.type_text(target, &input.text).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Evaluate a JavaScript expression in the current page and return its value. \
                       Requires a prior `navigate`."
    )]
    pub async fn evaluate(
        &self,
        Parameters(input): Parameters<EvaluateInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.evaluate(&input.expression).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Press a keyboard key on the focused element (e.g. `Enter` to submit a \
                       form after `type`, `Tab`, `Escape`, `ArrowDown`). Requires a prior `navigate`."
    )]
    pub async fn press(
        &self,
        Parameters(input): Parameters<PressInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.press(&input.key).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Scroll the page. `direction` ∈ {up,down,left,right}; `pixels` defaults to \
                       500. Useful for lazy-loaded / infinite-scroll content."
    )]
    pub async fn scroll(
        &self,
        Parameters(input): Parameters<ScrollInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match self.client.scroll(&input.direction, input.pixels.unwrap_or(500)).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            },
        )
    }

    #[tool(description = "Go back one entry in history.")]
    pub async fn back(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.history("back").await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(description = "Go forward one entry in history.")]
    pub async fn forward(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.history("forward").await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(description = "Reload the current page.")]
    pub async fn reload(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.history("refresh").await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Wait for the page to settle (e.g. after a click that triggers async \
                       loading). `timeout_ms` defaults to 5000."
    )]
    pub async fn wait(
        &self,
        Parameters(input): Parameters<WaitInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.wait(input.timeout_ms.unwrap_or(5000)).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Read the page's captured console messages. Set `include_errors` to also \
                       return uncaught JavaScript errors. Requires a prior `navigate`."
    )]
    pub async fn console(
        &self,
        Parameters(input): Parameters<ConsoleInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.console(input.include_errors).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Read or import cookies. `action: get` returns the tab's cookies; \
                       `action: set` imports `cookies` (Playwright format) into the session — \
                       useful for reusing a logged-in state."
    )]
    pub async fn cookies(
        &self,
        Parameters(input): Parameters<CookiesInput>,
    ) -> Result<CallToolResult, McpError> {
        let result = match input.action.as_str() {
            "get" => self.client.cookies_get().await,
            "set" => match input.cookies {
                Some(c) => self.client.cookies_set(c).await,
                None => return Ok(err("`action: set` requires `cookies`")),
            },
            other => return Ok(err(format!("unknown action `{other}`; use `get` or `set`"))),
        };
        Ok(match result {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Extract the page's links (paginated). Returns `{ links, pagination }`. \
                       `limit` defaults to 50, `offset` to 0. Requires a prior `navigate`."
    )]
    pub async fn links(
        &self,
        Parameters(input): Parameters<LinksInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match self.client.links(input.limit.unwrap_or(50), input.offset.unwrap_or(0)).await {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            },
        )
    }

    #[tool(
        description = "Scroll a specific element into view. Pass exactly one of `ref` (an `eN` \
                       token from `snapshot`) or `selector` (CSS)."
    )]
    pub async fn scroll_element(
        &self,
        Parameters(input): Parameters<TargetInput>,
    ) -> Result<CallToolResult, McpError> {
        let target = match resolve_target(&input.ref_, &input.selector) {
            Ok(t) => t,
            Err(e) => return Ok(e),
        };
        Ok(match self.client.scroll_element(target).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(description = "List images on the page with their URLs and dimensions. Requires a \
                          prior `navigate`.")]
    pub async fn images(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.images().await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Evaluate a long-running JavaScript expression (up to 5 minutes) and return \
                       its value. Use over `evaluate` for expressions that await slow work."
    )]
    pub async fn evaluate_extended(
        &self,
        Parameters(input): Parameters<EvaluateExtendedInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(
            match self
                .client
                .evaluate_extended(&input.expression, input.timeout_ms.unwrap_or(60_000))
                .await
            {
                Ok(v) => ok_json(v),
                Err(e) => err(e),
            },
        )
    }

    #[tool(
        description = "Extract structured JSON from the page using a deterministic schema. \
                       `schema` is `{kind:\"object\", fields:{NAME:{kind:KIND, selector:CSS}}}` \
                       where KIND ∈ text|html|attr|url|number. Requires a prior `navigate`."
    )]
    pub async fn extract_structured(
        &self,
        Parameters(input): Parameters<ExtractStructuredInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.extract_structured(input.schema).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Run a single batched action via camofox's `/act` endpoint. `kind` names \
                       the action (click, type, press, scroll, hover, wait, extractStructured, \
                       ...); `params` carries its fields. A lower-level alternative to the \
                       dedicated tools."
    )]
    pub async fn act(
        &self,
        Parameters(input): Parameters<ActInput>,
    ) -> Result<CallToolResult, McpError> {
        let params = input.params.unwrap_or(serde_json::json!({}));
        Ok(match self.client.act(&input.kind, params).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Switch the browser display mode and (for `virtual`) get a noVNC URL to \
                       watch/drive it live. `mode` ∈ headless | headed | virtual. This \
                       recreates the browser context, so the current page is dropped — call \
                       `navigate` again afterwards."
    )]
    pub async fn display(
        &self,
        Parameters(input): Parameters<DisplayInput>,
    ) -> Result<CallToolResult, McpError> {
        let headless = match input.mode.as_str() {
            "headless" => serde_json::json!(true),
            "headed" => serde_json::json!(false),
            "virtual" => serde_json::json!("virtual"),
            other => {
                return Ok(err(format!(
                    "unknown mode `{other}`; use headless | headed | virtual"
                )));
            }
        };
        Ok(match self.client.toggle_display(headless).await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(description = "Close the current page/tab and reset the session. The next `navigate` \
                          opens a fresh tab.")]
    pub async fn close(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.close().await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(description = "Start a Playwright trace recording for the current page.")]
    pub async fn trace_start(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.trace_start().await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(description = "Stop the trace and return the saved ZIP's server-side path.")]
    pub async fn trace_stop(
        &self,
        Parameters(_input): Parameters<EmptyInput>,
    ) -> Result<CallToolResult, McpError> {
        Ok(match self.client.trace_stop().await {
            Ok(v) => ok_json(v),
            Err(e) => err(e),
        })
    }

    #[tool(
        description = "Capture the current page as a PNG image, returned inline. Set `full_page` \
                       to capture beyond the viewport. Requires a prior `navigate`."
    )]
    pub async fn screenshot(
        &self,
        Parameters(input): Parameters<ScreenshotInput>,
    ) -> Result<CallToolResult, McpError> {
        match self.client.screenshot(input.full_page).await {
            Ok(bytes) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Ok(CallToolResult::success(vec![Content::image(
                    b64,
                    "image/png".to_string(),
                )]))
            }
            Err(e) => Ok(err(e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for CamofoxBrowse {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2024_11_05)
            .with_instructions(
                "Interactive browser automation over Camofox (Firefox). Call `navigate` to open \
                 a page, then `snapshot` to see its accessibility tree (`[eN]` refs), and \
                 `click`/`type`/`evaluate`/`screenshot` to drive it."
                    .to_string(),
            )
    }
}
