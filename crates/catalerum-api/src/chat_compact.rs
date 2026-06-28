//! Persistent chat-thread auto-compaction (SOUL §7/§12).
//!
//! The in-run compactor (`catalerum_llm::compact`) keeps a *single* agent run
//! under the model's context window — but a chat thread re-seeds from Postgres
//! every turn, so an in-run fold is forgotten the moment the turn ends. This
//! module is the durable layer: when a turn finishes with its context near the
//! window, a background task folds the older transcript into a **rolling
//! summary stored on the conversation** (`conversations.summary` +
//! `summary_upto`, migration 0054). The next turn then seeds
//! `[summary] + messages after summary_upto` instead of the whole history.
//!
//! Messages are never deleted — the conversation view still shows everything;
//! only what the model re-reads shrinks. A regenerate that prunes the covered
//! anchor row invalidates the summary via the FK's `ON DELETE SET NULL` (a
//! half-set pair is ignored), and the next oversized turn simply rebuilds it.
//!
//! Everything here is best-effort and off the hot path: the trigger check is
//! cheap, the summarize call runs in a spawned task, and any failure just
//! leaves the thread on the coarse `CHAT_HISTORY_LIMIT` bound it had before.

use tracing::warn;

use catalerum_core::model::{Message, MessageRole};
use catalerum_core::{ConversationId, WorkspaceId};
use catalerum_llm::compact::{cap_transcript, render_transcript, summarize_request};
use catalerum_llm::{CompactionConfig, DEFAULT_CONTEXT_WINDOW};

use crate::chat_context::{to_chat_messages, CHAT_HISTORY_LIMIT};
use crate::state::AppState;

/// How many trailing messages a fold keeps out of the summary (aligned forward
/// to a user-turn boundary). Big enough that the model keeps the recent
/// exchanges verbatim; small enough that a fold meaningfully shrinks the seed.
const KEEP_TAIL_MESSAGES: usize = 16;

/// Whether a finished exchange left the thread's context close enough to the
/// window to warrant a background fold. `context_tokens` is the loop's
/// provider-reported final-turn size (the grounded signal); `estimated` the
/// chars/4 estimate over the same transcript (covers providers that report no
/// usage). Pure, for tests.
#[must_use]
fn should_compact_thread(
    context_tokens: Option<u32>,
    estimated: u32,
    window: u32,
    trigger_ratio: f64,
) -> bool {
    let projected = estimated.max(context_tokens.unwrap_or(0));
    u64::from(projected) > (f64::from(window) * trigger_ratio) as u64
}

/// Where the kept tail starts: the last [`KEEP_TAIL_MESSAGES`] messages,
/// advanced forward to the next **user** message so the post-fold seed opens on
/// a clean turn boundary (mirroring `trim_to_turn_boundary`). `None` when there
/// is nothing worth folding — the head must hold at least one full exchange
/// (2 messages) and the tail must keep the newest exchange.
#[must_use]
fn fold_boundary(messages: &[Message], keep_tail: usize) -> Option<usize> {
    let mut boundary = messages.len().checked_sub(keep_tail)?;
    while boundary < messages.len() && messages[boundary].role != MessageRole::User {
        boundary += 1;
    }
    if boundary < 2 || boundary >= messages.len() {
        return None;
    }
    Some(boundary)
}

/// Post-turn hook (called from the ws chat handler): check the trigger and, if
/// the thread needs it, spawn the background fold. `context_tokens` /
/// `estimated_tokens` describe the exchange that just finished. Never fails the
/// turn; the spawned task logs its own failures.
pub(crate) async fn maybe_compact_thread(
    state: &AppState,
    workspace_id: WorkspaceId,
    conversation_id: ConversationId,
    model: String,
    context_tokens: Option<u32>,
    estimated_tokens: u32,
) {
    let policy = CompactionConfig::default();
    let window = state
        .model_context_window(&model)
        .await
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    if !should_compact_thread(
        context_tokens,
        estimated_tokens,
        window,
        policy.trigger_ratio,
    ) {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = compact_thread(&state, workspace_id, conversation_id, &model, window).await
        {
            warn!(%conversation_id, error = %e, "chat thread compaction failed");
        }
    });
}

