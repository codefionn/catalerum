//! Headless-browser fetch backend over the Chrome DevTools Protocol (SOUL §27).
//!
//! This is the JavaScript-rendering "browser control" path — the Playwright /
//! Chromium integration. Rather than bundle a browser, it speaks CDP over a
//! WebSocket to one you already run: a headless Chrome/Chromium started with
//! `--remote-debugging-port`, a Playwright `browserServer.wsEndpoint()`, or a
//! hosted Browserless/`chromedp` endpoint. catalerum stays the controller; the
//! browser is just another provider reached through a trait (SOUL §3.2).
//!
//! The flow is deliberately small: open a target, navigate, wait for the document
//! to settle (and optionally a selector), snapshot `document.documentElement
//! .outerHTML`, then run it through the same HTML→Markdown converter every other
//! backend uses. The target URL is SSRF-guarded before we connect (SOUL §19).
//!
//! Enable with the `browser` feature.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value as Json};
use tokio::time::{timeout, Duration, Instant};
use tokio_tungstenite::tungstenite::Message;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{FetchRequest, FetchedPage, WebFetcher};

use crate::http::render;
use crate::policy::FetchPolicy;

/// A CDP/WebSocket [`WebFetcher`] driving an external browser (SOUL §27).
#[derive(Clone)]
pub struct CdpFetcher {
    /// CDP WebSocket endpoint, e.g. `ws://localhost:9222/devtools/browser/<id>`.
    ws_url: String,
    policy: FetchPolicy,
    default_timeout_secs: u64,
}

impl CdpFetcher {
    /// Build a fetcher that connects to the given CDP WebSocket endpoint.
    #[must_use]
    pub fn new(ws_url: impl Into<String>, default_timeout_secs: u64, policy: FetchPolicy) -> Self {
        Self {
            ws_url: ws_url.into(),
            policy,
            default_timeout_secs: default_timeout_secs.max(1),
        }
    }
}

#[async_trait]
impl WebFetcher for CdpFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchedPage> {
        let url = self.policy.validate(&request.url)?;
        self.policy.guard_resolved(&url).await?;

        let budget = Duration::from_secs(
            request
                .timeout_secs
                .unwrap_or(self.default_timeout_secs)
                .max(1),
        );

        let html = timeout(budget, self.render_html(&request, budget))
            .await
            .map_err(|_| Error::Timeout)??;

        Ok(render(
            &request,
            &url,
            200,
            Some("text/html".to_string()),
            &html,
            html.len() as u64,
        ))
    }
}

impl CdpFetcher {
    /// Drive the browser to a settled DOM snapshot for `request.url`.
    async fn render_html(&self, request: &FetchRequest, budget: Duration) -> Result<String> {
        let (mut ws, _resp) = tokio_tungstenite::connect_async(&self.ws_url)
            .await
            .map_err(|e| Error::provider(format!("cdp connect to {} failed: {e}", self.ws_url)))?;

        let mut session = Session::new();

        // Open and attach to a fresh tab so we get a session id to scope page
        // commands to (`flatten` routes them over this one socket).
        let created = session
            .call(
                &mut ws,
                "Target.createTarget",
                json!({ "url": "about:blank" }),
                None,
            )
            .await?;
        let target_id = created
            .get("targetId")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::provider("cdp: missing/empty targetId"))?
            .to_string();

        let attached = session
            .call(
                &mut ws,
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
            )
            .await?;
        let sid = attached
            .get("sessionId")
            .and_then(Json::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::provider("cdp: missing/empty sessionId"))?
            .to_string();

        session
            .call(&mut ws, "Page.enable", json!({}), Some(&sid))
            .await?;
        session
            .call(
                &mut ws,
                "Page.navigate",
                json!({ "url": request.url }),
                Some(&sid),
            )
            .await?;

        // Poll the document until it is `complete` (and the optional selector is
        // present), within the remaining budget.
        wait_until_ready(
            &mut session,
            &mut ws,
            &sid,
            request.wait_for.as_deref(),
            budget,
        )
        .await?;

