//! Thin async client over the camofox-browser REST API (`:9377`). One client
//! owns one persistent tab (created lazily on first `navigate`) so an MCP agent
//! can drive a stateful session: navigate → snapshot → click/type → evaluate.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::RwLock;

/// Fixed identity for this server's camofox session. `userId` keys an isolated
/// Firefox profile; `sessionKey` a browser context. `/tabs` requires both.
const USER_ID: &str = "mcp";
const SESSION_KEY: &str = "browse";

/// Errors are surfaced to the MCP layer as plain strings — the agent only needs
/// a readable message, not a typed taxonomy.
pub type ClientResult<T> = Result<T, String>;

pub struct CamofoxClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    /// Current tab id, created lazily on first `navigate`.
    tab: RwLock<Option<String>>,
}

impl CamofoxClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            tab: RwLock::new(None),
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    async fn post(&self, path: &str, body: Value) -> ClientResult<Value> {
        let resp = self
            .auth(self.http.post(format!("{}{path}", self.base_url)))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("camofox request failed: {e}"))?;
        let status = resp.status();
        let val: Value = resp
            .json()
            .await
            .map_err(|e| format!("camofox returned non-JSON ({status}): {e}"))?;
        if !status.is_success() {
            let msg = val
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("camofox {path} -> {status}: {msg}"));
        }
        Ok(val)
    }

    async fn get(&self, path: &str) -> ClientResult<reqwest::Response> {
        let resp = self
            .auth(self.http.get(format!("{}{path}", self.base_url)))
            .send()
            .await
            .map_err(|e| format!("camofox request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("camofox {path} -> {}", resp.status()));
        }
        Ok(resp)
    }

    /// Health check — pre-launches the browser and confirms connectivity.
    pub async fn health(&self) -> ClientResult<Value> {
        let resp = self.get("/health").await?;
        resp.json()
            .await
            .map_err(|e| format!("camofox /health non-JSON: {e}"))
    }

    /// Current tab id, or an error prompting the agent to `navigate` first.
    async fn require_tab(&self) -> ClientResult<String> {
        self.tab
            .read()
            .await
            .clone()
            .ok_or_else(|| "no active page; call `navigate` first".to_string())
    }

    /// Navigate the session's tab to `url` (creating the tab on first call),
    /// then wait for readiness. Returns `{ url }`.
    pub async fn navigate(&self, url: &str, wait_ms: u64) -> ClientResult<Value> {
        let existing = self.tab.read().await.clone();
        let tab = match existing {
            Some(id) => {
                self.post(
                    &format!("/tabs/{id}/navigate"),
                    json!({ "userId": USER_ID, "url": url }),
                )
                .await?;
                id
            }
            None => {
                let created = self
                    .post(
                        "/tabs",
                        json!({ "userId": USER_ID, "sessionKey": SESSION_KEY, "url": url }),
                    )
                    .await?;
                let id = created
                    .get("tabId")
                    .and_then(|v| v.as_str())
                    .ok_or("camofox /tabs: missing tabId")?
                    .to_string();
                *self.tab.write().await = Some(id.clone());
                id
            }
        };
        self.post(
            &format!("/tabs/{tab}/wait"),
            json!({ "userId": USER_ID, "timeout": wait_ms }),
        )
        .await?;
        Ok(json!({ "url": url }))
    }

    /// Accessibility-tree snapshot with `[eN]` refs the interaction tools accept.
    pub async fn snapshot(&self) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        let resp = self
            .get(&format!("/tabs/{tab}/snapshot?userId={USER_ID}"))
            .await?;
        resp.json()
            .await
            .map_err(|e| format!("camofox snapshot non-JSON: {e}"))
    }

    /// Click an element by `eN` ref or CSS selector. Exactly one is expected.
    pub async fn click(&self, target: Target<'_>) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(&format!("/tabs/{tab}/click"), target.body()).await
    }

    /// Type text into an element by ref or selector.
    pub async fn type_text(&self, target: Target<'_>, text: &str) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        let mut body = target.body();
        body["text"] = json!(text);
        self.post(&format!("/tabs/{tab}/type"), body).await
    }

    /// Evaluate a JS expression and return camofox's `{ result, resultType }`.
    pub async fn evaluate(&self, expression: &str) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/evaluate"),
            json!({ "userId": USER_ID, "expression": expression }),
        )
        .await
    }

    /// Press a keyboard key (e.g. `Enter`, `Tab`, `ArrowDown`) — used to submit
    /// forms after `type`.
    pub async fn press(&self, key: &str) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/press"),
            json!({ "userId": USER_ID, "key": key }),
        )
        .await
    }

    /// Scroll the page in a direction by a pixel amount.
    pub async fn scroll(&self, direction: &str, pixels: u32) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/scroll"),
            json!({ "userId": USER_ID, "direction": direction, "pixels": pixels }),
        )
        .await
    }

    /// History / reload navigation. `which` ∈ {`back`,`forward`,`refresh`}.
    pub async fn history(&self, which: &str) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/{which}"),
            json!({ "userId": USER_ID }),
        )
        .await
    }

    /// Wait for the page to settle, bounded by `timeout_ms`.
    pub async fn wait(&self, timeout_ms: u64) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/wait"),
            json!({ "userId": USER_ID, "timeout": timeout_ms }),
        )
        .await
    }

    /// Drain captured console messages (and, when `include_errors`, uncaught
    /// JS errors) for the current page.
    pub async fn console(&self, include_errors: bool) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        let console = self
            .get(&format!("/tabs/{tab}/console?userId={USER_ID}"))
            .await?
            .json::<Value>()
            .await
            .map_err(|e| format!("camofox console non-JSON: {e}"))?;
        if !include_errors {
            return Ok(console);
        }
        let errors = self
            .get(&format!("/tabs/{tab}/errors?userId={USER_ID}"))
            .await?
            .json::<Value>()
            .await
            .map_err(|e| format!("camofox errors non-JSON: {e}"))?;
        Ok(json!({ "console": console, "errors": errors }))
    }

    /// Read this tab's cookies (camofox returns a bare array).
    pub async fn cookies_get(&self) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.get(&format!("/tabs/{tab}/cookies?userId={USER_ID}"))
            .await?
            .json::<Value>()
            .await
            .map_err(|e| format!("camofox cookies non-JSON: {e}"))
    }

    /// Import cookies into the session (Playwright cookie format).
    pub async fn cookies_set(&self, cookies: Value) -> ClientResult<Value> {
        self.post(
            &format!("/sessions/{USER_ID}/cookies"),
            json!({ "cookies": cookies }),
        )
        .await
    }

    /// Extract page links (paginated). Returns `{ links, pagination }`.
    pub async fn links(&self, limit: u32, offset: u32) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.get(&format!(
            "/tabs/{tab}/links?userId={USER_ID}&limit={limit}&offset={offset}"
        ))
        .await?
        .json::<Value>()
        .await
        .map_err(|e| format!("camofox links non-JSON: {e}"))
    }

    /// Forget the current tab so the next `navigate` creates a fresh one. Used
    /// after operations that invalidate tabs server-side (`toggle_display`) or
    /// explicit `close`.
    async fn clear_tab(&self) {
        *self.tab.write().await = None;
    }

    /// Scroll a specific element into view (by `eN` ref or CSS selector).
    pub async fn scroll_element(&self, target: Target<'_>) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(&format!("/tabs/{tab}/scroll-element"), target.body())
            .await
    }

    /// List images on the page (`{ images, container }`).
    pub async fn images(&self) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.get(&format!("/tabs/{tab}/images?userId={USER_ID}"))
            .await?
            .json::<Value>()
            .await
            .map_err(|e| format!("camofox images non-JSON: {e}"))
    }

    /// Evaluate a long-running JS expression (up to `timeout_ms`, server cap 300s).
    pub async fn evaluate_extended(&self, expression: &str, timeout_ms: u64) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/evaluate-extended"),
            json!({ "userId": USER_ID, "expression": expression, "timeout": timeout_ms }),
        )
        .await
    }

    /// Deterministic schema-based extraction. `schema` is camofox's extract
    /// schema, e.g. `{kind:"object", fields:{title:{kind:"text", selector:"h1"}}}`
    /// (field `kind` ∈ text|html|attr|url|number).
    pub async fn extract_structured(&self, schema: Value) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/extract-structured"),
            json!({ "userId": USER_ID, "schema": schema }),
        )
        .await
    }

    /// OpenClaw-compatible batched action endpoint. `kind` names the action
    /// (`click`, `type`, `press`, `scroll`, `hover`, `wait`, `extractStructured`,
    /// ...); `params` carries the action's remaining fields, merged into the body.
    pub async fn act(&self, kind: &str, params: Value) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        let mut body = json!({ "userId": USER_ID, "targetId": tab, "kind": kind });
        if let (Some(obj), Some(map)) = (body.as_object_mut(), params.as_object()) {
            for (k, v) in map {
                obj.insert(k.clone(), v.clone());
            }
        }
        self.post("/act", body).await
    }

    /// Switch the browser's display mode and return a `vncUrl` for watching it.
    /// `headless` ∈ `true` (headless), `false` (headed), or `"virtual"` (Xvfb+VNC).
    /// Invalidates the current tab, so we drop it; the next `navigate` reopens.
    pub async fn toggle_display(&self, headless: Value) -> ClientResult<Value> {
        let result = self
            .post(
                &format!("/sessions/{USER_ID}/toggle-display"),
                json!({ "headless": headless }),
            )
            .await?;
        self.clear_tab().await;
        Ok(result)
    }

    /// Close the current tab and reset the session's tab handle.
    pub async fn close(&self) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        let result = self
            .auth(self.http.delete(format!("{}/tabs/{tab}", self.base_url)))
            .json(&json!({ "userId": USER_ID }))
            .send()
            .await
            .map_err(|e| format!("camofox close failed: {e}"))?
            .json::<Value>()
            .await
            .unwrap_or_else(|_| json!({ "ok": true }));
        self.clear_tab().await;
        Ok(result)
    }

    /// Start a Playwright trace recording for the current tab.
    pub async fn trace_start(&self) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/trace/start"),
            json!({ "userId": USER_ID }),
        )
        .await
    }

    /// Stop the trace and return the saved ZIP's server-side location.
    pub async fn trace_stop(&self) -> ClientResult<Value> {
        let tab = self.require_tab().await?;
        self.post(
            &format!("/tabs/{tab}/trace/stop"),
            json!({ "userId": USER_ID }),
        )
        .await
    }

    /// Capture a PNG screenshot. Returns the raw bytes (caller base64-encodes).
    pub async fn screenshot(&self, full_page: bool) -> ClientResult<Vec<u8>> {
        let tab = self.require_tab().await?;
        let resp = self
            .get(&format!(
                "/tabs/{tab}/screenshot?userId={USER_ID}&fullPage={full_page}"
            ))
            .await?;
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("camofox screenshot read failed: {e}"))
    }
}

/// A click/type target — either an `eN` ref or a CSS selector.
pub enum Target<'a> {
    Ref(&'a str),
    Selector(&'a str),
}

impl Target<'_> {
    fn body(&self) -> Value {
        match self {
            Target::Ref(r) => json!({ "userId": USER_ID, "ref": r }),
            Target::Selector(s) => json!({ "userId": USER_ID, "selector": s }),
        }
    }
}
