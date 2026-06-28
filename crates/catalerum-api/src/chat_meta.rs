//! Persistent chat-thread auto-title + auto-tag (SOUL §12).
//!
//! The web client titles a brand-new thread **optimistically** from its opening
//! message (`derive_title`, first 50 chars). This module is the durable
//! replacement: after a turn finishes, a background task replays a bounded
//! transcript window through one small, tool-less model call that returns a
//! concise title plus 0-3 topic tags, then persists both (tags always; the title
//! only while the thread has no user-chosen name — `title_manual`, set by an
//! explicit rename, pins the title against the generator).
//!
//! Everything here is best-effort and off the hot path, exactly like the
//! thread-compaction pass it sits beside: the client already has its
//! `message_done`; the generated metadata lands whenever it lands and the
//! sidebar picks it up on its next refresh/poll.

use tracing::warn;

use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{Message, MessageRole};
use catalerum_llm::compact::{cap_transcript, render_transcript};

use crate::chat_context::to_chat_messages;
use crate::state::AppState;
use catalerum_core::{ConversationId, WorkspaceId};

/// How many messages of the transcript the generator reads (same bounded window
/// the compactor and the next-turn seed use).
const META_WINDOW_MESSAGES: usize = 20;

/// Cap on the rendered transcript handed to the generator (chars ≈ tokens × 4).
const META_TRANSCRIPT_MAX_CHARS: usize = 6_000;

/// The cap includes reasoning tokens on reasoning models. Two hundred tokens was
/// small enough for those models to finish thinking without emitting any JSON.
const META_MAX_TOKENS: u32 = 512;

/// Hard title limit shared by the prompt, structured-output schema and parser.
const META_TITLE_MAX_CHARS: usize = 60;

/// Tags are shown as sidebar pills; more than three is noise.
const META_MAX_TAGS: usize = 3;

/// Characters per tag; keeps pills single-line.
const META_TAG_MAX_CHARS: usize = 24;

const META_SYSTEM_PROMPT: &str = "Write a short, specific title for this chat and up to three topic tags. Return only JSON matching the supplied schema.";

/// Post-turn hook (called from the ws chat handler): spawn the background
/// metadata generation for a thread that just finished a turn. Never fails the
/// turn; the spawned task logs its own failures.
pub(crate) async fn maybe_generate_meta(
    state: &AppState,
    workspace_id: WorkspaceId,
    conversation_id: ConversationId,
    model: String,
) {
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = generate_meta(&state, workspace_id, conversation_id, &model).await {
            warn!(%conversation_id, error = %e, "chat metadata generation failed");
        }
    });
}

