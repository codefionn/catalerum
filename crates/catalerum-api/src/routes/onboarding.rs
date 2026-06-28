//! Quick-start / onboarding orchestration (SOUL §12/§22/§23).
//!
//! The workbench ships a first-run wizard that walks a new user through the
//! handful of setup steps that personalize the assistant: confirm the backing
//! services are reachable, pick a chat/speech model, record a few profile facts,
//! and — through a short **personalization chat** — turn what the assistant learns
//! about the user into durable [`Memory`](catalerum_core::model::Memory) facts and
//! tailored [`Skill`](catalerum_core::model::Skill) runbooks.
//!
//! Almost every step reuses an existing endpoint — `GET /status` (health),
//! `GET`/`PUT /llm-settings` (models), `PUT /profile` + `POST /memories`
//! (personalization), `PUT /skills/{name}` (authoring). This module adds only the
//! three things the wizard cannot assemble from those alone:
//!
//! - `GET  /onboarding/state`       — has the caller finished the quick-start,
//!   have they chosen a model, is their profile still empty (drives the first-run
//!   auto-open). Gated `profile:read`.
//! - `POST /onboarding/personalize` — one turn of an assistant-led personalization
//!   chat: given the conversation so far, the model replies with its next question
//!   and *proposes* the durable memories and skill drafts it has learned. It **does
//!   not persist** anything; the client reviews the proposals and writes the chosen
//!   ones via `POST /memories` and `PUT /skills/{name}` (every captured artifact
//!   stays a viewable, editable row — SOUL §16). Gated `skill:write` (it is the
//!   authoring path and spends gateway tokens).
//! - `POST /onboarding/complete`    — stamp a [`COMPLETED_KEY`] sentinel into
//!   the profile so the wizard does not auto-open again. Gated `profile:write`.
//!
//! Reuses the existing `profile` / `skill` capability domains — no new `Action`
//! variant or domain string is introduced (SOUL §19).

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::Map;

use crate::auth::Auth;
use crate::error::ApiResult;
use crate::state::AppState;

/// Profile-field key under which [`complete`] stamps the quick-start completion
/// time (RFC 3339). Its presence is the "onboarding done, don't auto-open"
/// signal; it is excluded from the `profile_empty` heuristic so the sentinel
/// itself never counts as user-entered personalization.
pub const COMPLETED_KEY: &str = "quickstart_completed_at";

/// Cap on how many skill drafts one `personalize` turn is allowed to propose, so a
/// runaway reply can't have the client author an unbounded fan-out of skills.
const MAX_SKILLS_PER_TURN: usize = 3;
/// Cap on how many memory facts one `personalize` turn is allowed to propose (a
/// runaway-output backstop, mirroring the §22 extractor's `MAX_CANDIDATES`).
const MAX_MEMORIES_PER_TURN: usize = 6;
/// Cap on how many prior turns the client may replay into one `personalize` call,
/// bounding the prompt (SOUL §18 — a bounded read, never an unbounded transcript).
const MAX_HISTORY_TURNS: usize = 40;

/// Advisory tool allow-list offered to the model when it drafts a skill's
/// `tools`. These are real registered tool names; the field is advisory today
/// (SOUL §23 — restricted-tool enforcement is deferred), so an off-list value is
/// harmless, but grounding the prompt keeps drafts realistic.
const TOOL_ALLOWLIST: &[&str] = &[
    "fetch_url",
    "remember",
    "recall",
    "update_profile",
    "list_skills",
    "use_skill",
];

/// Mount the onboarding routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/onboarding/state", get(state))
        .route("/onboarding/personalize", post(personalize))
        .route("/onboarding/complete", post(complete))
}

