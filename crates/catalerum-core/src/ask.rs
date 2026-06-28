//! Asking the user questions mid-turn — the transport-agnostic seam behind the
//! `ask_user` tool (SOUL §7, §12, §19).
//!
//! The `ask_user` tool puts one or more [`Question`]s to the human and pauses for
//! their reply. Each question offers model-suggested `options` (single- **or**
//! multiple-choice) and/or a free-text reply — "pick one of these, or tell me in
//! your own words". The *rendering* is the transport's job: the interactive web
//! chat draws a real form (radios / checkboxes / text field), while a plain text
//! channel would degrade to a numbered prompt ([`render_questions_text`]). The
//! user's [`Answer`]s come back as an ordinary follow-up turn.
//!
//! Durability (SOUL §7/§12): the `ask_user` tool does **not** block the turn.
//! It persists the questions as a [`PendingQuestion`](crate::model::PendingQuestion)
//! tied to the conversation and ends the turn; the client renders the form (from a
//! live frame, or by fetching the pending question on reload). The user's answer
//! arrives as an ordinary follow-up turn, which resolves the pending question — so
//! a pending question survives a page reload or a socket reconnect. A run with no
//! conversation context (an automation, a channel worker job) can't surface a form,
//! and the tool reports that the model must ask in prose instead.

use serde::{Deserialize, Serialize};

/// One question to put to the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Stable key the [`Answer`] is returned under (e.g. `"tone"`). The `ask_user`
    /// tool fills a positional default (`q1`, `q2`, …) when the model omits it, so
    /// the model can always correlate answers back to questions.
    pub id: String,
    /// The question text shown to the user.
    pub text: String,
    /// Model-suggested answers to choose from. Empty = a pure free-text question.
    #[serde(default)]
    pub options: Vec<String>,
    /// Multiple-choice: the user may pick **more than one** of `options`. `false`
    /// (the default) is single-choice — pick exactly one.
    #[serde(default)]
    pub multiple: bool,
    /// Whether the user may type their own answer instead of (or as well as)
    /// picking an option. A question with no `options` is always free-text
    /// regardless of this flag (see [`accepts_text`](Self::accepts_text)).
    #[serde(default)]
    pub allow_text: bool,
}

impl Question {
    /// Whether a free-text reply is accepted — either explicitly allowed, or
    /// implied because the question offers no options to pick from.
    #[must_use]
    pub fn accepts_text(&self) -> bool {
        self.allow_text || self.options.is_empty()
    }
}

/// The user's answer to one [`Question`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The [`Question::id`] this answers.
    pub id: String,
    /// The option(s) the user selected (a subset of the question's `options`).
    /// Empty when they answered purely with free text; at most one for a
    /// single-choice question.
    #[serde(default)]
    pub selected: Vec<String>,
    /// The free text the user typed, if any (`None`/absent when they only picked
    /// from `options`).
    #[serde(default)]
    pub text: Option<String>,
}

/// Render `questions` as a plain-text prompt for a channel that can't draw a form:
/// numbered questions, lettered options, and a hint about the choice mode / free
/// text. Kept here so every text-only transport degrades identically; the
/// interactive web asker ignores this and renders a real form.
#[must_use]
pub fn render_questions_text(questions: &[Question]) -> String {
    let multi = questions.len() > 1;
    let mut out = String::new();
    for (qi, q) in questions.iter().enumerate() {
        if qi > 0 {
            out.push_str("\n\n");
        }
        if multi {
            out.push_str(&format!("{}. {}", qi + 1, q.text));
        } else {
            out.push_str(&q.text);
        }
        // A hint about how to answer, only when there are options to choose among.
        if !q.options.is_empty() {
            out.push_str(if q.multiple {
                " (select one or more)"
            } else {
                " (select one)"
            });
        }
        for (oi, opt) in q.options.iter().enumerate() {
            out.push_str(&format!("\n  {}) {}", option_letter(oi), opt));
        }
        // Only worth stating when there were options too — a bare question is
        // free-text by definition.
        if q.accepts_text() && !q.options.is_empty() {
            out.push_str("\n  (or type your own answer)");
        }
    }
    out
}

/// The a/b/c… label for an option index, capped at `z` (a question never realistically
/// carries 26+ options).
fn option_letter(i: usize) -> char {
    (b'a' + (i.min(25) as u8)) as char
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn q(id: &str, text: &str, options: &[&str], multiple: bool, allow_text: bool) -> Question {
        Question {
            id: id.to_string(),
            text: text.to_string(),
            options: options.iter().map(|s| (*s).to_string()).collect(),
            multiple,
            allow_text,
        }
    }

    #[test]
    fn accepts_text_is_implied_by_having_no_options() {
        // No options → always free-text, regardless of the flag.
        assert!(q("a", "why?", &[], false, false).accepts_text());
        // With options, it follows the flag.
        assert!(!q("a", "which?", &["x", "y"], false, false).accepts_text());
        assert!(q("a", "which?", &["x", "y"], false, true).accepts_text());
    }

    #[test]
    fn question_and_answer_json_roundtrip_with_defaults() {
        // A minimal question (only id+text) fills the option/flags defaults.
        let parsed: Question = serde_json::from_value(json!({"id": "q1", "text": "Hi?"})).unwrap();
        assert_eq!(parsed, q("q1", "Hi?", &[], false, false));

        // A minimal answer (only id) fills empty selected + no text.
        let a: Answer = serde_json::from_value(json!({"id": "q1"})).unwrap();
        assert_eq!(a.selected, Vec::<String>::new());
        assert_eq!(a.text, None);

        // Full round-trips are stable.
        let full = q("tone", "Tone?", &["formal", "casual"], true, true);
        assert_eq!(
            serde_json::from_value::<Question>(serde_json::to_value(&full).unwrap()).unwrap(),
            full
        );
    }

    #[test]
    fn single_question_text_has_no_number_and_lettered_options() {
        let text =
            render_questions_text(&[q("t", "Pick a tone", &["formal", "casual"], false, false)]);
        assert!(text.starts_with("Pick a tone (select one)"), "{text}");
        assert!(text.contains("\n  a) formal"));
        assert!(text.contains("\n  b) casual"));
        // No options-omitted free-text hint (allow_text is false and options exist).
        assert!(!text.contains("type your own"));
    }

    #[test]
    fn multi_question_text_numbers_and_marks_mode_and_free_text() {
        let text = render_questions_text(&[
            q("a", "Colours?", &["red", "blue"], true, true),
            q("b", "Your name?", &[], false, false),
        ]);
        assert!(text.contains("1. Colours? (select one or more)"), "{text}");
        assert!(text.contains("\n  a) red"));
        assert!(text.contains("(or type your own answer)"));
        // The second, option-less question is numbered and carries no hint/options.
        assert!(text.contains("2. Your name?"), "{text}");
    }
}
