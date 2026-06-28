//! Non-transport chat context construction.
//!
//! WebSocket framing and streaming live in `routes::ws`; this module owns the
//! model-facing replay, skill, attachment, and multimodal context helpers shared
//! by that transport and persistent chat compaction.

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::llm::ChatMessage;
use catalerum_core::model::{Attachment, Message, MessageRole, SkillInvocation, ToolCall};
use catalerum_core::{UserId, WorkspaceId};

use crate::error::ApiError;
use crate::state::AppState;

/// Maximum number of recent persisted messages replayed before token-aware
/// compaction takes over.
pub(crate) const CHAT_HISTORY_LIMIT: i64 = 100;

/// Trim a bounded history window to begin at its first user message, so it
/// cannot open with an orphaned tool result.
pub(crate) fn trim_to_turn_boundary(history: &[Message]) -> &[Message] {
    let start = history
        .iter()
        .position(|message| message.role == MessageRole::User)
        .unwrap_or(0);
    &history[start..]
}

/// Synthesize error results for replayed tool calls whose result row was never
/// persisted, keeping the model history structurally valid after interruption.
pub(crate) fn patch_dangling_tool_calls(messages: &mut Vec<ChatMessage>) {
    let answered: std::collections::HashSet<String> = messages
        .iter()
        .filter_map(|message| message.tool_call_id.clone())
        .collect();
    let mut index = 0;
    while index < messages.len() {
        let missing: Vec<ToolCall> = if messages[index].role == MessageRole::Assistant {
            messages[index]
                .tool_calls
                .iter()
                .filter(|call| !answered.contains(&call.id))
                .map(|call| (*call).clone())
                .collect()
        } else {
            Vec::new()
        };
        for (offset, call) in missing.iter().enumerate() {
            messages.insert(
                index + 1 + offset,
                ChatMessage {
                    role: MessageRole::Tool,
                    content: r#"{"error":"interrupted — this tool call never completed"}"#
                        .to_string(),
                    images: Vec::new(),
                    media: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: Some(call.id.clone()),
                    name: Some(call.name.clone()),
                    reasoning: None,
                    reasoning_details: Vec::new(),
                },
            );
        }
        index += missing.len() + 1;
    }
}

/// Convert stored messages into model-facing chat messages in replay order.
pub(crate) fn to_chat_messages(history: &[Message]) -> Vec<ChatMessage> {
    history
        .iter()
        .map(|message| ChatMessage {
            role: message.role,
            content: if message.role == MessageRole::User {
                user_seed_content(
                    &message.content,
                    message.skill.as_ref(),
                    &message.attachments,
                )
            } else {
                message.content.clone()
            },
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: message.tool_calls.clone(),
            tool_call_id: message.tool_call_id.clone(),
            name: None,
            reasoning: None,
            reasoning_details: Vec::new(),
        })
        .collect()
}

/// Build the content a live or replayed user turn contributes to model context.
pub(crate) fn user_seed_content(
    content: &str,
    skill: Option<&SkillInvocation>,
    attachments: &[Attachment],
) -> String {
    let mut output = content.to_string();
    if let Some(skill) = skill {
        output.push_str(&render_skill_block(skill));
    }
    if !attachments.is_empty() {
        output.push_str(&render_attachments_block(attachments));
    }
    output
}

fn render_skill_block(skill: &SkillInvocation) -> String {
    let tools = if skill.tools.is_empty() {
        String::new()
    } else {
        format!(
            " It is meant to use these tools: {}.",
            skill.tools.join(", ")
        )
    };
    format!(
        "\n\n[The user invoked the skill \"{}\" — follow its runbook below for their \
         request.{tools}]\n\n{}",
        skill.name, skill.instructions
    )
}

/// Resolve and authorize a composer `/<skill>` invocation into the immutable
/// snapshot stored on its user message.
pub(crate) async fn resolve_skill_invocation(
    state: &AppState,
    workspace_id: WorkspaceId,
    name: &str,
    capabilities: &[Capability],
) -> Result<SkillInvocation, ApiError> {
    let skill = state
        .store()
        .skills()
        .get_by_name(workspace_id, name)
        .await?
        .ok_or_else(|| ApiError::bad_request(format!("unknown skill `{name}`")))?;
    let required = Capability::new(Action::Use, Resource::new("skill", &skill.name));
    if !capabilities
        .iter()
        .any(|capability| capability.covers(&required))
    {
        return Err(ApiError::Forbidden(format!(
            "your grant does not permit skill:use@{}",
            skill.name
        )));
    }
    Ok(SkillInvocation {
        name: skill.name,
        instructions: skill.instructions_md,
        tools: skill.tools,
    })
}

fn render_attachments_block(attachments: &[Attachment]) -> String {
    let mut block = String::from(
        "\n\n[Attached files — uploaded to the files store, NOT inlined here. To work on \
         one, `stage_object` it into a terminal session, or `copy_object` it between \
         stores; use `read_object` for already-extracted text. Each line gives the \
         store + key to pass:]",
    );
    for attachment in attachments {
        block.push_str("\n- ");
        block.push_str(&attachment_reference(attachment));
    }
    block
}

