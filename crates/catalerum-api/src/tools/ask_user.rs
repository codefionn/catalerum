//! `ask_user` — blocking mid-turn question forms (SOUL §7).

use super::*;

/// `ask_user` — ask the user one or more questions and pause for their reply
/// (SOUL §7/§12). It does **not** block the turn: it persists the questions as a
/// [`PendingQuestion`](catalerum_core::model::PendingQuestion) tied to the
/// conversation (so the form survives a page reload / socket reconnect) and returns
/// an `awaiting_answer` result — the model should then conclude its turn and wait.
/// The client renders the form; the user's answer arrives as an ordinary follow-up
/// turn, which resolves the pending question. Interactive chat only: a run with no
/// conversation ([`ToolContext::conversation_id`] is `None` — an automation / channel
/// worker) can't surface a form, so the call returns an error the model reads as
/// "ask in your reply instead".
pub(crate) struct AskUserTool {
    pub(crate) pending: PendingQuestionRepo,
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    // Ungated: asking the human a question grants no authority and touches no
    // resource — the human's *answer* is the only effect, and they choose it.
    fn description(&self) -> &str {
        "Ask the user one or more questions and pause for their reply. Each question \
         can offer suggested answers to choose from (set `multiple` for a multi-select, \
         otherwise single-select) and/or accept a typed free-text reply (`allow_text`; \
         always on when a question has no options). Renders as an interactive form in \
         the app that persists across reloads. Use when you need a decision or a \
         missing detail from the user — not for information you could look up yourself. \
         After calling it, STOP and end your turn: do NOT answer on the user's behalf \
         or call more tools — their reply will arrive as their next message. Only works \
         in an interactive chat; if it reports no user is available, just ask in your \
         reply text instead."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "description": "The questions to put to the user, asked together as one form.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Stable key the answer is returned under. Optional — defaults to q1, q2, … by position."
                            },
                            "text": {
                                "type": "string",
                                "description": "The question to ask."
                            },
                            "options": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Suggested answers to choose from. Omit for a pure free-text question."
                            },
                            "multiple": {
                                "type": "boolean",
                                "description": "If true the user may select several options (multi-select); otherwise exactly one (single-select). Default false."
                            },
                            "allow_text": {
                                "type": "boolean",
                                "description": "If true the user may type their own answer in addition to (or instead of) the options. Always effectively true when there are no options. Default false."
                            }
                        },
                        "required": ["text"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let questions = parse_questions(&args)?;
        // Interactive chat only: without a conversation to surface a form on (an
        // automation / channel worker run), degrade to "ask in prose".
        let conversation_id = ctx.conversation_id.ok_or_else(|| {
            Error::invalid(
                "no interactive conversation is available here to ask the user — \
                 ask the user directly in your reply instead",
            )
        })?;
        // At most one pending question per conversation: close any prior unanswered
        // one this same turn opened before recording the new one (superseded, so it
        // carries no answers).
        self.pending
            .resolve_for_conversation(ws, conversation_id, None)
            .await?;
        let pending = self.pending.create(ws, conversation_id, &questions).await?;
        // The turn ends here; the client shows the form (pushed live by the ws
        // handler, and re-fetchable on reload). The user's answer arrives as their
        // next message, which resolves this pending question.
        Ok(json!({
            "status": "awaiting_answer",
            "pending_question_id": pending.id,
            "questions": questions,
            "note": "The question form was shown to the user. Stop and end your turn now; \
                     their answer will arrive as their next message.",
        }))
    }
}

/// Parse the `questions` argument of `ask_user` into typed [`Question`]s: fills a
/// positional `id` default (`q1`, `q2`, …) when one is omitted and normalizes the
/// options (trim / drop-empty). Rejects an empty list or a question with no text.
pub(crate) fn parse_questions(args: &Json) -> Result<Vec<Question>> {
    let raw = args
        .get("questions")
        .and_then(Json::as_array)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| Error::invalid("`questions` must be a non-empty array"))?;
    let mut questions = Vec::with_capacity(raw.len());
    for (i, q) in raw.iter().enumerate() {
        let text = q
            .get("text")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid(format!("question {} has no `text`", i + 1)))?
            .to_string();
        let id = q
            .get("id")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("q{}", i + 1), str::to_string);
        let options = q
            .get("options")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Json::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let multiple = q.get("multiple").and_then(Json::as_bool).unwrap_or(false);
        let allow_text = q.get("allow_text").and_then(Json::as_bool).unwrap_or(false);
        questions.push(Question {
            id,
            text,
            options,
            multiple,
            allow_text,
        });
    }
    Ok(questions)
}
