//! Background memory auto-curation (SOUL §22): after a conversation, an LLM
//! proposes durable facts worth remembering about the user, and the new ones are
//! stored as memories (and embedded for recall).
//!
//! This is the producer side of "the assistant learns" — memories no longer come
//! only from the explicit `remember` tool. [`extract_memories`] is the unit of
//! work: load the recent exchange, ask the model for a JSON list of facts, and
//! store each new one **through the shared dedup seam**
//! ([`store_memory_deduped`](crate::dedup::store_memory_deduped), SOUL §29) so a
//! fact already known is not re-stored — the same seam the `remember` tool and the
//! `POST /memories` route use. Each genuinely new memory is enqueued for embedding
//! so auto-recall can surface it (§6.4).
//!
//! Dedup is heuristic (exact / whole-word-superset) plus, when this worker also
//! holds an [`EmbedContext`](crate::EmbedContext), embedding-similarity — the seam
//! decides. Facts that merely *extend* an existing memory refine it in place.
//!
//! # Job contract
//! [`enqueue_extract_memories`] writes a durable [`JOB_KIND_EXTRACT_MEMORIES`]
//! job ([`ExtractMemoriesPayload`]); a worker holding a [`CurateContext`] runs it.

use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use catalerum_core::error::Error as CoreError;
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{MemoryScope, MessageRole};
use catalerum_core::provider::LlmClient;
use catalerum_core::stream::StreamEvent;
use catalerum_core::{ConversationId, UserId, WorkspaceId};
use catalerum_store::Store;

use crate::dedup::{store_memory_deduped, MemoryDedupIndex, MemoryStoreStatus};
use crate::embed::EmbedContext;
use crate::error::{IngestError, Result};

/// The `job_queue.kind` token for a memory-extraction job (SOUL §22).
pub const JOB_KIND_EXTRACT_MEMORIES: &str = "extract_memories";

/// How many of a conversation's most recent **user/assistant** turns to feed the
/// extractor — bounds the prompt and focuses on the latest exchange.
const RECENT_MESSAGES: usize = 8;
/// How many of a conversation's most recent messages to **fetch** from the store
/// before [`recent_transcript`] distills the last [`RECENT_MESSAGES`] user/assistant
/// turns (SOUL §18 — bounded read, not the whole — possibly thousands-message —
/// conversation). Generous so interleaved system/tool turns can't starve the 8
/// user/assistant turns out of the window.
const TRANSCRIPT_SCAN: i64 = 64;
/// Cap on candidates accepted from one extraction (a runaway-output backstop).
const MAX_CANDIDATES: usize = 10;

/// The extraction instruction. The model must answer with **only** a JSON array
/// of short fact strings (or `[]`), so the output parses deterministically.
const EXTRACT_SYSTEM: &str = "\
You extract durable facts worth remembering about the user from a conversation — \
stable preferences, personal details, relationships, goals, recurring habits. \
Ignore one-off task content, questions, and anything ephemeral. Respond with \
ONLY a compact JSON array of short third-person fact strings, e.g. \
[\"prefers tea\", \"works in Berlin\"]. If there is nothing worth remembering, \
respond with exactly []. No prose, no markdown.";

/// What one [`extract_memories`] run produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractReport {
    /// How many *new* memories were created (post-dedup).
    pub created: usize,
    /// How many candidates the model proposed (pre-dedup).
    pub proposed: usize,
}

