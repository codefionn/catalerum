//! Slack delivery via an **incoming webhook** (SOUL §25).
//!
//! A Slack incoming webhook is the standard, tokenless way to post into a
//! channel: `POST <webhook_url>` with `{ "text": "<text>" }` — the same shape as
//! a Discord webhook ([`crate::discord`]) under a different body field. The
//! webhook URL is the destination (and the secret), configured per channel
//! instance. Inbound is **not** available over a webhook (a Slack bot would need
//! the Events API + a public endpoint); a relay can still post inbound messages
//! to `POST /channels/{channel}/inbound` (§11).

use async_trait::async_trait;
use serde_json::json;

use crate::{Channel, ChannelError, OutMessage, Result};

/// Slack's per-message limit on `text`, in characters. Slack accepts up to 40000
/// characters per message; a longer body is truncated/rejected, so cap to it
/// before sending (the same defensive cap [`crate::discord`] applies at 2000).
const SLACK_MAX_TEXT: usize = 40_000;

/// Delivers messages to a Slack channel via an incoming-webhook URL.
pub struct SlackWebhookChannel {
    client: reqwest::Client,
    webhook_url: String,
}

impl SlackWebhookChannel {
    /// A channel posting to `webhook_url` (a Slack incoming-webhook URL).
    #[must_use]
    pub fn new(webhook_url: impl Into<String>) -> Self {
        Self {
            client: crate::default_http_client(),
            webhook_url: webhook_url.into(),
        }
    }

    /// Build with a shared [`reqwest::Client`] (connection-pool reuse).
    #[must_use]
    pub fn with_client(client: reqwest::Client, webhook_url: impl Into<String>) -> Self {
        Self {
            client,
            webhook_url: webhook_url.into(),
        }
    }
}

#[async_trait]
impl Channel for SlackWebhookChannel {
    fn kind(&self) -> &str {
        "slack"
    }

    async fn send(&self, msg: &OutMessage) -> Result<()> {
        let resp = self
            .client
            .post(&self.webhook_url)
            .json(&json!({ "text": crate::truncate_message(&msg.text, SLACK_MAX_TEXT) }))
            .send()
            .await
            .map_err(|e| ChannelError::Request(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[tokio::test]
    async fn send_posts_slack_webhook_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/T/B/x"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let channel = SlackWebhookChannel::new(format!("{}/services/T/B/x", server.uri()));
        assert_eq!(channel.kind(), "slack");
        channel
            .send(&OutMessage::text("deploy finished ✅"))
            .await
            .unwrap();

        // The body is the Slack webhook shape: `{ "text": "<text>" }`.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let sent: Request = requests.into_iter().next().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body, json!({ "text": "deploy finished ✅" }));
    }

    #[tokio::test]
    async fn over_limit_text_is_capped_before_sending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let channel = SlackWebhookChannel::new(server.uri());
        channel
            .send(&OutMessage::text("a".repeat(SLACK_MAX_TEXT + 500)))
            .await
            .unwrap();

        let sent = server.received_requests().await.unwrap().remove(0);
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        let text = body["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), SLACK_MAX_TEXT, "capped to the limit");
        assert!(text.ends_with(" […]"), "truncation is marked");
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403).set_body_string("invalid_token"))
            .mount(&server)
            .await;

        let channel = SlackWebhookChannel::new(server.uri());
        let err = channel.send(&OutMessage::text("x")).await.unwrap_err();
        match err {
            ChannelError::Status { status, body } => {
                assert_eq!(status, 403);
                assert!(body.contains("invalid_token"));
            }
            other => panic!("expected a Status error, got {other:?}"),
        }
    }
}