/// `GET /onboarding/state` response — the few facts the wizard needs to decide
/// whether to auto-open and which steps still need attention.
#[derive(Debug, Serialize)]
pub struct OnboardingState {
    /// Whether the quick-start has been completed (the [`COMPLETED_KEY`] sentinel
    /// is present). The web shell only auto-opens the wizard when this is `false`.
    pub completed: bool,
    /// When it was completed (the sentinel value), if ever.
    pub completed_at: Option<String>,
    /// Whether the caller has an explicit per-user chat-model override (else they
    /// are riding the `[llm]` config default — the "set a model if no default" cue).
    pub chat_model_set: bool,
    /// Whether the profile carries no user-entered fields yet (the sentinel does
    /// not count). A fresh account is `true`.
    pub profile_empty: bool,
}

async fn state(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<OnboardingState>> {
    let p = auth.principal();
    auth.require(Action::Read, "profile")?;
    let profile = state
        .store()
        .profiles()
        .get(p.workspace_id, p.user_id)
        .await?;
    let settings = state
        .store()
        .llm_settings()
        .get(p.workspace_id, p.user_id)
        .await?;
    let completed_at = profile
        .fields
        .get(COMPLETED_KEY)
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let user_fields = profile
        .fields
        .keys()
        .filter(|k| *k != COMPLETED_KEY)
        .count();
    Ok(Json(OnboardingState {
        completed: completed_at.is_some(),
        completed_at,
        chat_model_set: settings.chat_model.is_some(),
        profile_empty: user_fields == 0,
    }))
}

/// One visible turn of the personalization chat, as the client replays it. `role`
/// is `"user"` or `"assistant"`; any other value is treated as a user turn.
#[derive(Debug, Clone, Deserialize)]
pub struct PersonalizeTurn {
    /// `"user"` | `"assistant"`.
    #[serde(default)]
    pub role: String,
    /// The turn's text.
    #[serde(default)]
    pub content: String,
}

/// `POST /onboarding/personalize` body — the visible conversation so far. An empty
/// list asks the assistant to open the conversation (greet + first question); the
/// server holds no chat state, so the client always replays the full exchange.
#[derive(Debug, Default, Deserialize)]
pub struct PersonalizeRequest {
    /// The chat so far, oldest turn first (empty on the very first call).
    #[serde(default)]
    pub messages: Vec<PersonalizeTurn>,
}

/// One proposed skill — the same shape as [`CreateSkill`](super::skills::CreateSkill)
/// minus `code`, so the client can persist a chosen draft straight through
/// `PUT /skills/{name}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillDraft {
    /// Kebab-case, per-workspace-unique skill name (the path/invocation key).
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Markdown runbook.
    pub instructions_md: String,
    /// Advisory tool names the runbook leans on.
    pub tools: Vec<String>,
}

/// `POST /onboarding/personalize` response — one assistant turn plus the durable
/// artifacts it proposes from the exchange so far. Nothing here is persisted; the
/// client reviews the proposals and writes the chosen ones through the normal
/// `POST /memories` / `PUT /skills/{name}` surfaces (SOUL §16).
#[derive(Debug, Serialize)]
pub struct PersonalizeResponse {
    /// The assistant's next message — a greeting + question on the opening turn,
    /// a follow-up question otherwise, or a wrap-up once `done` is set.
    pub reply: String,
    /// Newly learned durable facts (short third-person strings), as memory
    /// candidates. Deduped/persisted client-side.
    pub memories: Vec<String>,
    /// Newly proposed skill drafts (same shape as [`SkillDraft`]), if the exchange
    /// surfaced a concrete reusable workflow.
    pub skills: Vec<SkillDraft>,
    /// The model's own signal that it has learned enough to finish — a cue for the
    /// client to nudge "Finish", never a hard stop (the user drives the chat).
    pub done: bool,
}