/// Extract and store new memories from a conversation (SOUL §22). Idempotent
/// against duplicates: each proposed fact goes through the shared dedup seam
/// ([`store_memory_deduped`]), so a fact already stored is skipped (and one that
/// merely extends a known fact refines it), and re-running on the same exchange
/// creates nothing new. `embed`, when present, enables the seam's
/// embedding-similarity layer (and embed-on-store); pass `None` for heuristic-only.
pub async fn extract_memories<L: LlmClient + ?Sized>(
    store: &Store,
    llm: &L,
    model: &str,
    embed: Option<&EmbedContext>,
    workspace_id: WorkspaceId,
    conversation_id: ConversationId,
    user_id: UserId,
) -> Result<ExtractReport> {
    // The recent exchange as plain text (user/assistant turns with content). Fetch
    // only the most-recent `TRANSCRIPT_SCAN` messages (§18 — never the whole, maybe
    // huge, conversation), from which `recent_transcript` keeps the last few turns.
    let history = store
        .messages()
        .list_recent(conversation_id, TRANSCRIPT_SCAN)
        .await?;
    let transcript = recent_transcript(&history);
    if transcript.is_empty() {
        return Ok(ExtractReport {
            created: 0,
            proposed: 0,
        });
    }

    let request = ChatRequest::new(
        model,
        vec![
            ChatMessage::system(EXTRACT_SYSTEM),
            ChatMessage::user(transcript),
        ],
    );
    let raw = complete_text(llm, request).await?;
    let candidates = parse_memory_list(&raw);
    let proposed = candidates.len();
    if candidates.is_empty() {
        return Ok(ExtractReport {
            created: 0,
            proposed,
        });
    }

    // Store each candidate through the shared dedup seam (SOUL §29): the heuristic
    // exact/superset layer plus, when `embed` is present, embedding similarity.
    // The seam re-reads the workspace's memories per candidate, so within-batch
    // duplicates (and duplicates of a fact just stored this run) are caught too.
    let index = embed.map(|e| MemoryDedupIndex {
        embedder: &*e.embedder,
        vector: &e.vector,
        embed_model: e.config.embed_model.as_str(),
    });
    let mut created = 0;
    for candidate in candidates {
        let text = candidate.trim();
        if text.is_empty() {
            continue;
        }
        let outcome = store_memory_deduped(
            store,
            index.as_ref(),
            workspace_id,
            MemoryScope::User,
            Some(user_id),
            text,
            None,
        )
        .await?;
        if outcome.status == MemoryStoreStatus::Stored {
            created += 1;
        }
    }
    debug!(%conversation_id, %user_id, proposed, created, "extracted memories");
    Ok(ExtractReport { created, proposed })
}

/// The recent user/assistant exchange as a plain `Role: content` transcript
/// (skipping system/tool turns and empty content), capped to the last few
/// messages. Empty when there is nothing to extract from.
fn recent_transcript(history: &[catalerum_core::Message]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for m in history.iter().rev() {
        if lines.len() >= RECENT_MESSAGES {
            break;
        }
        let role = match m.role {
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::System | MessageRole::Tool => continue,
        };
        let content = m.content.trim();
        if content.is_empty() {
            continue;
        }
        lines.push(format!("{role}: {content}"));
    }
    lines.reverse();
    lines.join("\n")
}

/// Drive a (non-streaming) completion by draining the chat stream and
/// concatenating its text deltas. A mid-stream `Error` event fails the call.
async fn complete_text<L: LlmClient + ?Sized>(llm: &L, request: ChatRequest) -> Result<String> {
    let mut stream = llm.chat_stream(request).await?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event? {
            StreamEvent::TextDelta { text: t } => text.push_str(&t),
            StreamEvent::Error { message } => {
                return Err(IngestError::from(CoreError::provider(message)))
            }
            StreamEvent::Done { .. } => break,
            // Curation collects only the answer text; reasoning, tool-call deltas,
            // and the agent loop's tool-lifecycle/compaction events are not part
            // of the result.
            StreamEvent::ReasoningDelta { .. }
            | StreamEvent::ToolCallDelta { .. }
            | StreamEvent::ToolCallStarted { .. }
            | StreamEvent::ToolResult { .. }
            | StreamEvent::Compacted { .. } => {}
        }
    }
    Ok(text)
}

/// Parse the model's reply into a list of fact strings. Expects a bare JSON
/// array; tolerates surrounding prose by extracting the first `[` … last `]`
/// span. Returns at most [`MAX_CANDIDATES`]; anything unparseable → empty.
fn parse_memory_list(raw: &str) -> Vec<String> {
    let try_parse = |s: &str| serde_json::from_str::<Vec<String>>(s.trim()).ok();
    let parsed = try_parse(raw).or_else(|| {
        let start = raw.find('[')?;
        let end = raw.rfind(']')?;
        if end > start {
            try_parse(&raw[start..=end])
        } else {
            None
        }
    });
    parsed
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(MAX_CANDIDATES)
        .collect()
}

/// The services a worker needs to run an [`JOB_KIND_EXTRACT_MEMORIES`] job: an
/// [`LlmClient`] and the extraction model. Bundled like [`crate::EmbedContext`].
#[derive(Clone)]
pub struct CurateContext {
    /// The chat client used to extract facts.
    pub llm: Arc<dyn LlmClient>,
    /// The model to extract with (an `[llm]`/`[curation]` config field).
    pub model: String,
}

impl CurateContext {
    /// Bundle the services for memory extraction.
    #[must_use]
    pub fn new(llm: Arc<dyn LlmClient>, model: impl Into<String>) -> Self {
        Self {
            llm,
            model: model.into(),
        }
    }

