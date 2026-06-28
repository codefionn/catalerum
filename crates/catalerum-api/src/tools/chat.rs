//! Chat-history tools: `search_messages` / `read_conversation`.

use super::*;

/// Max characters of a message returned by `search_messages` — a match-centred,
/// ellipsized window so a long turn doesn't blow the agent's context.
pub(crate) const MESSAGE_SNIPPET_CHARS: usize = 240;

/// A short, **match-centred** excerpt of `content` for a `search_messages` hit: a
/// window of up to `max` characters centred on the first case-insensitive
/// occurrence of `query`, ellipsized (`…`) where it was clipped. Char-safe — counts
/// `char`s and never splits a UTF-8 boundary, so it can't panic on multi-byte text.
/// If `query` isn't found (it shouldn't be — the SQL already matched it — but
/// defensively), the window falls back to the head of `content`.
pub(crate) fn match_snippet(content: &str, query: &str, max: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max {
        return content.to_string();
    }
    // Match offset as a CHAR index (lowercasing can change byte length, so count
    // chars in the lowered prefix), clamped so an expanding-lowercase edge can't
    // push it past the text.
    let lower = content.to_lowercase();
    let match_char = lower
        .find(&query.to_lowercase())
        .map(|b| lower[..b].chars().count())
        .unwrap_or(0)
        .min(chars.len());
    // A `max`-char window centred on the match, clamped to the text bounds.
    let start = match_char.saturating_sub(max / 2);
    let end = (start + max).min(chars.len());
    let start = end.saturating_sub(max); // pull back if we clamped against the tail
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

/// `search_messages` (SOUL §7/§12) — literal full-text search over past chat
/// messages; the agent-tool counterpart of the History panel's message search, and
/// the **only** way for the agent to find chat history (messages aren't embedded,
/// so `search_semantic` can't reach them). Thin store client
/// (`MessageRepo::search_in_workspace`), gated on `conversation:read`, workspace-
/// scoped.
pub(crate) struct SearchMessagesTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SearchMessagesTool {
    fn name(&self) -> &str {
        "search_messages"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "conversation")
    }
    fn description(&self) -> &str {
        "Search past chat messages by the exact text in them — a literal, \
         case-insensitive substring search over conversation content. This is the \
         only way to recall chat history (messages are not in the semantic index, \
         so search_semantic can't find them). Use it to recall what was said \
         earlier (e.g. \"what did we decide about the migration?\"). Each hit gives \
         the conversation title and id, the message role, a match-centred snippet, \
         and when it was sent; newest match first."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Exact text to find in past messages (case-insensitive substring; %/_ are literal)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as i64;
        let hits = self
            .store
            .messages()
            .search_in_workspace(ws, &query, limit)
            .await
            .map_err(|e| Error::provider(format!("message search failed: {e}")))?;
        let results: Vec<Json> = hits
            .into_iter()
            .map(|h| {
                json!({
                    "message_id": h.message.id,
                    "conversation_id": h.message.conversation_id,
                    "conversation_title": h.conversation_title,
                    "role": h.message.role,
                    "snippet": match_snippet(&h.message.content, &query, MESSAGE_SNIPPET_CHARS),
                    "created_at": h.message.created_at,
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }
}

/// Default / max messages a `read_conversation` returns (the most recent, oldest-
/// first) — a bounded thread window so a long conversation can't blow the agent's
/// context.
pub(crate) const CONV_READ_DEFAULT: u64 = 30;
pub(crate) const CONV_READ_MAX: u64 = 100;
/// Per-message content cap (chars) for a thread read — one giant pasted turn can't
/// dominate the window; `read_conversation`'s `truncated` flag marks a clipped one.
pub(crate) const CONV_MSG_CHARS: usize = 2000;

/// Truncate `s` to at most `max` **chars** (char-safe — never splits a UTF-8
/// boundary), flagging whether it was clipped.
pub(crate) fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

/// `read_conversation` (SOUL §7/§12) — read one chat thread's recent messages by
/// id, the read half of `search_messages` (a hit gives a `conversation_id`; this
/// pulls the surrounding thread). Thin store client, gated on `conversation:read`,
/// workspace-scoped — the `conversations().get` confirms the thread is in **this**
/// workspace before any message is read (NotFound never leaks another tenant's
/// chat).
pub(crate) struct ReadConversationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ReadConversationTool {
    fn name(&self) -> &str {
        "read_conversation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "conversation")
    }
    fn description(&self) -> &str {
        "Read a past chat conversation's messages by its id (the `conversation_id` \
         from a search_messages hit). Returns the thread's title plus its most \
         recent messages, oldest-first — each message's role, text, and time — so \
         you can recall the full context around a search hit. Long messages are \
         truncated (a `truncated` flag marks them); raise `limit` for more turns."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Conversation id (a UUID from a search_messages hit)." },
                "limit": {
                    "type": "integer",
                    "description": "Max recent messages to return (1-100, default 30).",
                    "minimum": 1,
                    "maximum": 100
                }
            },
            "required": ["id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id: ConversationId = parse_id(&args, "id")?;
        let limit = opt_clamped_u64(&args, "limit", CONV_READ_DEFAULT, CONV_READ_MAX) as i64;
        // Confirm the conversation is in THIS workspace first (NotFound otherwise —
        // never leaks another tenant's thread); then read its recent messages.
        let conv = self.store.conversations().get(ws, id).await?;
        let msgs = self
            .store
            .messages()
            .list_recent(id, limit)
            .await
            .map_err(|e| Error::provider(format!("read conversation failed: {e}")))?;
        let messages: Vec<Json> = msgs
            .into_iter()
            .map(|m| {
                let (content, truncated) = truncate_chars(&m.content, CONV_MSG_CHARS);
                json!({
                    "role": m.role,
                    "content": content,
                    "truncated": truncated,
                    "created_at": m.created_at,
                })
            })
            .collect();
        Ok(json!({
            "id": conv.id,
            "title": conv.title,
            "created_at": conv.created_at,
            "messages": messages,
        }))
    }
}
