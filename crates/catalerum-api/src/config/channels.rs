//! Outbound and inbound chat channels (Discord, Telegram, Matrix).

use serde::{Deserialize, Serialize};

use super::Secret;

/// Outbound messaging channels (SOUL §25). Configured channels are the delivery
/// targets for the `notify` tool (§7) and the `Notify { channel }` automation
/// action (§11), routed by **name** across providers. Empty = no channel (the
/// `notify` tool isn't registered).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Discord incoming-webhook URLs keyed by channel name (the `Notify`/`notify`
    /// `channel`). The `default` entry delivers when no channel is named. TOML:
    /// `[channels.discord]` with `name = "https://discord.com/api/webhooks/…"` pairs.
    /// Outbound only (a webhook can't receive).
    pub discord: std::collections::HashMap<String, String>,
    /// Slack incoming-webhook URLs keyed by channel name — the same shape as
    /// `discord`. TOML: `[channels.slack]` with `name = "https://hooks.slack.com/…"`
    /// pairs. Outbound only.
    pub slack: std::collections::HashMap<String, String>,
    /// Telegram bot channels keyed by channel name. Each delivers via a bot's
    /// `sendMessage` to one chat. TOML: a `[channels.telegram.<name>]` table per
    /// channel with `bot_token` + `chat_id` (+ optional `base_url`, `inbound`).
    pub telegram: std::collections::HashMap<String, TelegramChannelConfig>,
    /// Matrix bot channels keyed by channel name. Each sends to + (with `inbound`)
    /// receives from one room over a bot's access token. TOML: a
    /// `[channels.matrix.<name>]` table with `homeserver` + `access_token` +
    /// `room_id` (+ `user_id` for inbound echo-filtering, `inbound`).
    pub matrix: std::collections::HashMap<String, MatrixChannelConfig>,
}

/// A Telegram bot delivery target (SOUL §25): a bot token + the chat it posts to.
/// `base_url` overrides the Bot API host (default `https://api.telegram.org`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TelegramChannelConfig {
    /// Bot token from BotFather (`<id>:<secret>`).
    pub bot_token: Secret,
    /// Destination chat id (a user, group, or channel).
    pub chat_id: String,
    /// Override the Bot API base URL; empty = the public `https://api.telegram.org`.
    pub base_url: String,
    /// Receive inbound messages from this chat via `getUpdates` long-polling, firing
    /// `ChannelMessage` triggers (SOUL §11/§25). Off by default (a poll loop holds a
    /// connection + a single `getUpdates` consumer, so it is opt-in).
    pub inbound: bool,
}

impl TelegramChannelConfig {
    /// Whether this entry has the credentials to deliver.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.bot_token.is_empty() && !self.chat_id.trim().is_empty()
    }
}

/// A Matrix bot channel (SOUL §25): a homeserver + bot access token + a room, used
/// for both delivery (`/send`) and — when `inbound` — receive (`/sync`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MatrixChannelConfig {
    /// Homeserver base URL (e.g. `https://matrix.org`).
    pub homeserver: String,
    /// The bot user's access token.
    pub access_token: Secret,
    /// The room id to send to / receive from (e.g. `!abc:example.org`).
    pub room_id: String,
    /// The bot's own Matrix user id (e.g. `@assistant:example.org`). Required for
    /// `inbound` — it filters the bot's own messages so a reply never re-triggers
    /// the agent (an echo loop). Optional for delivery-only.
    pub user_id: String,
    /// Receive inbound messages from `room_id` via `/sync` long-polling, firing
    /// `ChannelMessage` triggers (SOUL §11/§25). Off by default; ignored (with a
    /// warning) when `user_id` is empty.
    pub inbound: bool,
}

impl MatrixChannelConfig {
    /// Whether this entry has the credentials to deliver.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.homeserver.trim().is_empty()
            && !self.access_token.is_empty()
            && !self.room_id.trim().is_empty()
    }
}

impl ChannelsConfig {
    /// Whether any channel is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.discord.values().all(|u| u.trim().is_empty())
            && self.slack.values().all(|u| u.trim().is_empty())
            && self.telegram.values().all(|t| !t.is_configured())
            && self.matrix.values().all(|m| !m.is_configured())
    }
}