    /// Run [`extract_memories`] for a conversation using these services. `embed`,
    /// when supplied by the worker, enables the dedup seam's embedding-similarity
    /// layer (and embed-on-store).
    pub async fn extract(
        &self,
        store: &Store,
        embed: Option<&EmbedContext>,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        user_id: UserId,
    ) -> Result<ExtractReport> {
        extract_memories(
            store,
            &*self.llm,
            &self.model,
            embed,
            workspace_id,
            conversation_id,
            user_id,
        )
        .await
    }
}

impl std::fmt::Debug for CurateContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CurateContext")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// The JSON payload of a [`JOB_KIND_EXTRACT_MEMORIES`] job: which conversation to
/// mine and for which user (the workspace defers to the job row when absent).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractMemoriesPayload {
    /// The workspace that owns the conversation. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The conversation to extract from.
    pub conversation_id: ConversationId,
    /// The user the extracted memories belong to.
    pub user_id: UserId,
}

impl ExtractMemoriesPayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        user_id: UserId,
    ) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            conversation_id,
            user_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_EXTRACT_MEMORIES`] job (SOUL §22).
pub async fn enqueue_extract_memories(
    store: &Store,
    workspace_id: WorkspaceId,
    conversation_id: ConversationId,
    user_id: UserId,
) -> Result<Uuid> {
    let payload = ExtractMemoriesPayload::new(workspace_id, conversation_id, user_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_EXTRACT_MEMORIES,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %conversation_id, "enqueued extract_memories job");
    Ok(job.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_memory_list_handles_bare_array_prose_wrapped_and_garbage() {
        assert_eq!(
            parse_memory_list(r#"["prefers tea", "works in Berlin"]"#),
            vec!["prefers tea".to_string(), "works in Berlin".to_string()]
        );
        // Prose-wrapped → extract the bracket span.
        assert_eq!(
            parse_memory_list("Sure! Here you go:\n[\"likes cats\"]\nHope that helps."),
            vec!["likes cats".to_string()]
        );
        // Empty array / garbage / no array → nothing.
        assert!(parse_memory_list("[]").is_empty());
        assert!(parse_memory_list("nothing to remember").is_empty());
        assert!(parse_memory_list("").is_empty());
        // Blank entries dropped.
        assert_eq!(parse_memory_list(r#"["  ", "ok"]"#), vec!["ok".to_string()]);
    }

    #[test]
    fn parse_memory_list_caps_candidates() {
        let many: Vec<String> = (0..50).map(|i| format!("fact {i}")).collect();
        let json = serde_json::to_string(&many).unwrap();
        assert_eq!(parse_memory_list(&json).len(), MAX_CANDIDATES);
    }

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_EXTRACT_MEMORIES, "extract_memories");
    }

    fn msg(role: MessageRole, content: &str) -> catalerum_core::Message {
        catalerum_core::Message {
            id: catalerum_core::MessageId::new(),
            conversation_id: catalerum_core::ConversationId::new(),
            role,
            content: content.to_string(),
            attachments: Vec::new(),
            skill: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            tool_is_error: false,
            tool_duration_ms: None,
            usage: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn recent_transcript_keeps_last_user_assistant_turns_and_skips_noise() {
        // System/Tool turns and empty content are skipped; the kept user/assistant
        // turns come out in chronological order as "Role: content".
        let history = vec![
            msg(MessageRole::System, "you are an assistant"),
            msg(MessageRole::User, "hi"),
            msg(MessageRole::Assistant, "hello"),
            msg(MessageRole::Tool, "{tool output}"),
            msg(MessageRole::User, "   "), // empty-after-trim → skipped
            msg(MessageRole::User, "weather?"),
            msg(MessageRole::Assistant, "sunny"),
        ];
        assert_eq!(
            recent_transcript(&history),
            "User: hi\nAssistant: hello\nUser: weather?\nAssistant: sunny"
        );

        // More than RECENT_MESSAGES user/assistant turns → only the last 8 are kept
        // (validating that the TRANSCRIPT_SCAN fetch window need only be a small
        // multiple of RECENT_MESSAGES).
        let mut many = Vec::new();
        for i in 0..12 {
            many.push(msg(MessageRole::User, &format!("u{i}")));
            many.push(msg(MessageRole::Assistant, &format!("a{i}")));
        }
        let lines: Vec<String> = recent_transcript(&many)
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), RECENT_MESSAGES, "capped to the last 8 turns");
        assert_eq!(lines[0], "User: u8");
        assert_eq!(lines.last().unwrap(), "Assistant: a11");
    }
}