/// The model's instruction: run an assistant-led personalization interview and
/// answer each turn with ONLY a JSON object, so the reply, the freshly learned
/// memories, and any skill drafts parse deterministically (the same
/// prompt-for-JSON-then-parse contract the §22 memory extractor uses).
const PERSONALIZE_SYSTEM: &str = "\
You are the onboarding assistant for catalerum, a personal AI workspace. Your job \
is a short, friendly interview that personalizes the workspace around the user. \
Ask about their role, what they want help with, the tools/systems they work in, \
and their working habits and preferences — ONE focused question at a time, warm \
and concise. As you learn things, capture them:\n\
- `memories`: durable third-person facts worth remembering (stable preferences, \
personal details, relationships, goals, recurring habits) — e.g. [\"works in \
Berlin\", \"prefers concise replies\"]. Only include facts you newly learned from \
the user's LATEST message; ignore ephemeral/one-off content; use [] when none.\n\
- `skills`: once you understand a concrete recurring workflow, propose a reusable \
\"skill\" (a named runbook the assistant can follow later). Each is an object \
{\"name\": \"kebab-case-id\", \"description\": \"one sentence\", \"instructions_md\": \
\"a short markdown runbook with concrete numbered steps\", \"tools\": [\"tool-name\", \
...]}. name is short/lowercase/hyphenated; tools is chosen only from the allow-list \
given below (use [] when none apply). Only propose a skill when it is genuinely \
useful, and never re-propose one you already proposed; use [] otherwise.\n\
Respond with ONLY a compact JSON object — no prose, no markdown fences: \
{\"reply\": \"your next message to the user\", \"memories\": [...], \"skills\": \
[...], \"done\": false}. Set `done` to true once you have gathered enough to \
personalize the workspace. `reply` is always present and conversational.";

async fn personalize(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<PersonalizeRequest>,
) -> ApiResult<Json<PersonalizeResponse>> {
    let p = auth.principal();
    auth.require(Action::Write, "skill")?;

    // Effective chat model: the caller's per-user override, else the `[llm]`
    // config default (the same precedence the chat path uses — a settings-read
    // failure degrades to the config default rather than failing the request).
    let model = state
        .store()
        .llm_settings()
        .get(p.workspace_id, p.user_id)
        .await
        .ok()
        .and_then(|s| s.chat_model)
        .unwrap_or_else(|| state.config().llm.default_model.clone());

    // The "About you" step runs before this chat, so the profile usually already
    // carries user-entered fields — feed them to the interviewer so it builds on
    // them instead of re-asking. A read failure degrades to an empty profile.
    let profile_fields = state
        .store()
        .profiles()
        .get(p.workspace_id, p.user_id)
        .await
        .map(|profile| profile.fields)
        .unwrap_or_default();

    let request = ChatRequest::new(
        model,
        build_personalize_messages(&body.messages, &profile_fields),
    );
    let turn = state.llm().chat(request).await?;
    Ok(Json(parse_personalize(&turn.content)))
}

/// Assemble the LLM messages for one personalization turn: the system prompt (with
/// the tool allow-list appended so drafted `tools` stay realistic, and the user's
/// "About you" profile fields appended so the interview builds on them instead of
/// re-asking), then the replayed conversation. An empty history is seeded with a
/// hidden kick-off so the assistant opens the chat; the replay is capped at
/// [`MAX_HISTORY_TURNS`] (§18).
fn build_personalize_messages(
    history: &[PersonalizeTurn],
    profile_fields: &Map,
) -> Vec<ChatMessage> {
    let mut system = format!(
        "{PERSONALIZE_SYSTEM}\n\nWhen choosing skill `tools`, use only this \
         allow-list: [{allow}].",
        allow = TOOL_ALLOWLIST.join(", "),
    );
    if let Some(profile) = render_profile_section(profile_fields) {
        system.push_str("\n\n");
        system.push_str(&profile);
    }
    let mut messages = vec![ChatMessage::system(system)];

    let turns: Vec<&PersonalizeTurn> = history
        .iter()
        .filter(|t| !t.content.trim().is_empty())
        .collect();
    if turns.is_empty() {
        // The opening turn: a hidden instruction (never shown to the user) that
        // makes the assistant greet and ask its first question.
        messages.push(ChatMessage::user(
            "Begin the personalization conversation: greet me in one or two warm \
             sentences and ask your first question.",
        ));
        return messages;
    }

    // Keep only the most recent turns (§18 — bounded prompt, not the whole chat).
    let start = turns.len().saturating_sub(MAX_HISTORY_TURNS);
    for t in &turns[start..] {
        let content = t.content.trim();
        if t.role.eq_ignore_ascii_case("assistant") {
            messages.push(ChatMessage::assistant(content));
        } else {
            messages.push(ChatMessage::user(content));
        }
    }
    messages
}