/// One background pass: read the bounded transcript window, ask the model for a
/// title + tags, parse the (fenced or bare) JSON reply, persist via
/// `set_generated_meta` (which re-checks `title_manual` — a rename that landed
/// while the call was in flight still wins).
async fn generate_meta(
    state: &AppState,
    workspace_id: WorkspaceId,
    conversation_id: ConversationId,
    model: &str,
) -> Result<(), String> {
    // Re-read fresh: the conversation may have been deleted since the hook fired.
    let conversation = state
        .store()
        .conversations()
        .get(workspace_id, conversation_id)
        .await
        .map_err(|e| format!("loading conversation: {e}"))?;
    let messages = state
        .store()
        .messages()
        .list_recent(
            conversation_id,
            i64::try_from(META_WINDOW_MESSAGES).unwrap_or(20),
        )
        .await
        .map_err(|e| format!("loading messages: {e}"))?;
    // Nothing to relabel while the thread still has no exchange.
    if messages.len() < 2 {
        return Ok(());
    }
    let transcript = cap_transcript(
        render_transcript(&to_chat_messages(&messages)),
        META_TRANSCRIPT_MAX_CHARS,
    );
    // Keep a stable local fallback across later turns. A web chat already has
    // its optimistic opening-message title; only conversations created without
    // one need to derive it from the bounded transcript.
    let fallback_title = conversation
        .title
        .as_deref()
        .and_then(fallback_title_from_content)
        .or_else(|| fallback_title(&messages));
    let generated = if model.eq_ignore_ascii_case("echo") {
        // The bundled development provider repeats the prompt verbatim. Calling
        // it cannot generate metadata and could mistake JSON in the chat for its
        // own response.
        None
    } else {
        let req = metadata_request(model, &transcript);
        match state.llm().chat(req).await {
            Ok(turn) => {
                let parsed = parse_meta(&turn.content);
                if parsed.is_none() {
                    warn!(
                        %conversation_id,
                        model,
                        content_chars = turn.content.chars().count(),
                        reasoning_chars = turn.reasoning.chars().count(),
                        finish_reason = ?turn.finish_reason,
                        "chat metadata model returned no usable JSON; using local title"
                    );
                }
                parsed
            }
            Err(error) => {
                warn!(
                    %conversation_id,
                    model,
                    error = %error,
                    "chat metadata model call failed; using local title"
                );
                None
            }
        }
    };
    let (generated_title, tags) = generated.unwrap_or_default();
    let title = generated_title.or(fallback_title);
    state
        .store()
        .conversations()
        .set_generated_meta(workspace_id, conversation_id, title.as_deref(), &tags)
        .await
        .map_err(|e| format!("persisting metadata: {e}"))?;
    tracing::info!(%conversation_id, "chat thread auto-titled/tagged");
    Ok(())
}

/// Build a small structured-output request. Low reasoning keeps the model from
/// spending the whole cap before it writes the title. Providers that reject the
/// schema are handled by the local fallback in [`generate_meta`].
fn metadata_request(model: &str, transcript: &str) -> ChatRequest {
    let mut req = ChatRequest::new(
        model,
        vec![
            ChatMessage::system(META_SYSTEM_PROMPT),
            ChatMessage::user(format!("Conversation transcript:\n\n{transcript}")),
        ],
    );
    req.max_tokens = Some(META_MAX_TOKENS);
    req.reasoning_effort = Some("low".to_string());
    req.extra.insert(
        "text".to_string(),
        serde_json::json!({
            "format": {
                "type": "json_schema",
                "name": "conversation_metadata",
                "strict": true,
                "schema": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "maxLength": META_TITLE_MAX_CHARS
                        },
                        "tags": {
                            "type": "array",
                            "maxItems": META_MAX_TAGS,
                            "items": {
                                "type": "string",
                                "maxLength": META_TAG_MAX_CHARS
                            }
                        }
                    },
                    "required": ["title", "tags"],
                    "additionalProperties": false
                }
            }
        }),
    );
    req
}

/// Produce a useful title even when the configured model is offline, is the
/// bundled echo provider, or returns malformed output. This intentionally does
/// only light cleanup. It should be predictable, not pretend to be a second
/// language model.
fn fallback_title(messages: &[Message]) -> Option<String> {
    let opening = messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .map(|message| message.content.as_str())?;
    fallback_title_from_content(opening)
}

fn fallback_title_from_content(opening: &str) -> Option<String> {
    let mut title = clean_label(opening, META_TITLE_MAX_CHARS);
    for prefix in [
        "please ",
        "can you ",
        "could you ",
        "would you ",
        "i need you to ",
    ] {
        if title
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            title = title[prefix.len()..].trim_start().to_string();
            break;
        }
    }
    let title = title.trim_matches(|c: char| matches!(c, '.' | ',' | ':' | ';' | '!' | '?'));
    (!title.is_empty()).then(|| uppercase_first(title))
}