        let snapshot = session
            .call(
                &mut ws,
                "Runtime.evaluate",
                json!({
                    "expression": "document.documentElement.outerHTML",
                    "returnByValue": true,
                }),
                Some(&sid),
            )
            .await?;
        let html = snapshot
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(Json::as_str)
            .ok_or_else(|| Error::provider("cdp: outerHTML was not a string"))?;
        // Honour the policy byte cap on the rendered DOM (SOUL §27).
        let html = crate::policy::cap_str(html, self.policy.max_bytes).to_string();

        // Best-effort cleanup; ignore failures (the tab dies with the socket).
        let _ = session
            .call(
                &mut ws,
                "Target.closeTarget",
                json!({ "targetId": target_id }),
                None,
            )
            .await;
        let _ = ws.close(None).await;

        Ok(html)
    }
}

/// Wait for `document.readyState === "complete"`, then (if asked) for a selector,
/// polling every 100ms until the time budget runs out.
async fn wait_until_ready<S>(
    session: &mut Session,
    ws: &mut S,
    sid: &str,
    wait_for: Option<&str>,
    budget: Duration,
) -> Result<()>
where
    S: SinkExt<Message>
        + StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
    <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
{
    let deadline = Instant::now() + budget;
    let expr = match wait_for {
        Some(sel) => format!(
            "document.readyState === 'complete' && !!document.querySelector({})",
            json!(sel)
        ),
        None => "document.readyState === 'complete'".to_string(),
    };
    loop {
        let res = session
            .call(
                ws,
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true }),
                Some(sid),
            )
            .await?;
        if res.get("result").and_then(|r| r.get("value")) == Some(&Json::Bool(true)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Settle for whatever is loaded rather than failing outright.
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Tracks the CDP request-id counter and correlates responses.
struct Session {
    next_id: u64,
}

impl Session {
    fn new() -> Self {
        Self { next_id: 1 }
    }

    /// Send a CDP command and return its `result` object, ignoring any events
    /// that arrive before the matching response.
    async fn call<S>(
        &mut self,
        ws: &mut S,
        method: &str,
        params: Json,
        session_id: Option<&str>,
    ) -> Result<Json>
    where
        S: SinkExt<Message>
            + StreamExt<Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Unpin,
        <S as futures_util::Sink<Message>>::Error: std::fmt::Display,
    {
        let id = self.next_id;
        self.next_id += 1;

        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            msg["sessionId"] = json!(sid);
        }
        ws.send(Message::text(msg.to_string()))
            .await
            .map_err(|e| Error::provider(format!("cdp send `{method}` failed: {e}")))?;

        while let Some(frame) = ws.next().await {
            let frame = frame.map_err(|e| Error::provider(format!("cdp recv failed: {e}")))?;
            let text = match &frame {
                Message::Text(t) => t.as_str(),
                Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
                Message::Close(_) => return Err(Error::provider("cdp socket closed")),
                Message::Frame(_) => continue,
            };
            let value: Json = match serde_json::from_str(text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Events have no `id`; skip until our response arrives.
            if value.get("id").and_then(Json::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                let m = err
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or("cdp error");
                return Err(Error::provider(format!("cdp `{method}`: {m}")));
            }
            return Ok(value.get("result").cloned().unwrap_or(Json::Null));
        }
        Err(Error::provider(format!("cdp `{method}`: connection ended")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_with_endpoint() {
        let f = CdpFetcher::new(
            "ws://localhost:9222/devtools/browser/x",
            20,
            FetchPolicy::default(),
        );
        assert_eq!(f.ws_url, "ws://localhost:9222/devtools/browser/x");
        assert_eq!(f.default_timeout_secs, 20);
    }

    #[tokio::test]
    async fn validates_target_before_connecting() {
        // A blocked target must fail at the policy gate, never attempting a CDP
        // connection to the (here, bogus) endpoint.
        let f = CdpFetcher::new("ws://127.0.0.1:1/", 5, FetchPolicy::default());
        let err = f
            .fetch(FetchRequest::new("http://localhost/secret"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)), "got {err:?}");
    }
}