/// Render the user-entered "About you" profile fields as a system-prompt section
/// (the same `- key: value` shape [`crate::guidance::user_context`] emits), or
/// `None` when the profile carries none. The [`COMPLETED_KEY`] sentinel is not a
/// user-entered fact and is skipped.
fn render_profile_section(fields: &Map) -> Option<String> {
    let mut s = String::from(
        "The user already shared this about themselves in the wizard's \"About \
         you\" step — treat it as known: don't re-ask for it or re-propose it as \
         a memory, but build your questions on it:\n",
    );
    let mut any = false;
    for (key, value) in fields {
        if key == COMPLETED_KEY {
            continue;
        }
        let rendered = match value {
            serde_json::Value::String(v) => v.clone(),
            other => other.to_string(),
        };
        if rendered.trim().is_empty() {
            continue;
        }
        s.push_str(&format!("- {key}: {rendered}\n"));
        any = true;
    }
    any.then_some(s)
}

/// A lenient mirror of [`SkillDraft`] for decoding the model's JSON: every field
/// defaults, and `instructions_md` accepts the `instructions` spelling too, so a
/// slightly-off reply still yields a usable draft rather than dropping the whole
/// batch.
#[derive(Debug, Default, Deserialize)]
struct RawSkillDraft {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, alias = "instructions")]
    instructions_md: String,
    #[serde(default)]
    tools: Vec<String>,
}

/// A lenient mirror of [`PersonalizeResponse`] for decoding the model's per-turn
/// JSON object; every field defaults and `reply` accepts the `message` spelling,
/// so a slightly-off reply still yields a usable turn.
#[derive(Debug, Default, Deserialize)]
struct RawPersonalize {
    #[serde(default, alias = "message")]
    reply: String,
    #[serde(default)]
    memories: Vec<String>,
    #[serde(default)]
    skills: Vec<RawSkillDraft>,
    #[serde(default)]
    done: bool,
}

/// Parse the model's per-turn reply into a normalized [`PersonalizeResponse`].
///
/// Tolerant by design: it locates the first `{ … }` object (ignoring any prose or
/// ```` ```json ```` fences around it) and normalizes each field. A reply with no
/// parseable object degrades to using the whole text as `reply` with no proposals,
/// so the chat keeps working even when the model forgets the JSON envelope.
fn parse_personalize(raw: &str) -> PersonalizeResponse {
    let parsed = extract_json_object(raw)
        .and_then(|obj| serde_json::from_str::<RawPersonalize>(obj).ok())
        .unwrap_or_default();

    // `reply` is required for the chat to progress: fall back to the raw text
    // (when it wasn't a JSON envelope) and finally to a gentle default.
    let reply = {
        let r = parsed.reply.trim();
        if !r.is_empty() {
            r.to_string()
        } else if extract_json_object(raw).is_none() {
            raw.trim().to_string()
        } else {
            String::new()
        }
    };
    let reply = if reply.is_empty() {
        "Tell me a little about yourself and what you'd like help with.".to_string()
    } else {
        reply
    };

    let memories = normalize_memories(parsed.memories);
    let skills = normalize_skill_drafts(parsed.skills);
    PersonalizeResponse {
        reply,
        memories,
        skills,
        done: parsed.done,
    }
}

