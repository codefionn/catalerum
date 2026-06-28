//! The `ask_user` question form (SOUL §7/§12).
//!
//! Renders the questions from a [`StreamUpdate::QuestionsRequested`](crate::api::StreamUpdate)
//! frame as an inline chat form and, on submit, hands the collected
//! [`Answer`]s back through `on_submit`. The [`chat`](crate::components::chat)
//! panel wires that callback to the socket reply channel, which resumes the paused
//! `ask_user` tool server-side.
//!
//! Each question renders per its shape: single-choice → radios, multiple-choice →
//! checkboxes, and a free-text field when the question accepts one (explicitly, or
//! because it offers no options). The form owns one reactive slot per question, so
//! a fresh form instance (keyed on the pending id in `chat`) starts blank.

use leptos::prelude::*;

use crate::api::{Answer, Question};

/// Per-question editable state: the selected option(s) and any typed free text.
#[derive(Clone, Copy)]
struct Slot {
    selected: RwSignal<Vec<String>>,
    text: RwSignal<String>,
}

/// An inline form for one `ask_user` request. `on_submit` fires once, with one
/// [`Answer`] per question (order preserved), when the user confirms. It is an
/// `UnsyncCallback` (not the `Send + Sync` `Callback`) so the chat panel can wire it
/// straight to its `!Send` turn sender.
#[component]
pub fn QuestionForm(
    /// The questions to ask, in order.
    questions: Vec<Question>,
    /// Invoked with the collected answers when the user submits.
    #[prop(into)]
    on_submit: UnsyncCallback<Vec<Answer>>,
) -> impl IntoView {
    // One reactive slot per question, created once for this form instance.
    let slots: Vec<Slot> = questions
        .iter()
        .map(|_| Slot {
            selected: RwSignal::new(Vec::<String>::new()),
            text: RwSignal::new(String::new()),
        })
        .collect();

    // Gather the current state into `Answer`s and hand them off.
    let submit = {
        let questions = questions.clone();
        let slots = slots.clone();
        move |_| {
            let answers = questions
                .iter()
                .zip(slots.iter())
                .map(|(q, slot)| {
                    let typed = slot.text.get();
                    let typed = typed.trim();
                    Answer {
                        id: q.id.clone(),
                        selected: slot.selected.get(),
                        text: (!typed.is_empty()).then(|| typed.to_string()),
                    }
                })
                .collect::<Vec<_>>();
            on_submit.run(answers);
        }
    };

    let rows = questions
        .into_iter()
        .zip(slots)
        .enumerate()
        .map(|(qi, (q, slot))| question_row(qi, q, slot))
        .collect::<Vec<_>>();

    view! {
        <div class="chat-questions">
            <div class="chat-questions-body">{rows}</div>
            <div class="chat-questions-actions">
                <button type="button" class="chat-questions-submit" on:click=submit>
                    "Send answers"
                </button>
            </div>
        </div>
    }
}

/// Render one question: its text, the choice controls (radios / checkboxes), and a
/// free-text field when accepted.
fn question_row(qi: usize, q: Question, slot: Slot) -> impl IntoView {
    let group = format!("catq-{qi}");
    let multiple = q.multiple;
    let accepts_text = q.accepts_text();
    let has_options = !q.options.is_empty();

    // The choice controls, one per option.
    let opts = q
        .options
        .into_iter()
        .map(|opt| {
            let selected = slot.selected;
            let opt_for_checked = opt.clone();
            let checked = move || selected.get().iter().any(|s| s == &opt_for_checked);
            let opt_for_toggle = opt.clone();
            let toggle = move |_| {
                let opt = opt_for_toggle.clone();
                if multiple {
                    // Checkbox: flip membership based on model state.
                    selected.update(|v| match v.iter().position(|x| x == &opt) {
                        Some(pos) => {
                            v.remove(pos);
                        }
                        None => v.push(opt),
                    });
                } else {
                    // Radio: exactly one selection.
                    selected.set(vec![opt]);
                }
            };
            view! {
                <label class="chat-question-opt">
                    <input
                        type=if multiple { "checkbox" } else { "radio" }
                        name=group.clone()
                        prop:checked=checked
                        on:change=toggle
                    />
                    <span>{opt}</span>
                </label>
            }
        })
        .collect::<Vec<_>>();

    // `has_options` / `accepts_text` are fixed for this form instance (the questions
    // never change reactively), so gate the blocks with a plain `Option` rather than
    // a reactive `<Show>` — that also sidesteps cloning non-`Clone` view values.
    let opts_block = has_options.then(|| view! { <div class="chat-question-opts">{opts}</div> });

    // The free-text field, when this question accepts one. Labelled "Other…" when it
    // sits alongside options, else it is the whole answer.
    let text = slot.text;
    let free_text = accepts_text.then(|| {
        let placeholder = if has_options {
            "Other answer…"
        } else {
            "Type your answer…"
        };
        view! {
            <textarea
                class="chat-question-text"
                rows="2"
                placeholder=placeholder
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
            ></textarea>
        }
    });

    view! {
        <div class="chat-question">
            <div class="chat-question-text-label">{q.text}</div>
            {opts_block}
            {free_text}
        </div>
    }
}