fn clean_label(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn uppercase_first(value: &str) -> String {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().chain(chars).collect()
}

/// Parse the model reply into `(title, tags)`. Tolerates a bare object or a
/// ```json fence; drops blank entries, dedupes case-insensitively and keeps at
/// most [`META_MAX_TAGS`]. Returns `None` when nothing usable came back (the
/// caller keeps the optimistic title).
#[must_use]
pub(crate) fn parse_meta(raw: &str) -> Option<(Option<String>, Vec<String>)> {
    let trimmed = raw.trim();
    let fence = String::from("```");
    let body = trimmed
        .strip_prefix(&format!("{fence}json"))
        .or_else(|| trimmed.strip_prefix(&fence))
        .map(str::trim)
        .and_then(|s| s.strip_suffix(&fence))
        .map(str::trim)
        .unwrap_or(trimmed);
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(&body[start..=end]).ok()?;
    let title = value
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| clean_label(s, META_TITLE_MAX_CHARS))
        .filter(|s| !s.is_empty());
    let mut tags: Vec<String> = Vec::new();
    if let Some(arr) = value.get("tags").and_then(|v| v.as_array()) {
        for item in arr {
            let Some(tag) = item.as_str() else {
                continue;
            };
            let tag = clean_label(tag, META_TAG_MAX_CHARS);
            if tag.is_empty() || tags.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
                continue;
            }
            tags.push(tag);
            if tags.len() == META_MAX_TAGS {
                break;
            }
        }
    }
    if title.is_none() && tags.is_empty() {
        return None;
    }
    Some((title, tags))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_json() {
        let (title, tags) =
            parse_meta(r#"  {"title":"Rust async question","tags":["rust","tokio"]}  "#).unwrap();
        assert_eq!(title.as_deref(), Some("Rust async question"));
        assert_eq!(tags, vec!["rust".to_string(), "tokio".to_string()]);
    }

    #[test]
    fn parses_fenced_json() {
        let fence = String::from("```");
        let raw = format!("Sure!\n{fence}json\n{{\"title\":\"T\",\"tags\":[]}}\n{fence}");
        let (title, tags) = parse_meta(&raw).unwrap();
        assert_eq!(title.as_deref(), Some("T"));
        assert!(tags.is_empty());
    }

    #[test]
    fn caps_dedupes_and_skips_non_strings() {
        let raw = r#"{"title":"","tags":["a","A","b", 7, "c", "d"]}"#;
        let (_, tags) = parse_meta(raw).unwrap();
        assert_eq!(
            tags,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn garbage_yields_none() {
        assert!(parse_meta("no json here").is_none());
        assert!(parse_meta("{\"title\":\"\",\"tags\":[]}").is_none());
    }

    #[test]
    fn request_constrains_output_and_reasoning() {
        let req = metadata_request("test-model", "user: hello");
        assert_eq!(req.model, "test-model");
        assert_eq!(req.max_tokens, Some(META_MAX_TOKENS));
        assert_eq!(req.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            req.extra["text"]["format"]["schema"]["properties"]["title"]["maxLength"],
            META_TITLE_MAX_CHARS
        );
    }

    #[test]
    fn local_fallback_cleans_the_opening_request() {
        assert_eq!(
            fallback_title_from_content("  please   fix the auto chat titling!\nthanks  ")
                .as_deref(),
            Some("Fix the auto chat titling! thanks")
        );
        assert_eq!(fallback_title_from_content(" \n\t "), None);
        assert!(
            fallback_title_from_content(&"x".repeat(100))
                .unwrap()
                .chars()
                .count()
                <= META_TITLE_MAX_CHARS
        );
    }

    #[test]
    fn parser_normalizes_and_caps_labels() {
        let long = "x".repeat(100);
        let raw = format!(r#"{{"title":"  two\n lines  ","tags":["  rust\n async  ","{long}"]}}"#);
        let (title, tags) = parse_meta(&raw).unwrap();
        assert_eq!(title.as_deref(), Some("two lines"));
        assert_eq!(tags[0], "rust async");
        assert_eq!(tags[1].chars().count(), META_TAG_MAX_CHARS);
    }
}