/// Trim, drop empties, and cap the proposed memory facts at [`MAX_MEMORIES_PER_TURN`]
/// (client-side dedup against already-accepted facts happens on the wizard).
fn normalize_memories(memories: Vec<String>) -> Vec<String> {
    memories
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .take(MAX_MEMORIES_PER_TURN)
        .collect()
}

/// Normalize the proposed skill drafts: drop runbook-less entries, kebab-case +
/// de-duplicate names within the batch, and cap at [`MAX_SKILLS_PER_TURN`].
fn normalize_skill_drafts(raw: Vec<RawSkillDraft>) -> Vec<SkillDraft> {
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<SkillDraft> = Vec::new();
    for raw in raw {
        if out.len() >= MAX_SKILLS_PER_TURN {
            break;
        }
        let instructions_md = raw.instructions_md.trim().to_string();
        let description = raw.description.trim().to_string();
        // A draft needs *some* substance — a name (or description to seed one) and
        // a runbook. Skip empties rather than propose a hollow skill.
        if instructions_md.is_empty() {
            continue;
        }
        let seed = if raw.name.trim().is_empty() {
            &description
        } else {
            &raw.name
        };
        let name = unique_name(&normalize_skill_name(seed), &mut used);
        let tools = normalize_tools(raw.tools);
        out.push(SkillDraft {
            name,
            description,
            instructions_md,
            tools,
        });
    }
    out
}

/// Extract the first top-level `{ … }` JSON object substring from a model reply,
/// tolerating surrounding prose / code fences. Returns `None` when there is no
/// braced span.
fn extract_json_object(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end > start {
        Some(&raw[start..=end])
    } else {
        None
    }
}

/// Kebab-case a free-text name into a safe skill id: lowercase, non-alphanumeric
/// runs collapse to a single `-`, leading/trailing `-` trimmed, capped at 40
/// chars. Falls back to `"skill"` when nothing usable remains.
fn normalize_skill_name(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            s.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
    }
    let trimmed = s.trim_matches('-');
    let capped: String = trimmed.chars().take(40).collect();
    let capped = capped.trim_matches('-').to_string();
    if capped.is_empty() {
        "skill".to_string()
    } else {
        capped
    }
}

/// Disambiguate a name against the names already chosen this batch by appending
/// `-2`, `-3`, … on collision, so two drafts never share a `(workspace, name)` key.
fn unique_name(base: &str, used: &mut std::collections::HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut n = 2u32;
    loop {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// Trim, drop empties, and de-duplicate the drafted tool names (order-preserving)
/// — the same hygiene the skills route applies to a hand-authored tool set.
fn normalize_tools(tools: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    tools
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.clone()))
        .collect()
}

