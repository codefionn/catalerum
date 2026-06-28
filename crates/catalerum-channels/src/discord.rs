//! Discord delivery via an **incoming webhook** (SOUL §25).
//!
//! A Discord incoming webhook is the standard, tokenless way to post into a
//! channel: `POST <webhook_url>` with `{ "content": "<text>" }`. The webhook URL
//! is the destination (and the secret), configured per channel instance. (Slack
//! incoming webhooks share the same shape under a different field, a trivial
//! variant for later.)

use async_trait::async_trait;
use serde_json::json;

use crate::{Channel, ChannelError, OutMessage, Result};

/// Discord's hard limit on webhook `content` length, in Unicode code points.
/// A longer message is rejected with a `400`, so cap to it before sending.
const DISCORD_MAX_CONTENT: usize = 2000;

/// Delivers messages to a Discord channel via an incoming-webhook URL.
pub struct DiscordWebhookChannel {
    client: reqwest::Client,
    webhook_url: String,
}

impl DiscordWebhookChannel {
    /// A channel posting to `webhook_url` (a Discord incoming-webhook URL).
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
impl Channel for DiscordWebhookChannel {
    fn kind(&self) -> &str {
        "discord"
    }

    async fn send(&self, msg: &OutMessage) -> Result<()> {
        let resp = self
            .client
            .post(&self.webhook_url)
            .json(&json!({ "content": crate::truncate_message(&msg.text, DISCORD_MAX_CONTENT) }))
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
    async fn send_posts_discord_webhook_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/webhooks/abc/def"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let channel = DiscordWebhookChannel::new(format!("{}/webhooks/abc/def", server.uri()));
        assert_eq!(channel.kind(), "discord");
        channel
            .send(&OutMessage::text("deploy finished ✅"))
            .await
            .unwrap();

        // The body is the Discord webhook shape: `{ "content": "<text>" }`.
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let sent: Request = requests.into_iter().next().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body, json!({ "content": "deploy finished ✅" }));
    }

    #[tokio::test]
    async fn over_limit_content_is_capped_before_sending() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let channel = DiscordWebhookChannel::new(format!("{}/webhooks/abc/def", server.uri()));
        // A 3000-char agent message would be rejected by Discord (2000 cap).
        channel
            .send(&OutMessage::text("a".repeat(3000)))
            .await
            .unwrap();

        let sent = server.received_requests().await.unwrap().remove(0);
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        let content = body["content"].as_str().unwrap();
        assert_eq!(
            content.chars().count(),
            DISCORD_MAX_CONTENT,
            "capped to the limit"
        );
        assert!(content.ends_with(" […]"), "truncation is marked");
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let channel = DiscordWebhookChannel::new(server.uri());
        let err = channel.send(&OutMessage::text("x")).await.unwrap_err();
        match err {
            ChannelError::Status { status, body } => {
                assert_eq!(status, 500);
                assert!(body.contains("boom"));
            }
            other => panic!("expected a Status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_host_is_a_request_error() {
        // Nothing is listening on this port → a connection (request) error.
        let channel = DiscordWebhookChannel::new("http://127.0.0.1:1/webhook");
        let err = channel.send(&OutMessage::text("x")).await.unwrap_err();
        assert!(matches!(err, ChannelError::Request(_)));
    }
}