fn attachment_reference(attachment: &Attachment) -> String {
    let name = attachment.filename.as_deref().unwrap_or("(unnamed)");
    let mut metadata = Vec::new();
    if let Some(content_type) = attachment
        .content_type
        .as_deref()
        .filter(|content_type| !content_type.is_empty())
    {
        metadata.push(content_type.to_string());
    }
    if let Some(size) = attachment.size {
        metadata.push(format!("{size} bytes"));
    }
    let metadata = if metadata.is_empty() {
        String::new()
    } else {
        format!(" ({})", metadata.join(", "))
    };
    let location = match attachment.url.strip_prefix("/storage/objects/") {
        Some(rest) => {
            let (key, store) = match rest.split_once("?store=") {
                Some((key, store)) if !store.is_empty() => (key, Some(store)),
                _ => (rest.split_once('?').map_or(rest, |(key, _)| key), None),
            };
            match store {
                Some(store) => format!("store `{store}`, key `{key}`"),
                None => format!("default files store, key `{key}`"),
            }
        }
        None => format!("url `{}`", attachment.url),
    };
    format!("{name}{metadata} — {location}")
}

fn attachment_is_image(attachment: &Attachment) -> bool {
    attachment
        .content_type
        .as_deref()
        .is_some_and(|content_type| content_type.starts_with("image/"))
}

/// Inline image attachments into the corresponding model messages. Call while
/// `history` and `seed` are still one-to-one.
pub(crate) async fn inline_image_attachments(
    state: &AppState,
    workspace_id: WorkspaceId,
    user_id: UserId,
    history: &[Message],
    seed: &mut [ChatMessage],
) {
    for (message, chat_message) in history.iter().zip(seed.iter_mut()) {
        if message.role != MessageRole::User {
            continue;
        }
        for attachment in message
            .attachments
            .iter()
            .filter(|attachment| attachment_is_image(attachment))
        {
            if let Some(uri) = image_data_uri(state, workspace_id, user_id, attachment).await {
                chat_message.images.push(uri);
            }
        }
    }
}

async fn image_data_uri(
    state: &AppState,
    workspace_id: WorkspaceId,
    user_id: UserId,
    attachment: &Attachment,
) -> Option<String> {
    use base64::Engine as _;

    let rest = match attachment.url.strip_prefix("/storage/objects/") {
        Some(rest) => rest,
        None => {
            let url = attachment.url.as_str();
            return (url.starts_with("http://") || url.starts_with("https://"))
                .then(|| url.to_string());
        }
    };
    let (store, key) = match rest.split_once("?store=") {
        Some((key, store)) if !store.is_empty() => (Some(store), key),
        _ => (None, rest.split_once('?').map_or(rest, |(key, _)| key)),
    };
    let (bytes, stored_content_type) = crate::routes::storage::read_object_bytes(
        state.storage(),
        state.store(),
        workspace_id,
        Some(user_id),
        (store, key),
    )
    .await
    .ok()?;
    let mime = attachment
        .content_type
        .as_deref()
        .filter(|content_type| content_type.starts_with("image/"))
        .or(stored_content_type.as_deref())?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{mime};base64,{encoded}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{ConversationId, MessageId};

    fn message(role: MessageRole) -> Message {
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
    fn skill_and_attachment_context_rides_only_user_seed() {
        let mut user = message(MessageRole::User);
        user.content = "/summarize the notes".to_string();
        user.skill = Some(SkillInvocation {
            name: "summarize".to_string(),
            instructions: "Read, then summarize.".to_string(),
            tools: vec!["search_notes".to_string()],
        });
        user.attachments = vec![Attachment {
            url: "/storage/objects/chat/report.pdf".to_string(),
            filename: Some("report.pdf".to_string()),
            content_type: Some("application/pdf".to_string()),
            size: Some(2048),
        }];
        let seed = to_chat_messages(std::slice::from_ref(&user));
        assert!(seed[0].content.contains("invoked the skill \"summarize\""));
        assert!(seed[0].content.contains("search_notes"));
        assert!(seed[0].content.contains("Attached files"));
        assert!(seed[0]
            .content
            .contains("default files store, key `chat/report.pdf`"));
        assert_eq!(user.content, "/summarize the notes");

        let mut assistant = message(MessageRole::Assistant);
        assistant.content = "ok".to_string();
        assistant.skill = user.skill;
        assistant.attachments = user.attachments;
        assert_eq!(to_chat_messages(&[assistant])[0].content, "ok");
    }

    #[test]
    fn history_starts_at_a_user_turn() {
        use MessageRole::{Assistant, Tool, User};
        let history = [
            message(Tool),
            message(Assistant),
            message(User),
            message(Assistant),
        ];
        let trimmed = trim_to_turn_boundary(&history);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].role, User);
        assert_eq!(trim_to_turn_boundary(&history[..2]).len(), 2);
        assert!(trim_to_turn_boundary(&[]).is_empty());
    }

    #[test]
    fn dangling_tool_calls_receive_an_interrupted_result() {
        let mut history = vec![
            ChatMessage::user("run it"),
            ChatMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                images: Vec::new(),
                media: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".to_string(),
                    name: "slow".to_string(),
                    arguments: "{}".to_string(),
                }],
                tool_call_id: None,
                name: None,
                reasoning: None,
                reasoning_details: Vec::new(),
            },
        ];
        patch_dangling_tool_calls(&mut history);
        assert_eq!(history.len(), 3);
        assert_eq!(history[2].role, MessageRole::Tool);
        assert_eq!(history[2].tool_call_id.as_deref(), Some("call-1"));
        assert!(history[2].content.contains("interrupted"));
    }
}