/// Fold the compactable prefix of a conversation into its rolling summary: read
/// the same bounded window the next seed would replay, keep the newest
/// [`KEEP_TAIL_MESSAGES`] (user-boundary aligned), summarize the rest together
/// with any existing summary via one tool-less model call, and persist
/// `summary` + `summary_upto` on the conversation.
async fn compact_thread(
    state: &AppState,
    workspace_id: WorkspaceId,
    conversation_id: ConversationId,
    model: &str,
    window: u32,
) -> Result<(), String> {
    // Re-read fresh: the conversation may have been deleted, regenerated (anchor
    // nulled), or already compacted by a racing turn since the trigger fired.
    let conversation = state
        .store()
        .conversations()
        .get(workspace_id, conversation_id)
        .await
        .map_err(|e| format!("loading conversation: {e}"))?;
    let prior = match (conversation.summary.as_deref(), conversation.summary_upto) {
        (Some(summary), Some(upto)) => Some((summary, upto)),
        _ => None,
    };
    let messages = match prior {
        Some((_, upto)) => {
            state
                .store()
                .messages()
                .list_recent_after(conversation_id, upto, CHAT_HISTORY_LIMIT)
                .await
        }
        None => {
            state
                .store()
                .messages()
                .list_recent(conversation_id, CHAT_HISTORY_LIMIT)
                .await
        }
    }
    .map_err(|e| format!("loading messages: {e}"))?;

    let Some(boundary) = fold_boundary(&messages, KEEP_TAIL_MESSAGES) else {
        return Ok(()); // Nothing worth folding — the tail IS the thread.
    };
    let head = &messages[..boundary];
    let anchor = head.last().expect("non-empty head").id;

    // Render the head (attachments references included — they carry the store
    // keys a later turn may still need) and bound the summarize prompt itself
    // to the window it must fit in (chars ≈ tokens × 4, half the budget).
    let policy = CompactionConfig {
        context_window: Some(window),
        ..CompactionConfig::default()
    };
    let max_chars = (policy.budget_tokens() as usize / 2).saturating_mul(4);
    let transcript = cap_transcript(render_transcript(&to_chat_messages(head)), max_chars);
    let request = summarize_request(model, prior.map(|(s, _)| s), transcript);
    let turn = state
        .llm()
        .chat(request)
        .await
        .map_err(|e| format!("summarize call: {e}"))?;
    let summary = turn.content.trim().to_string();
    if summary.is_empty() {
        return Err("summarize call returned an empty summary".to_string());
    }

    state
        .store()
        .conversations()
        .set_summary(workspace_id, conversation_id, Some((&summary, anchor)))
        .await
        .map_err(|e| format!("persisting summary: {e}"))?;
    tracing::info!(
        %conversation_id,
        folded = head.len(),
        "chat thread compacted into rolling summary"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{MessageId, WorkspaceId};

    fn msg(role: MessageRole) -> Message {
        Message {
            id: MessageId::new(),
            conversation_id: ConversationId::new(),
            role,
            content: String::new(),
            attachments: Vec::new(),
            skill: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_is_error: false,
            tool_duration_ms: None,
            usage: None,
            created_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        }
    }

    #[test]
    fn trigger_uses_max_of_reported_and_estimated() {
        // Window 1000 @ 0.8 → threshold 800.
        assert!(!should_compact_thread(Some(500), 400, 1_000, 0.8));
        assert!(should_compact_thread(Some(900), 400, 1_000, 0.8));
        assert!(should_compact_thread(None, 900, 1_000, 0.8));
        assert!(!should_compact_thread(None, 800, 1_000, 0.8)); // exactly at, not over
        let _ = WorkspaceId::new(); // keep the import honest under cfg(test)
    }

    #[test]
    fn fold_boundary_aligns_to_a_user_turn_and_keeps_a_real_head() {
        use MessageRole::{Assistant, Tool, User};
        let history: Vec<Message> = [
            User, Assistant, // exchange 1 (foldable)
            User, Assistant, Tool, Assistant, // exchange 2 (foldable)
            User, Assistant, // exchange 3
            User, Assistant, // exchange 4 (newest)
        ]
        .into_iter()
        .map(msg)
        .collect();

        // keep 4 → naive boundary 6 is already a User turn.
        assert_eq!(fold_boundary(&history, 4), Some(6));
        // keep 5 → naive boundary 5 is mid-exchange (assistant); advances to 6.
        assert_eq!(fold_boundary(&history, 5), Some(6));
        // keep 9 → naive boundary 1 is mid-exchange; advances to the next user
        // turn at 2, folding just the first exchange (a minimal but real fold —
        // the tail may come out slightly smaller than requested, never larger).
        assert_eq!(fold_boundary(&history, 9), Some(2));
        // Keeping everything (and more) → nothing worth folding.
        assert_eq!(fold_boundary(&history, 20), None);
        // A tail that would swallow the whole thread (no user boundary after
        // the naive point) → None rather than an empty tail.
        let tail_less: Vec<Message> = [User, Assistant, Assistant, Assistant]
            .into_iter()
            .map(msg)
            .collect();
        assert_eq!(fold_boundary(&tail_less, 2), None);
        // Tiny threads never fold.
        assert_eq!(fold_boundary(&[msg(User)], 0), None);
        assert_eq!(fold_boundary(&[], 4), None);
    }
}