async fn complete(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<OnboardingState>> {
    let p = auth.principal();
    auth.require(Action::Write, "profile")?;
    // Stamp the completion sentinel (merge — never clobbers user fields).
    let mut fields = Map::new();
    fields.insert(
        COMPLETED_KEY.to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    let profile = state
        .store()
        .profiles()
        .merge(p.workspace_id, p.user_id, &fields)
        .await?;
    // A profile write invalidates the sole-user personalization cache (SOUL §29).
    state.bump_personalization(p.workspace_id);
    let completed_at = profile
        .fields
        .get(COMPLETED_KEY)
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let user_fields = profile
        .fields
        .keys()
        .filter(|k| *k != COMPLETED_KEY)
        .count();
    let settings = state
        .store()
        .llm_settings()
        .get(p.workspace_id, p.user_id)
        .await?;
    Ok(Json(OnboardingState {
        completed: completed_at.is_some(),
        completed_at,
        chat_model_set: settings.chat_model.is_some(),
        profile_empty: user_fields == 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn personalize_request_decodes_partial_and_empty() {
        let b: PersonalizeRequest = serde_json::from_str(
            r#"{"messages":[{"role":"assistant","content":"hi"},{"content":"I'm a dev"}]}"#,
        )
        .unwrap();
        assert_eq!(b.messages.len(), 2);
        assert_eq!(b.messages[0].role, "assistant");
        assert_eq!(b.messages[1].role, ""); // absent role decodes empty (→ user)
                                            // An empty body is valid — it drives the opening turn.
        let empty: PersonalizeRequest = serde_json::from_str("{}").unwrap();
        assert!(empty.messages.is_empty());
    }

    #[test]
    fn build_messages_seeds_opening_when_history_empty() {
        let msgs = build_personalize_messages(&[], &Map::new());
        // system + one seeded user kick-off.
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.contains("onboarding assistant"));
        assert!(msgs[0].content.contains("fetch_url")); // allow-list appended
        assert!(msgs[1].content.contains("greet"));
        // Empty profile → no "About you" section in the system prompt.
        assert!(!msgs[0].content.contains("About you"));
    }

    #[test]
    fn build_messages_injects_profile_and_skips_sentinel() {
        let mut fields = Map::new();
        fields.insert("name".into(), serde_json::Value::String("Ada".into()));
        fields.insert(
            "timezone".into(),
            serde_json::json!({"iana": "Europe/Berlin"}),
        );
        fields.insert("role".into(), serde_json::Value::String("  ".into())); // blank → skipped
        fields.insert(
            COMPLETED_KEY.into(),
            serde_json::Value::String("2026-01-01T00:00:00Z".into()),
        );
        let msgs = build_personalize_messages(&[], &fields);
        let system = &msgs[0].content;
        assert!(system.contains("About you"));
        assert!(system.contains("- name: Ada"));
        assert!(system.contains("- timezone: {\"iana\":\"Europe/Berlin\"}")); // non-string → compact JSON
        assert!(!system.contains("- role:")); // blank value skipped
        assert!(!system.contains(COMPLETED_KEY)); // sentinel never leaks

        // A profile of only the sentinel injects nothing.
        let mut only_sentinel = Map::new();
        only_sentinel.insert(
            COMPLETED_KEY.into(),
            serde_json::Value::String("2026-01-01T00:00:00Z".into()),
        );
        assert!(render_profile_section(&only_sentinel).is_none());
    }

    #[test]
    fn build_messages_replays_roles_and_caps_history() {
        let turn = |role: &str, content: &str| PersonalizeTurn {
            role: role.to_string(),
            content: content.to_string(),
        };
        // Blank turns are dropped; roles map through; unknown role → user.
        let history = vec![
            turn("user", "  "),
            turn("assistant", "q1"),
            turn("USER", "a1"),
            turn("weird", "a2"),
        ];
        let msgs = build_personalize_messages(&history, &Map::new());
        // system + 3 non-blank turns.
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1].content, "q1");
        assert_eq!(msgs[2].content, "a1");
        assert_eq!(msgs[3].content, "a2");

        // Over the cap, only the most recent MAX_HISTORY_TURNS survive.
        let long: Vec<PersonalizeTurn> = (0..MAX_HISTORY_TURNS + 5)
            .map(|i| turn("user", &format!("m{i}")))
            .collect();
        let msgs = build_personalize_messages(&long, &Map::new());
        assert_eq!(msgs.len(), MAX_HISTORY_TURNS + 1); // + system
        assert_eq!(msgs[1].content, "m5"); // first 5 dropped
    }

    #[test]
    fn parse_personalize_reads_full_object() {
        let raw = r#"{"reply":"Nice to meet you! What do you work on?",
            "memories":["works in Berlin","  "],
            "skills":[{"name":"Weekly Review","description":"Reflect","instructions_md":"1. List wins","tools":["recall"]}],
            "done":false}"#;
        let out = parse_personalize(raw);
        assert!(out.reply.starts_with("Nice to meet"));
        assert_eq!(out.memories, vec!["works in Berlin".to_string()]); // blank dropped
        assert_eq!(out.skills.len(), 1);
        assert_eq!(out.skills[0].name, "weekly-review");
        assert_eq!(out.skills[0].tools, vec!["recall".to_string()]);
        assert!(!out.done);
    }

    #[test]
    fn parse_personalize_strips_prose_and_fences() {
        let raw = "Sure!\n```json\n{\"reply\":\"hi\",\"done\":true}\n```\nHope that helps.";
        let out = parse_personalize(raw);
        assert_eq!(out.reply, "hi");
        assert!(out.done);
        assert!(out.memories.is_empty());
        assert!(out.skills.is_empty());
    }

    #[test]
    fn parse_personalize_falls_back_to_raw_when_not_json() {
        // No JSON envelope → the whole text becomes the reply, no proposals.
        let out = parse_personalize("What would you like help with?");
        assert_eq!(out.reply, "What would you like help with?");
        assert!(out.memories.is_empty());
        assert!(out.skills.is_empty());
        // Empty reply in a valid object → the gentle default.
        let out = parse_personalize(r#"{"reply":"   ","memories":[]}"#);
        assert!(out.reply.contains("Tell me a little about yourself"));
    }

    #[test]
    fn normalize_memories_caps_and_drops_blanks() {
        let many: Vec<String> = (0..20).map(|i| format!("fact {i}")).collect();
        assert_eq!(normalize_memories(many).len(), MAX_MEMORIES_PER_TURN);
        assert_eq!(
            normalize_memories(vec!["  a  ".into(), "".into(), "b".into()]),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn normalize_skill_name_kebabs_and_caps() {
        assert_eq!(normalize_skill_name("Weekly Review!"), "weekly-review");
        assert_eq!(normalize_skill_name("  Triage   Inbox  "), "triage-inbox");
        assert_eq!(normalize_skill_name("@@@"), "skill");
        assert_eq!(normalize_skill_name(""), "skill");
        let long = normalize_skill_name(&"a".repeat(80));
        assert_eq!(long.chars().count(), 40);
    }

    #[test]
    fn unique_name_disambiguates() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_name("review", &mut used), "review");
        assert_eq!(unique_name("review", &mut used), "review-2");
        assert_eq!(unique_name("review", &mut used), "review-3");
    }

    #[test]
    fn normalize_tools_trims_dedups_drops_empty() {
        let t = normalize_tools(vec![
            " fetch_url ".to_string(),
            "fetch_url".to_string(),
            String::new(),
            "recall".to_string(),
        ]);
        assert_eq!(t, vec!["fetch_url".to_string(), "recall".to_string()]);
    }

    fn raw_draft(name: &str, instructions: &str) -> RawSkillDraft {
        RawSkillDraft {
            name: name.to_string(),
            description: String::new(),
            instructions_md: instructions.to_string(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn normalize_skill_drafts_skips_hollow_and_dedups_and_caps() {
        // Runbook-less entries are dropped; names kebab-case + disambiguate; the
        // batch is capped at MAX_SKILLS_PER_TURN.
        let drafts = normalize_skill_drafts(vec![
            RawSkillDraft {
                name: "empty".into(),
                description: "nothing".into(),
                instructions_md: "  ".into(),
                tools: Vec::new(),
            },
            raw_draft("plan", "x"),
            raw_draft("plan", "y"),
            raw_draft("plan", "z"),
            raw_draft("extra", "w"),
        ]);
        assert_eq!(drafts.len(), MAX_SKILLS_PER_TURN);
        assert_eq!(drafts[0].name, "plan");
        assert_eq!(drafts[1].name, "plan-2");
    }

    #[test]
    fn extract_json_object_finds_braced_span() {
        assert_eq!(extract_json_object("pre {\"a\":1} post"), Some("{\"a\":1}"));
        assert!(extract_json_object("no object").is_none());
    }
}
